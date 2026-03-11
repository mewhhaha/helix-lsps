use std::{
    collections::VecDeque,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

const RESOLVE_PACKAGE_SCRIPT: &str = r#"
const base = process.argv[1];
const pkg = process.argv[2];
try {
  const resolved = require.resolve(`${pkg}/package.json`, { paths: [base] });
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
    pub lint_command: CommandSpec,
    pub format_command: Option<CommandSpec>,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery;

struct ToolSpec {
    package_name: &'static str,
    executable_name: &'static str,
    args: &'static [&'static str],
}

const OXLINT: ToolSpec = ToolSpec {
    package_name: "oxlint",
    executable_name: "oxlint",
    args: &["--lsp"],
};

const OXFMT: ToolSpec = ToolSpec {
    package_name: "oxfmt",
    executable_name: "oxfmt",
    args: &["--lsp"],
};

impl Discovery {
    pub fn maybe_context_for_uri_path(&self, file_path: &Path) -> Result<Option<ProjectContext>> {
        if let Some(project) = discover_local_project(file_path)? {
            return Ok(Some(project));
        }

        discover_global_fallback(file_path)
    }

    #[cfg(test)]
    pub fn context_for_uri_path(&self, file_path: &Path) -> Result<ProjectContext> {
        self.maybe_context_for_uri_path(file_path)?
            .ok_or_else(|| anyhow!("no oxlint installation is available for {}", file_path.display()))
    }

}

fn discover_local_project(file_path: &Path) -> Result<Option<ProjectContext>> {
    let start_dir = normalize_start_dir(file_path)?;

    for candidate in start_dir.ancestors() {
        if !candidate.join("package.json").exists() {
            continue;
        }

        if let Some(lint_command) = resolve_local_command(candidate, &OXLINT)? {
            return Ok(Some(ProjectContext {
                key: SessionKey::Project(candidate.to_path_buf()),
                root: Some(candidate.to_path_buf()),
                lint_command,
                format_command: resolve_local_command(candidate, &OXFMT)?
                    .or_else(|| resolve_global_command(&OXFMT)),
            }));
        }
    }

    for candidate in start_dir.ancestors() {
        if let Some(lint_command) = resolve_local_command(candidate, &OXLINT)? {
            return Ok(Some(ProjectContext {
                key: SessionKey::Project(candidate.to_path_buf()),
                root: Some(candidate.to_path_buf()),
                lint_command,
                format_command: resolve_local_command(candidate, &OXFMT)?
                    .or_else(|| resolve_global_command(&OXFMT)),
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

    let mut queue = VecDeque::from([start_dir.to_path_buf()]);
    while let Some(candidate) = queue.pop_front() {
        if candidate.join("package.json").is_file() {
            if let Some(lint_command) = resolve_local_command(&candidate, &OXLINT)? {
                return Ok(Some(ProjectContext {
                    key: SessionKey::Project(candidate.clone()),
                    root: Some(candidate.clone()),
                    lint_command,
                    format_command: resolve_local_command(&candidate, &OXFMT)?
                        .or_else(|| resolve_global_command(&OXFMT)),
                }));
            }
        }

        let Ok(entries) = fs::read_dir(&candidate) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || should_skip_descendant(&path) {
                continue;
            }
            queue.push_back(path);
        }
    }

    Ok(None)
}

fn should_scan_descendants(path: &Path) -> bool {
    path.join("package.json").is_file()
}

fn should_skip_descendant(path: &Path) -> bool {
    path.file_name()
        .and_then(|segment| segment.to_str())
        .is_some_and(|segment| segment == "node_modules" || segment.starts_with('.'))
}

fn discover_global_fallback(file_path: &Path) -> Result<Option<ProjectContext>> {
    let _cwd = normalize_start_dir(file_path)?;
    let Some(lint_command) = resolve_global_command(&OXLINT) else {
        return Ok(None);
    };

    Ok(Some(ProjectContext {
        key: SessionKey::Global,
        root: None,
        lint_command,
        format_command: resolve_global_command(&OXFMT),
    }))
}

fn normalize_start_dir(file_path: &Path) -> Result<&Path> {
    if file_path.is_dir() {
        return Ok(file_path);
    }

    file_path.parent().ok_or_else(|| {
        anyhow!(
            "cannot determine parent directory for {}",
            file_path.display()
        )
    })
}

fn resolve_local_command(candidate: &Path, tool: &ToolSpec) -> Result<Option<CommandSpec>> {
    let binary = candidate
        .join("node_modules")
        .join(".bin")
        .join(executable_name(tool.executable_name));
    if binary.exists() {
        return Ok(Some(CommandSpec {
            program: binary,
            args: tool.args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: Some(candidate.to_path_buf()),
        }));
    }

    let package_json = candidate
        .join("node_modules")
        .join(tool.package_name)
        .join("package.json");
    if package_json.exists() {
        return package_command_from_package_json(candidate, package_json, tool);
    }

    let Some(package_json) = resolve_package_json_with_node(candidate, tool.package_name)? else {
        return Ok(None);
    };

    package_command_from_package_json(candidate, package_json, tool)
}

fn package_command_from_package_json(
    candidate: &Path,
    package_json: PathBuf,
    tool: &ToolSpec,
) -> Result<Option<CommandSpec>> {
    let raw = fs::read_to_string(&package_json)
        .with_context(|| format!("failed to read {}", package_json.display()))?;
    let package: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", package_json.display()))?;
    let relative_bin = package
        .get("bin")
        .and_then(|value| match value {
            Value::String(bin) => Some(bin.as_str()),
            Value::Object(map) => map.get(tool.executable_name).and_then(Value::as_str),
            _ => None,
        })
        .ok_or_else(|| {
            anyhow!(
                "package {} does not declare a {} bin",
                package_json.display(),
                tool.executable_name
            )
        })?;

    let binary = package_json
        .parent()
        .expect("package.json always has a parent")
        .join(relative_bin);

    Ok(Some(if is_node_entrypoint(&binary) {
        CommandSpec {
            program: PathBuf::from("node"),
            args: std::iter::once(binary.to_string_lossy().into_owned())
                .chain(tool.args.iter().map(|arg| (*arg).to_owned()))
                .collect(),
            cwd: Some(candidate.to_path_buf()),
        }
    } else {
        CommandSpec {
            program: binary,
            args: tool.args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: Some(candidate.to_path_buf()),
        }
    }))
}

fn resolve_package_json_with_node(candidate: &Path, package_name: &str) -> Result<Option<PathBuf>> {
    let output = match Command::new("node")
        .arg("-e")
        .arg(RESOLVE_PACKAGE_SCRIPT)
        .arg(candidate)
        .arg(package_name)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to run node while resolving {package_name} from {}",
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

fn resolve_global_command(tool: &ToolSpec) -> Option<CommandSpec> {
    find_in_path(executable_name(tool.executable_name)).map(|program| CommandSpec {
        program,
        args: tool.args.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: None,
    })
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
    fn prefers_nearest_package_with_local_oxc_tools() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        let package = workspace.join("packages/app");
        let source = package.join("src");

        fs::create_dir_all(source.clone()).unwrap();
        fs::create_dir_all(package.join("node_modules/.bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"app"}"#).unwrap();
        fs::write(package.join("node_modules/.bin/oxlint"), "#!/bin/sh\n").unwrap();
        fs::write(package.join("node_modules/.bin/oxfmt"), "#!/bin/sh\n").unwrap();

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.root, Some(package.clone()));
        assert!(context.lint_command.program.ends_with("oxlint"));
        assert!(
            context
                .format_command
                .as_ref()
                .is_some_and(|command| command.program.ends_with("oxfmt"))
        );
    }

    #[test]
    fn falls_back_to_global_oxlint_when_no_local_install_exists() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let source = root.join("src");
        let bin_dir = root.join("bin");

        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("oxlint"), "#!/bin/sh\n").unwrap();
        fs::write(bin_dir.join("oxfmt"), "#!/bin/sh\n").unwrap();

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
        assert!(context.lint_command.program.ends_with("oxlint"));
        assert!(
            context
                .format_command
                .as_ref()
                .is_some_and(|command| command.program.ends_with("oxfmt"))
        );
    }

    #[test]
    fn falls_back_to_package_bin_when_shim_is_missing() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let package = root.join("project");
        let source = package.join("src");
        let lint_package = package.join("node_modules/oxlint");

        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(lint_package.join("bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"fixture"}"#).unwrap();
        fs::write(
            lint_package.join("package.json"),
            r#"{"name":"oxlint","bin":{"oxlint":"bin/oxlint.js"}}"#,
        )
        .unwrap();
        fs::write(lint_package.join("bin/oxlint.js"), "console.log('fake');").unwrap();

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.lint_command.program, PathBuf::from("node"));
        assert_eq!(
            context.lint_command.args[0],
            lint_package.join("bin/oxlint.js").to_string_lossy()
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
        fs::write(package.join("node_modules/.bin/oxlint"), "#!/bin/sh\n").unwrap();
        fs::write(package.join("node_modules/.bin/oxfmt"), "#!/bin/sh\n").unwrap();

        let discovery = Discovery;
        let context = discovery.context_for_uri_path(&workspace).unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.root, Some(package));
    }
}
