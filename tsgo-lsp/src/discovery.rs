use std::{
    collections::VecDeque,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

const NATIVE_PREVIEW_PACKAGE: &str = "@typescript/native-preview";
const TYPESCRIPT_PACKAGE: &str = "typescript";
// TypeScript 7 is the first `typescript` release built on the native (tsgo)
// compiler; earlier majors ship the JS compiler, which has no `--lsp` mode.
const MINIMUM_TYPESCRIPT_MAJOR: u32 = 7;
const RESOLVE_PACKAGE_SCRIPT: &str = r#"
const base = process.argv[1];
const resolve = (name) => {
  try {
    return require.resolve(`${name}/package.json`, { paths: [base] });
  } catch (error) {
    return null;
  }
};
let resolved = resolve("@typescript/native-preview");
if (!resolved) {
  const typescript = resolve("typescript");
  if (typescript) {
    const { version } = JSON.parse(require("fs").readFileSync(typescript, "utf8"));
    if (parseInt(version, 10) >= 7) {
      resolved = typescript;
    }
  }
}
if (!resolved) {
  process.exit(1);
}
process.stdout.write(resolved);
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
        let project = discover_descendant_project(start_dir)?;
        if let Some(project) = project {
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
            let command = resolve_local_command(&candidate)?;
            if let Some(command) = command {
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
    if is_workspace_root(start_dir) {
        return true;
    }

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

fn is_workspace_root(start_dir: &Path) -> bool {
    [".git", ".svn", ".jj", ".helix"]
        .iter()
        .any(|marker| start_dir.join(marker).exists())
}

fn enqueue_child_directories(dir: &Path, queue: &mut VecDeque<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut children = entries
        .filter_map(|entry| entry.ok())
        // DirEntry::file_type does not follow symlinks; descending through
        // them can loop forever on symlink cycles.
        .filter(|entry| {
            entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false)
        })
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| !matches!(name, "node_modules" | "target") && !name.starts_with('.'))
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
            "could not find a local {NATIVE_PREVIEW_PACKAGE} or {TYPESCRIPT_PACKAGE} (>= {MINIMUM_TYPESCRIPT_MAJOR}) installation for {} and no global tsgo was available on PATH",
            file_path.display()
        ));
    };

    Ok(ProjectContext {
        key: SessionKey::Global,
        root: None,
        command: CommandSpec {
            program,
            args: lsp_args(),
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
            args: lsp_args(),
            cwd: Some(candidate.to_path_buf()),
        }));
    }

    for package_name in [NATIVE_PREVIEW_PACKAGE, TYPESCRIPT_PACKAGE] {
        let package_json = candidate
            .join("node_modules")
            .join(package_name)
            .join("package.json");
        if !package_json.exists() {
            continue;
        }

        if let Some(command) = package_command_from_package_json(candidate, package_json)? {
            return Ok(Some(command));
        }
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
    let package_name = package
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(NATIVE_PREVIEW_PACKAGE);
    if package_name == TYPESCRIPT_PACKAGE && !is_supported_typescript_version(&package) {
        return Ok(None);
    }

    let bin_name = if package_name == TYPESCRIPT_PACKAGE {
        "tsc"
    } else {
        "tsgo"
    };
    let package_dir = package_json
        .parent()
        .ok_or_else(|| anyhow!("package path has no parent: {}", package_json.display()))?;

    // The declared bin is a Node shim that spawns the real compiler as a
    // child, which would be orphaned when we kill the session. Prefer the
    // native binary from the platform package the shim itself would resolve.
    if let Some(native) = find_platform_binary(package_dir, package_name, bin_name) {
        return Ok(Some(CommandSpec {
            program: native,
            args: lsp_args(),
            cwd: Some(candidate.to_path_buf()),
        }));
    }

    let Some(relative_bin) = package.get("bin").and_then(|value| match value {
        Value::String(bin) => Some(bin.as_str()),
        Value::Object(map) => map.get(bin_name).and_then(Value::as_str),
        _ => None,
    }) else {
        return Ok(None);
    };
    // Normalize away the leading `./` that npm bin entries conventionally use.
    let binary = package_dir.join(relative_bin).components().collect::<PathBuf>();

    Ok(Some(if is_node_entrypoint(&binary) {
        CommandSpec {
            program: PathBuf::from("node"),
            args: std::iter::once(binary.to_string_lossy().into_owned())
                .chain(lsp_args())
                .collect(),
            cwd: Some(candidate.to_path_buf()),
        }
    } else {
        CommandSpec {
            program: binary,
            args: lsp_args(),
            cwd: Some(candidate.to_path_buf()),
        }
    }))
}

fn lsp_args() -> Vec<String> {
    vec!["--lsp".into(), "--stdio".into()]
}

fn is_supported_typescript_version(package: &Value) -> bool {
    package
        .get("version")
        .and_then(Value::as_str)
        .and_then(|version| version.split(['.', '-']).next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= MINIMUM_TYPESCRIPT_MAJOR)
}

