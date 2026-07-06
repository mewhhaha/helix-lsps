use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::Duration,
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, select, tick, unbounded};
use url::Url;

use crate::discovery::SessionKey;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchedFileChange {
    pub session_key: SessionKey,
    pub uri: String,
    pub kind: WatchedFileChangeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchedFileChangeKind {
    Created,
    Changed,
    Deleted,
}

impl WatchedFileChangeKind {
    pub fn lsp_file_change_type(self) -> u8 {
        match self {
            Self::Created => 1,
            Self::Changed => 2,
            Self::Deleted => 3,
        }
    }
}

#[derive(Debug, Default)]
struct WatchedRoots {
    roots: HashMap<SessionKey, WatchedRoot>,
}

pub struct WorkspaceWatcher {
    commands: Sender<WatcherCommand>,
    worker: Option<JoinHandle<()>>,
}

enum WatcherCommand {
    Watch(SessionKey, WatchedRoot),
    Unwatch(SessionKey),
    SetActive(bool),
    Stop,
}

impl WorkspaceWatcher {
    pub fn spawn(interval: Duration) -> Result<(Self, Receiver<Vec<WatchedFileChange>>)> {
        let (commands, command_rx) = unbounded();
        let (changes_tx, changes_rx) = unbounded();
        let worker = thread::Builder::new()
            .name("tsgo-file-watcher".into())
            .spawn(move || run_watcher(interval, command_rx, changes_tx))
            .context("failed to spawn tsgo file watcher")?;

        Ok((
            Self {
                commands,
                worker: Some(worker),
            },
            changes_rx,
        ))
    }

    pub fn watch(&self, session_key: SessionKey, root: PathBuf) {
        let watched_root = WatchedRoot::new(root);
        let _ = self
            .commands
            .send(WatcherCommand::Watch(session_key, watched_root));
    }

    pub fn unwatch(&self, session_key: &SessionKey) {
        let _ = self
            .commands
            .send(WatcherCommand::Unwatch(session_key.clone()));
    }

    pub fn set_active(&self, active: bool) {
        let _ = self.commands.send(WatcherCommand::SetActive(active));
    }
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        let _ = self.commands.send(WatcherCommand::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl WatchedRoots {
    fn watch_root(&mut self, session_key: SessionKey, watched_root: WatchedRoot) {
        let already_watching_root = self
            .roots
            .get(&session_key)
            .is_some_and(|existing| existing.root == watched_root.root);
        if already_watching_root {
            return;
        }

        self.roots.insert(session_key, watched_root);
    }

    fn unwatch(&mut self, session_key: &SessionKey) {
        self.roots.remove(session_key);
    }

    fn scan(&mut self) -> Vec<WatchedFileChange> {
        let mut changes = Vec::new();

        for (session_key, watched_root) in &mut self.roots {
            let current = collect_snapshot(&watched_root.root);
            let mut previous = std::mem::take(&mut watched_root.snapshot);

            for (path, stamp) in &current {
                match previous.remove(path) {
                    Some(previous_stamp) if &previous_stamp == stamp => {}
                    Some(_) => {
                        if let Ok(uri) = file_uri(path) {
                            changes.push(WatchedFileChange {
                                session_key: session_key.clone(),
                                uri,
                                kind: WatchedFileChangeKind::Changed,
                            });
                        }
                    }
                    None => {
                        if let Ok(uri) = file_uri(path) {
                            changes.push(WatchedFileChange {
                                session_key: session_key.clone(),
                                uri,
                                kind: WatchedFileChangeKind::Created,
                            });
                        }
                    }
                }
            }

            for path in previous.into_keys() {
                if let Ok(uri) = file_uri(&path) {
                    changes.push(WatchedFileChange {
                        session_key: session_key.clone(),
                        uri,
                        kind: WatchedFileChangeKind::Deleted,
                    });
                }
            }

            watched_root.snapshot = current;
        }

        changes.sort_by(|left, right| {
            left.uri.cmp(&right.uri).then_with(|| {
                left.kind
                    .lsp_file_change_type()
                    .cmp(&right.kind.lsp_file_change_type())
            })
        });
        changes
    }
}

fn run_watcher(
    interval: Duration,
    commands: Receiver<WatcherCommand>,
    changes_tx: Sender<Vec<WatchedFileChange>>,
) {
    let ticks = tick(interval);
    let mut active = false;
    let mut watched_roots = WatchedRoots::default();

    loop {
        select! {
            recv(commands) -> command => {
                let Ok(command) = command else {
                    break;
                };

                match command {
                    WatcherCommand::Watch(session_key, watched_root) => {
                        watched_roots.watch_root(session_key, watched_root);
                    }
                    WatcherCommand::Unwatch(session_key) => {
                        watched_roots.unwatch(&session_key);
                    }
                    WatcherCommand::SetActive(next_active) => {
                        active = next_active;
                    }
                    WatcherCommand::Stop => break,
                }
            }
            recv(ticks) -> tick => {
                if tick.is_err() || !active {
                    continue;
                }

                let changes = watched_roots.scan();
                if !changes.is_empty() && changes_tx.send(changes).is_err() {
                    break;
                }
            }
        }
    }
}

#[derive(Debug)]
struct WatchedRoot {
    root: PathBuf,
    snapshot: HashMap<PathBuf, FileStamp>,
}

impl WatchedRoot {
    fn new(root: PathBuf) -> Self {
        let snapshot = collect_snapshot(&root);
        Self { root, snapshot }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn collect_snapshot(root: &Path) -> HashMap<PathBuf, FileStamp> {
    let mut snapshot = HashMap::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        let mut entries = entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_dir() {
                if should_descend_into(&path) {
                    pending.push(path);
                }
                continue;
            }

            if !file_type.is_file() || !should_watch_file(&path) {
                continue;
            }

            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            snapshot.insert(
                path,
                FileStamp {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                },
            );
        }
    }

    snapshot
}

fn should_descend_into(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };

    !matches!(
        name,
        ".cache"
            | ".git"
            | ".hg"
            | ".jj"
            | ".next"
            | ".svn"
            | ".turbo"
            | "build"
            | "coverage"
            | "dist"
            | "node_modules"
            | "target"
    )
}

fn should_watch_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("cjs" | "cts" | "js" | "json" | "jsx" | "mjs" | "mts" | "ts" | "tsx")
    )
}

