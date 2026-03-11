use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, RwLock},
};

const PRETTIER_BRIDGE: &str = include_str!("prettier_bridge.mjs");

#[async_trait]
pub trait Formatter: Send + Sync {
    async fn format(
        &self,
        file_path: &Path,
        source: &str,
        workspace_root: Option<&Path>,
    ) -> Result<FormatOutcome, FormatError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatOutcome {
    Formatted(String),
    Ignored,
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct FormatError {
    message: String,
}

impl FormatError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FormatError {}

#[derive(Debug)]
pub struct NodePrettierFormatter {
    node_binary: OsString,
    workers: RwLock<HashMap<PathBuf, Arc<WorkspaceWorker>>>,
}

impl NodePrettierFormatter {
    pub fn new(node_binary: impl Into<OsString>) -> Self {
        Self {
            node_binary: node_binary.into(),
            workers: RwLock::new(HashMap::new()),
        }
    }

    async fn worker_for(&self, workspace_dir: &Path) -> Result<Arc<WorkspaceWorker>, FormatError> {
        if let Some(worker) = self.workers.read().await.get(workspace_dir).cloned() {
            return Ok(worker);
        }

        let worker = Arc::new(WorkspaceWorker::spawn(&self.node_binary, workspace_dir)?);
        let mut workers = self.workers.write().await;

        Ok(workers
            .entry(workspace_dir.to_path_buf())
            .or_insert_with(|| worker.clone())
            .clone())
    }

    async fn invalidate_worker(&self, workspace_dir: &Path, worker: &Arc<WorkspaceWorker>) {
        let mut workers = self.workers.write().await;

        if workers
            .get(workspace_dir)
            .is_some_and(|current| Arc::ptr_eq(current, worker))
        {
            workers.remove(workspace_dir);
        }
    }
}

impl Default for NodePrettierFormatter {
    fn default() -> Self {
        Self::new(env::var_os("PRETTIER_LSP_NODE_BINARY").unwrap_or_else(|| "node".into()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum NodeBridgeResponse {
    Formatted { formatted: String },
    Ignored,
    Unsupported,
    Error { code: String, message: String },
}

#[derive(Debug, Serialize)]
struct NodeBridgeRequest<'a> {
    file_path: &'a Path,
    source: &'a str,
    workspace_root: Option<&'a Path>,
}

#[derive(Debug)]
struct WorkspaceWorker {
    state: Mutex<WorkerState>,
}

#[derive(Debug)]
struct WorkerState {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl WorkspaceWorker {
    fn spawn(node_binary: &OsString, workspace_dir: &Path) -> Result<Self, FormatError> {
        let mut child = Command::new(node_binary)
            .current_dir(workspace_dir)
            .arg("--input-type=module")
            .arg("--eval")
            .arg(PRETTIER_BRIDGE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                FormatError::new(format!("failed to spawn {:?}: {error}", node_binary))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| FormatError::new("node worker stdin was not available"))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| FormatError::new("node worker stdout was not available"))?,
        );

        Ok(Self {
            state: Mutex::new(WorkerState {
                _child: child,
                stdin,
                stdout,
            }),
        })
    }

    async fn request(
        &self,
        file_path: &Path,
        source: &str,
        workspace_root: Option<&Path>,
    ) -> Result<NodeBridgeResponse, FormatError> {
        let payload = serde_json::to_string(&NodeBridgeRequest {
            file_path,
            source,
            workspace_root,
        })
        .map_err(|error| FormatError::new(format!("failed to serialize request: {error}")))?;

        let mut state = self.state.lock().await;
        state
            .stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|error| {
                FormatError::new(format!("failed writing request to node worker: {error}"))
            })?;
        state.stdin.write_all(b"\n").await.map_err(|error| {
            FormatError::new(format!("failed terminating node request: {error}"))
        })?;
        state
            .stdin
            .flush()
            .await
            .map_err(|error| FormatError::new(format!("failed flushing node request: {error}")))?;

        let mut line = String::new();
        let bytes_read =
            state.stdout.read_line(&mut line).await.map_err(|error| {
                FormatError::new(format!("failed reading node response: {error}"))
            })?;

        if bytes_read == 0 {
            return Err(FormatError::new("node worker closed before responding"));
        }

        serde_json::from_str::<NodeBridgeResponse>(&line).map_err(|error| {
            FormatError::new(format!("node worker returned invalid JSON: {error}"))
        })
    }
}

#[async_trait]
impl Formatter for NodePrettierFormatter {
    async fn format(
        &self,
        file_path: &Path,
        source: &str,
        workspace_root: Option<&Path>,
    ) -> Result<FormatOutcome, FormatError> {
        let workspace_dir = resolve_workspace_dir(file_path, workspace_root);
        let worker = self.worker_for(&workspace_dir).await?;
        let response = worker.request(file_path, source, workspace_root).await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.invalidate_worker(&workspace_dir, &worker).await;
                return Err(error);
            }
        };

        match response {
            NodeBridgeResponse::Formatted { formatted } => Ok(FormatOutcome::Formatted(formatted)),
            NodeBridgeResponse::Ignored => Ok(FormatOutcome::Ignored),
            NodeBridgeResponse::Unsupported => Ok(FormatOutcome::Unsupported),
            NodeBridgeResponse::Error { code, message } => {
                Err(FormatError::new(format!("{code}: {message}")))
            }
        }
    }
}

fn resolve_workspace_dir(file_path: &Path, workspace_root: Option<&Path>) -> PathBuf {
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));

    if let Some(workspace_root) = workspace_root.filter(|root| parent.starts_with(root)) {
        return parent
            .ancestors()
            .take_while(|path| path.starts_with(workspace_root))
            .find(|path| is_workspace_boundary(path))
            .unwrap_or(workspace_root)
            .to_path_buf();
    }

    parent
        .ancestors()
        .find(|path| is_workspace_boundary(path))
        .unwrap_or(parent)
        .to_path_buf()
}