fn find_platform_binary(package_dir: &Path, package_name: &str, bin_name: &str) -> Option<PathBuf> {
    let platform = node_platform()?;
    let arch = node_arch()?;
    let base_name = package_name.rsplit('/').next()?;

    // Canonicalize so pnpm's symlinked layout resolves to the virtual store,
    // where the platform packages are siblings under the same node_modules.
    let package_dir = fs::canonicalize(package_dir).unwrap_or_else(|_| package_dir.to_path_buf());
    let node_modules = package_dir
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "node_modules"))?;

    let binary = node_modules
        .join("@typescript")
        .join(format!("{base_name}-{platform}-{arch}"))
        .join("lib")
        .join(if cfg!(windows) {
            format!("{bin_name}.exe")
        } else {
            bin_name.to_owned()
        });

    binary.is_file().then_some(binary)
}

fn node_platform() -> Option<&'static str> {
    match env::consts::OS {
        "linux" => Some("linux"),
        "macos" => Some("darwin"),
        "windows" => Some("win32"),
        "freebsd" => Some("freebsd"),
        "netbsd" => Some("netbsd"),
        "openbsd" => Some("openbsd"),
        "aix" => Some("aix"),
        "solaris" | "illumos" => Some("sunos"),
        _ => None,
    }
}

fn node_arch() -> Option<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Some("x64"),
        "aarch64" => Some("arm64"),
        "arm" => Some("arm"),
        "powerpc64" => Some("ppc64"),
        "s390x" => Some("s390x"),
        "riscv64" => Some("riscv64"),
        "loongarch64" => Some("loong64"),
        _ => None,
    }
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
                    "failed to run node while resolving {NATIVE_PREVIEW_PACKAGE} or {TYPESCRIPT_PACKAGE} from {}",
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
    if matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("js" | "cjs" | "mjs")
    ) {
        return true;
    }

    // typescript's `bin/tsc` is an extensionless Node script; detect it by
    // shebang so it is launched via `node` (shebangs don't work on Windows).
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 64];
    let Ok(bytes_read) = file.read(&mut head) else {
        return false;
    };
    let first_line = head[..bytes_read]
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    first_line.starts_with(b"#!") && first_line.windows(4).any(|window| window == b"node")
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

    use super::{Discovery, SessionKey, node_arch, node_platform, resolve_local_command};

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
    fn prefers_typescript_seven_platform_binary_over_bin_shim() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let package = root.join("project");
        let source = package.join("src");
        let typescript = package.join("node_modules/typescript");
        let platform_package = package.join(format!(
            "node_modules/@typescript/typescript-{}-{}",
            node_platform().unwrap(),
            node_arch().unwrap()
        ));

        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(typescript.join("bin")).unwrap();
        fs::create_dir_all(platform_package.join("lib")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"fixture"}"#).unwrap();
        fs::write(
            typescript.join("package.json"),
            r#"{"name":"typescript","version":"7.0.2","bin":{"tsc":"./bin/tsc"}}"#,
        )
        .unwrap();
        fs::write(typescript.join("bin/tsc"), "#!/usr/bin/env node\n").unwrap();
        fs::write(platform_package.join("lib/tsc"), "").unwrap();

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(
            fs::canonicalize(&context.command.program).unwrap(),
            fs::canonicalize(platform_package.join("lib/tsc")).unwrap()
        );
        assert_eq!(context.command.args, vec!["--lsp", "--stdio"]);
    }

    #[test]
    fn falls_back_to_typescript_seven_bin_shim_when_platform_package_is_missing() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let package = root.join("project");
        let source = package.join("src");
        let typescript = package.join("node_modules/typescript");

        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(typescript.join("bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"fixture"}"#).unwrap();
        fs::write(
            typescript.join("package.json"),
            r#"{"name":"typescript","version":"7.1.0-dev.20260710.1","bin":{"tsc":"./bin/tsc"}}"#,
        )
        .unwrap();
        fs::write(
            typescript.join("bin/tsc"),
            "#!/usr/bin/env node\nimport \"../lib/tsc.js\";\n",
        )
        .unwrap();

        let discovery = Discovery;
        let context = discovery
            .context_for_uri_path(&source.join("index.ts"))
            .unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.command.program, PathBuf::from("node"));
        assert_eq!(
            context.command.args,
            vec![
                typescript.join("bin/tsc").to_string_lossy().into_owned(),
                "--lsp".to_owned(),
                "--stdio".to_owned()
            ]
        );
    }

    #[test]
    fn ignores_typescript_installations_older_than_seven() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let package = root.join("project");
        let typescript = package.join("node_modules/typescript");

        fs::create_dir_all(typescript.join("bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"fixture"}"#).unwrap();
        fs::write(
            typescript.join("package.json"),
            r#"{"name":"typescript","version":"5.9.2","bin":{"tsc":"./bin/tsc","tsserver":"./bin/tsserver"}}"#,
        )
        .unwrap();
        fs::write(typescript.join("bin/tsc"), "#!/usr/bin/env node\n").unwrap();

        assert_eq!(resolve_local_command(&package).unwrap(), None);
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

    #[test]
    fn finds_descendant_project_from_git_workspace_root() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let workspace = root.join("workspace");
        let package = workspace.join("apps/app");
        let source = package.join("src");

        fs::create_dir_all(workspace.join(".git")).unwrap();
        fs::create_dir_all(source).unwrap();
        fs::create_dir_all(package.join("node_modules/.bin")).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"app"}"#).unwrap();
        fs::write(package.join("node_modules/.bin/tsgo"), "#!/bin/sh\n").unwrap();

        let discovery = Discovery;
        let context = discovery.context_for_uri_path(&workspace).unwrap();

        assert_eq!(context.key, SessionKey::Project(package.clone()));
        assert_eq!(context.root, Some(package));
    }
}