fn file_uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow!("failed to convert {} to a file URI", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{WatchedFileChangeKind, WatchedRoot, WatchedRoots};
    use crate::discovery::SessionKey;

    #[test]
    fn reports_created_changed_and_deleted_files() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let key = SessionKey::Project(root.clone());
        let mut watcher = WatchedRoots::default();

        watcher.watch_root(key.clone(), WatchedRoot::new(root.clone()));
        assert!(watcher.scan().is_empty());

        let file = root.join("src").join("created.ts");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "export const value = 1;\n").unwrap();

        let changes = watcher.scan();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].session_key, key);
        assert_eq!(changes[0].kind, WatchedFileChangeKind::Created);

        fs::write(&file, "export const value = 200;\n").unwrap();
        let changes = watcher.scan();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, WatchedFileChangeKind::Changed);

        fs::remove_file(&file).unwrap();
        let changes = watcher.scan();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, WatchedFileChangeKind::Deleted);
    }

    #[test]
    fn ignores_dependency_directories() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let key = SessionKey::Project(root.clone());
        let mut watcher = WatchedRoots::default();

        watcher.watch_root(key, WatchedRoot::new(root.clone()));
        fs::create_dir_all(root.join("node_modules/package")).unwrap();
        fs::write(root.join("node_modules/package/index.ts"), "export {};\n").unwrap();

        assert!(watcher.scan().is_empty());
    }
}