fn is_workspace_boundary(path: &Path) -> bool {
    path.join("package.json").is_file() || path.join("node_modules").is_dir()
}

#[cfg(test)]
mod tests {
    use super::{Formatter, NodePrettierFormatter, resolve_workspace_dir};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};
    use tempfile::{tempdir, tempdir_in};

    #[test]
    fn finds_nearest_package_boundary() {
        let temp_dir = tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let nested = workspace.join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            workspace.join("package.json"),
            "{\n  \"private\": true\n}\n",
        )
        .unwrap();

        assert_eq!(
            resolve_workspace_dir(&nested.join("example.js"), Some(&workspace)),
            workspace
        );
    }

    #[test]
    fn stops_searching_at_the_initialized_workspace_root() {
        let temp_dir = tempdir().unwrap();
        let outer = temp_dir.path().join("outer");
        let workspace = outer.join("workspace");
        let nested = workspace.join("src/nested");

        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(outer.join("node_modules")).unwrap();
        fs::write(outer.join("package.json"), "{\n  \"private\": true\n}\n").unwrap();

        assert_eq!(
            resolve_workspace_dir(&nested.join("example.js"), Some(&workspace)),
            workspace
        );
    }

    #[tokio::test]
    async fn reuses_a_single_worker_for_repeated_formats() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("harness/workspace");
        assert!(
            workspace
                .join("node_modules/prettier/package.json")
                .is_file()
        );

        let scratch_dir = tempdir_in(&workspace).unwrap();
        let file_path = scratch_dir.path().join("example.js");
        let wrapper_dir = tempdir().unwrap();
        let counter_path = wrapper_dir.path().join("spawn-count.txt");
        let wrapper_path = wrapper_dir.path().join("node-wrapper");
        fs::write(
            &wrapper_path,
            format!(
                "#!/bin/sh\necho spawn >> '{}'\nexec node \"$@\"\n",
                counter_path.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper_path, permissions).unwrap();

        let formatter = NodePrettierFormatter::new(&wrapper_path);
        let source = "const answer={value:\"forty two\"}\n";

        formatter
            .format(&file_path, source, Some(&workspace))
            .await
            .unwrap();
        formatter
            .format(&file_path, source, Some(&workspace))
            .await
            .unwrap();

        let counter = fs::read_to_string(counter_path).unwrap();
        assert_eq!(counter.lines().count(), 1);
    }
}
