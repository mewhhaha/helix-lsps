use std::{
    collections::VecDeque,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

const PACKAGE_NAME: &str = "@typescript/native-preview";
const RESOLVE_PACKAGE_SCRIPT: &str = r#"
const base = process.argv[1];
try {
  const resolved = require.resolve("@typescript/native-preview/package.json", { paths: [base] });
  process.stdout.write(resolved);
} catch (error) {
  process.exit(1);
}
"#;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum SessionKey {
    Project(PathBuf),
    Global,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectContext {
    pub key: SessionKey,
    pub root: Option<PathBuf>,
    pub command: CommandSpec,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery;

impl Discovery {
    pub fn context_for_uri_path(&self, file_path: &Path) -> Result<ProjectContext> {
        if let Some(project) = discover_local_project(file_path)? {
            return Ok(project);
        }

        discover_global_fallback(file_path)
    }
}

fn discover_local_project(file_path: &Path) -> Result<Option<ProjectContext>> {
    let start_dir = normalize_start_dir(file_path)?;

    for candidate in start_dir.ancestors() {
        if !candidate.join("package.json").exists() {
            continue;
        }

        if let Some(command) = resolve_local_command(candidate)? {
            return Ok(Some(ProjectContext {
                key: SessionKey::Project(candidate.to_path_buf()),
                root: Some(candidate.to_path_buf()),
                command,
            }));
        }
    }

    for candidate in start_dir.ancestors() {
        if let Some(command) = resolve_local_command(candidate)? {
            return Ok(Some(ProjectContext {
                key: SessionKey::Project(candidate.to_path_buf()),
                root: Some(candidate.to_path_buf()),
                command,
            }));
        }
    }

    if file_path.is_dir() {
        if let Some(project) = discover_descendant_project(start_dir)? {
            return Ok(Some(project));
        }
    }

    Ok(None)
}

fn discover_descendant_project(start_dir: &Path) -> Result<Option<ProjectContext>> {
    if !should_scan_descendants(start_dir) {
        return Ok(None);
    }

    let mut queue = VecDeque::new();
    enqueue_child_directories(start_dir, &mut queue);

    while let Some(candidate) = queue.pop_front() {
        if candidate.join("package.json").exists() {
            if let Some(command) = resolve_local_command(&candidate)? {
                return Ok(Some(ProjectContext {
                    key: SessionKey::Project(candidate.clone()),
                    root: Some(candidate.clone()),
                    command,
                }));
            }
        }

        enqueue_child_directories(&candidate, &mut queue);
    }

    Ok(None)
}

fn should_scan_descendants(start_dir: &Path) -> bool {
    if start_dir.join("pnpm-workspace.yaml").exists() {
        return true;
    }

    let package_json = start_dir.join("package.json");
    let Ok(raw) = fs::read_to_string(package_json) else {
        return false;
    };
    let Ok(package) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };

    package.get("workspaces").is_some()
}

fn enqueue_child_directories(dir: &Path, queue: &mut VecDeque<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut children = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| !matches!(name, ".git" | "node_modules" | "target"))
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    children.sort();
    queue.extend(children);
}

fn discover_global_fallback(file_path: &Path) -> Result<ProjectContext> {
    let cwd = normalize_start_dir(file_path)?;
    let Some(program) = find_in_path(executable_name("tsgo")) else {
        return Err(anyhow!(
            "could not find a local {PACKAGE_NAME} installation for {} and no global tsgo was available on PATH",
            file_path.display()
        ));
    };

    Ok(ProjectContext {
        key: SessionKey::Global,
        root: None,
        command: CommandSpec {
            program,
            args: vec!["--lsp".into(), "--stdio".into()],
            cwd: Some(cwd.to_path_buf()),
        },
    })
}

fn normalize_start_dir(file_path: &Path) -> Result<&Path> {
    if file_path.is_dir() {
        return Ok(file_path);
    }

    file_path.parent().ok_or_else(|| {
        anyhow!(
            "cannot resolve tsgo for a path without a parent directory: {}",
            file_path.display()
        )
    })
}

fn resolve_local_command(candidate: &Path) -> Result<Option<CommandSpec>> {
    let binary = candidate
        .join("node_modules")
        .join(".bin")
        .join(executable_name("tsgo"));
    if binary.exists() {
        return Ok(Some(CommandSpec {
            program: binary,
            args: vec!["--lsp".into(), "--stdio".into()],
            cwd: Some(candidate.to_path_buf()),
        }));
    }

    let package_json = candidate
        .join("node_modules")
        .join(PACKAGE_NAME)
        .join("package.json");
    if package_json.exists() {
        return package_command_from_package_json(candidate, package_json);
    }

    let Some(package_json) = resolve_package_json_with_node(candidate)? else {
        return Ok(None);
    };

    package_command_from_package_json(candidate, package_json)
}

fn package_command_from_package_json(
    candidate: &Path,
    package_json: PathBuf,
) -> Result<Option<CommandSpec>> {
    let raw = fs::read_to_string(&package_json)
        .with_context(|| format!("failed to read {}", package_json.display()))?;
    let package: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", package_json.display()))?;
    let relative_bin = package
        .get("bin")
        .and_then(|value| match value {
            Value::String(bin) => Some(bin.as_str()),
            Value::Object(map) => map.get("tsgo").and_then(Value::as_str),
            _ => None,
        })
        .ok_or_else(|| {
            anyhow!(
                "package {} does not declare a tsgo bin",
                package_json.display()
            )
        })?;

    let binary = package_json
        .parent()
        .expect("package.json always has a parent")
        .join(relative_bin);

    Ok(Some(if is_node_entrypoint(&binary) {
        CommandSpec {
            program: PathBuf::from("node"),
            args: vec![
                binary.to_string_lossy().into_owned(),
                "--lsp".into(),
                "--stdio".into(),
            ],
            cwd: Some(candidate.to_path_buf()),
        }
    } else {
        CommandSpec {
            program: binary,
            args: vec!["--lsp".into(), "--stdio".into()],
            cwd: Some(candidate.to_path_buf()),
        }
    }))
}

fn resolve_package_json_with_node(candidate: &Path) -> Result<Option<PathBuf>> {
    let output = match Command::new("node")
        .arg("-e")
        .arg(RESOLVE_PACKAGE_SCRIPT)
        .arg(candidate)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to run node while resolving {PACKAGE_NAME} from {}",
                    candidate.display()
                )
            });
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let resolved = String::from_utf8(output.stdout)?.trim().to_owned();
    if resolved.is_empty() {
        return Ok(None);
    }

    Ok(Some(PathBuf::from(resolved)))
}

fn is_node_entrypoint(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("js" | "cjs" | "mjs")
    )
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.cmd")
    } else {
        base.to_owned()
    }
}

fn find_in_path(binary_name: String) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;

    env::split_paths(&path)
        .map(|dir| dir.join(&binary_name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use std::{env, fs, path::PathBuf};

    use tempfile::tempdir;

    use super::{Discovery, SessionKey};

    #[test]
    fn prefers_nearest_package_with_local_tsgo() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        let package = workspace.join("packages/app");
        let source = package.join("src");

        fs::create_dir_all(source.clone()).unwrap();
        fs::create_dir_all(package.join("node_modules/.bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"app"}"#).unwrap();
        fs::write(package.join("node_modules/.bin/tsgo"), "#!/bin/sh\n").unwrap();

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.root, Some(package));
    }

    #[test]
    fn falls_back_to_global_tsgo_when_no_local_install_exists() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let source = root.join("src");
        let bin_dir = root.join("bin");

        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("tsgo"), "#!/bin/sh\n").unwrap();

        let old_path = env::var_os("PATH");
        unsafe {
            env::set_var("PATH", &bin_dir);
        }

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        match old_path {
            Some(path) => unsafe { env::set_var("PATH", path) },
            None => unsafe { env::remove_var("PATH") },
        }

        assert_eq!(context.key, SessionKey::Global);
        assert!(context.command.program.ends_with("tsgo"));
    }

    #[test]
    fn falls_back_to_package_bin_when_shim_is_missing() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let package = root.join("project");
        let source = package.join("src");
        let tsgo_package = package.join("node_modules/@typescript/native-preview");

        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(tsgo_package.join("bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"fixture"}"#).unwrap();
        fs::write(
            tsgo_package.join("package.json"),
            r#"{"name":"@typescript/native-preview","bin":{"tsgo":"bin/tsgo.js"}}"#,
        )
        .unwrap();
        fs::write(tsgo_package.join("bin/tsgo.js"), "console.log('fake');").unwrap();

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.command.program, PathBuf::from("node"));
        assert_eq!(
            context.command.args[0],
            tsgo_package.join("bin/tsgo.js").to_string_lossy()
        );
    }

    #[test]
    fn finds_descendant_workspace_project_for_directory_roots() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        let package = workspace.join("packages/app");
        let source = package.join("src");

        fs::create_dir_all(source).unwrap();
        fs::create_dir_all(package.join("node_modules/.bin")).unwrap();
        fs::write(
            workspace.join("package.json"),
            r#"{"name":"workspace","private":true,"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        fs::write(package.join("package.json"), r#"{"name":"app"}"#).unwrap();
        fs::write(package.join("node_modules/.bin/tsgo"), "#!/bin/sh\n").unwrap();

        let discovery = Discovery;
        let context = discovery.context_for_uri_path(&workspace).unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.root, Some(package));
    }
}
