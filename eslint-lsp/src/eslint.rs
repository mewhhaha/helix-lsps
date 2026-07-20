use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use tracing::warn;

/// Node bridge script, embedded so installed binaries do not depend on the
/// build-machine source tree.
const ESLINT_BRIDGE: &str = include_str!("../node/eslint-bridge.mjs");

/// Upper bound on a single lint round-trip. A hung eslint config must not wedge
/// the worker mutex forever; expiry is treated as a restartable transport error.
const WORKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

const FLAT_CONFIG_NAMES: &[&str] = &[
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
];

const LEGACY_CONFIG_NAMES: &[&str] = &[
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yaml",
    ".eslintrc.yml",
];

const RESOLVE_ESLINT_SCRIPT: &str = r#"
const base = process.argv[1];
try {
  const resolved = require.resolve("eslint/package.json", { paths: [base] });
  process.stdout.write(resolved);
} catch (error) {
  process.stderr.write(error && error.message ? error.message : String(error));
  process.exit(1);
}
"#;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConfigFormat {
    Default,
    Flat,
    Eslintrc,
}

impl ConfigFormat {
    pub fn as_bridge_value(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Flat => "flat",
            Self::Eslintrc => "eslintrc",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectContext {
    pub cwd: PathBuf,
    pub config_format: ConfigFormat,
    pub eslint_package_json: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectRoots {
    cwd: PathBuf,
    config_format: ConfigFormat,
    eslint_resolution_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProjectKey {
    cwd: PathBuf,
    config_format: ConfigFormat,
    eslint_package_json: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LintRequest {
    pub file_path: PathBuf,
    pub text: String,
    pub fix: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LintMessage {
    pub rule_id: Option<String>,
    pub severity: u8,
    pub message: String,
    // ESLint legitimately omits these for ignored-file warnings and fatal config
    // errors, so decode must not fail (and kill the healthy worker) when absent.
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LintResponse {
    pub diagnostics: Vec<LintMessage>,
    pub fixed_text: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeRequest<'a> {
    file_path: &'a Path,
    text: &'a str,
    fix: bool,
    id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeEnvelope {
    id: u64,
    ok: bool,
    diagnostics: Option<Vec<LintMessage>>,
    fixed_text: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
pub struct Resolver {
    package_cache: Mutex<HashMap<PathBuf, PathBuf>>,
    worker_cache: Mutex<HashMap<ProjectKey, Arc<Worker>>>,
}

impl Resolver {
    pub async fn lint(&self, request: LintRequest) -> Result<LintResponse> {
        let project = self.resolve_project_context(&request.file_path).await?;
        let worker = self.worker(&project).await?;
        worker.lint(&request).await
    }

    pub async fn resolve_project_context(&self, file_path: &Path) -> Result<ProjectContext> {
        let file_dir = file_path.parent().ok_or_else(|| {
            anyhow!(
                "cannot lint a path without a parent directory: {}",
                file_path.display()
            )
        })?;

        let roots = discover_project_roots(file_dir)?;
        let eslint_package_json = self
            .resolve_eslint_package_json(&roots.eslint_resolution_dir)
            .await?;

        Ok(ProjectContext {
            cwd: roots.cwd,
            config_format: roots.config_format,
            eslint_package_json,
        })
    }

    async fn worker(&self, project: &ProjectContext) -> Result<Arc<Worker>> {
        let key = ProjectKey {
            cwd: project.cwd.clone(),
            config_format: project.config_format,
            eslint_package_json: project.eslint_package_json.clone(),
        };

        {
            let cache = self.worker_cache.lock().await;
            if let Some(worker) = cache.get(&key) {
                return Ok(worker.clone());
            }
        }

        let worker = Arc::new(Worker::spawn(key.clone()).await?);

        let mut cache = self.worker_cache.lock().await;
        Ok(cache.entry(key).or_insert_with(|| worker.clone()).clone())
    }

    async fn resolve_eslint_package_json(&self, base_dir: &Path) -> Result<PathBuf> {
        {
            let cache = self.package_cache.lock().await;
            if let Some(cached) = cache.get(base_dir) {
                return Ok(cached.clone());
            }
        }

        let output = Command::new("node")
            .arg("-e")
            .arg(RESOLVE_ESLINT_SCRIPT)
            .arg(base_dir)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to spawn node while resolving eslint from {}",
                    base_dir.display()
                )
            })?;

        if output.status.success() {
            let resolved = PathBuf::from(String::from_utf8(output.stdout)?.trim());
            let mut cache = self.package_cache.lock().await;
            cache.insert(base_dir.to_path_buf(), resolved.clone());
            return Ok(resolved);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!(
            "could not resolve a local eslint installation from {}: {}",
            base_dir.display(),
            stderr.trim()
        ))
    }
}

pub fn is_tool_unavailable(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("could not resolve a local eslint installation")
        || message.contains("failed to spawn node while resolving eslint")
        || message.contains("failed to spawn the eslint worker process")
}

struct Worker {
    key: ProjectKey,
    node_binary: OsString,
    next_request_id: Mutex<u64>,
    process: Mutex<WorkerProcess>,
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    async fn spawn(key: ProjectKey) -> Result<Self> {
        Self::spawn_with_node(key, "node".into()).await
    }

    async fn spawn_with_node(key: ProjectKey, node_binary: OsString) -> Result<Self> {
        let process = spawn_worker_process(&key, &node_binary)?;

        Ok(Self {
            key,
            node_binary,
            next_request_id: Mutex::new(0),
            process: Mutex::new(process),
        })
    }

    async fn lint(&self, request: &LintRequest) -> Result<LintResponse> {
        let mut process = self.process.lock().await;

        match self.lint_with_process(request, &mut process).await {
            Ok(response) => Ok(response),
            Err(error) if should_restart_worker_after(&error) => {
                let _ = process.child.start_kill();
                let _ = process.child.wait().await;
                let replacement =
                    spawn_worker_process(&self.key, &self.node_binary).with_context(|| {
                        format!("failed to restart eslint worker after request failure: {error}")
                    })?;
                *process = replacement;

                self.lint_with_process(request, &mut process)
                    .await
                    .with_context(|| {
                        format!(
                            "eslint worker request failed after restart; original failure: {error}"
                        )
                    })
            }
            Err(error) => Err(error),
        }
    }

    async fn lint_with_process(
        &self,
        request: &LintRequest,
        process: &mut WorkerProcess,
    ) -> Result<LintResponse> {
        let request_id = {
            let mut next = self.next_request_id.lock().await;
            *next += 1;
            *next
        };

        let payload = serde_json::to_string(&BridgeRequest {
            file_path: &request.file_path,
            text: &request.text,
            fix: request.fix,
            id: request_id,
        })?;

        let mut line = String::new();
        let exchange = async {
            process
                .stdin
                .write_all(payload.as_bytes())
                .await
                .context("failed to write request to eslint worker")?;
            process
                .stdin
                .write_all(b"\n")
                .await
                .context("failed to flush request delimiter to eslint worker")?;
            process
                .stdout
                .read_line(&mut line)
                .await
                .context("failed to read response from eslint worker")
        };

        let bytes_read = match timeout(WORKER_REQUEST_TIMEOUT, exchange).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(anyhow!(
                    "eslint worker timed out while handling the request"
                ));
            }
        };

        // A zero-length read is the only reliable signal that the worker closed
        // stdout; a successfully framed response must not be discarded just
        // because a one-shot child has since exited.
        if bytes_read == 0 {
            return Err(anyhow!("eslint worker exited before replying"));
        }

        // A well-framed response line that fails to decode indicates a bug in the
        // payload, not a broken transport, so this deliberately is not treated as
        // restart-worthy (see should_restart_worker_after).
        let envelope: BridgeEnvelope =
            serde_json::from_str(&line).context("failed to decode eslint worker response")?;

        if envelope.id != request_id {
            return Err(anyhow!(
                "eslint worker response id mismatch: expected {}, got {}",
                request_id,
                envelope.id
            ));
        }

        if envelope.ok {
            return Ok(LintResponse {
                diagnostics: envelope.diagnostics.unwrap_or_default(),
                fixed_text: envelope.fixed_text,
            });
        }

        Err(anyhow!(format_bridge_error(
            envelope.error.as_deref().unwrap_or_default()
        )))
    }
}

fn spawn_worker_process(key: &ProjectKey, node_binary: &OsStr) -> Result<WorkerProcess> {
    let mut child = Command::new(node_binary)
        .arg("--input-type=module")
        .arg("--eval")
        .arg(ESLINT_BRIDGE)
        .arg("eslint-lsp-bridge")
        .arg(&key.eslint_package_json)
        .arg(&key.cwd)
        .arg(key.config_format.as_bridge_value())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherit stderr so worker crashes and config errors stay debuggable
        // instead of being silently swallowed.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn the eslint worker process")?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to acquire eslint worker stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to acquire eslint worker stdout"))?;

    Ok(WorkerProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

fn format_bridge_error(stderr: &str) -> String {
    let summary = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("ESLint failed while evaluating the local project configuration.");

    if stderr.contains("getFilename is not a function")
        || stderr.contains("getPhysicalFilename is not a function")
    {
        return format!(
            "ESLint crashed while loading the local config/plugin: {summary}. This usually means the local plugin stack is not compatible with the local ESLint version. If you are on ESLint 10, update incompatible plugins or wrap them with @eslint/compat."
        );
    }

    format!("ESLint failed while evaluating the local project setup: {summary}")
}

fn should_restart_worker_after(error: &anyhow::Error) -> bool {
    let message = error.to_string();

    message.contains("failed to write request to eslint worker")
        || message.contains("failed to flush request delimiter to eslint worker")
        || message.contains("failed to read response from eslint worker")
        || message.contains("eslint worker exited before replying")
        || message.contains("eslint worker exited unexpectedly")
        || message.contains("eslint worker timed out while handling the request")
        || message.contains("eslint worker response id mismatch")
}

fn discover_project_roots(start_dir: &Path) -> Result<ProjectRoots> {
    let mut nearest_package_dir = None;

    for candidate in start_dir.ancestors() {
        if nearest_package_dir.is_none() && candidate.join("package.json").exists() {
            nearest_package_dir = Some(candidate.to_path_buf());
        }

        if contains_any(candidate, FLAT_CONFIG_NAMES) {
            return Ok(ProjectRoots {
                cwd: candidate.to_path_buf(),
                config_format: ConfigFormat::Flat,
                eslint_resolution_dir: nearest_package_dir
                    .unwrap_or_else(|| candidate.to_path_buf()),
            });
        }

        if contains_any(candidate, LEGACY_CONFIG_NAMES) || package_json_has_eslint_config(candidate)
        {
            return Ok(ProjectRoots {
                cwd: candidate.to_path_buf(),
                config_format: ConfigFormat::Eslintrc,
                eslint_resolution_dir: nearest_package_dir
                    .unwrap_or_else(|| candidate.to_path_buf()),
            });
        }
    }

    let fallback_dir = nearest_package_dir.unwrap_or_else(|| start_dir.to_path_buf());

    Ok(ProjectRoots {
        cwd: fallback_dir.clone(),
        config_format: ConfigFormat::Default,
        eslint_resolution_dir: fallback_dir,
    })
}

fn contains_any(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| dir.join(name).exists())
}

/// Reports whether the directory's package.json carries an inline eslintConfig.
///
/// A missing, unreadable, or unparseable package.json is treated as "no eslint
/// config" (with a warning for the unreadable/unparseable cases) so a single bad
/// file in the ancestor chain does not fail discovery and every subsequent lint.
fn package_json_has_eslint_config(dir: &Path) -> bool {
    let package_json = dir.join("package.json");

    let raw = match fs::read_to_string(&package_json) {
        Ok(raw) => raw,
        Err(error) => {
            if package_json.exists() {
                warn!("failed to read {}: {error}", package_json.display());
            }
            return false;
        }
    };

    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => value.get("eslintConfig").is_some(),
        Err(error) => {
            warn!("failed to parse {}: {error}", package_json.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use anyhow::anyhow;

    use super::{
        ConfigFormat, LintRequest, ProjectKey, Worker, discover_project_roots, format_bridge_error,
        is_tool_unavailable, should_restart_worker_after,
    };

    #[test]
    fn prefers_nearest_flat_config() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("packages/app/src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("eslint.config.mjs"), "export default [];").unwrap();

        let roots = discover_project_roots(&nested).unwrap();
        assert_eq!(roots.cwd, root);
        assert_eq!(roots.config_format, ConfigFormat::Flat);
        assert_eq!(roots.eslint_resolution_dir, root);
    }

    #[test]
    fn falls_back_to_package_json_without_explicit_config() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"fixture"}"#).unwrap();

        let roots = discover_project_roots(&nested).unwrap();
        assert_eq!(roots.cwd, root);
        assert_eq!(roots.config_format, ConfigFormat::Default);
        assert_eq!(roots.eslint_resolution_dir, root);
    }

    #[test]
    fn detects_legacy_config_in_package_json() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"fixture","eslintConfig":{"rules":{"semi":"error"}}}"#,
        )
        .unwrap();

        let roots = discover_project_roots(&nested).unwrap();
        assert_eq!(roots.cwd, root);
        assert_eq!(roots.config_format, ConfigFormat::Eslintrc);
        assert_eq!(roots.eslint_resolution_dir, root);
    }

    #[test]
    fn keeps_root_config_but_resolves_eslint_from_nearest_package() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let package = root.join("packages/app");
        let nested = package.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"workspace","private":true,"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        fs::write(root.join("eslint.config.mjs"), "export default [];").unwrap();
        fs::write(package.join("package.json"), r#"{"name":"app"}"#).unwrap();

        let roots = discover_project_roots(&nested).unwrap();
        assert_eq!(roots.cwd, root);
        assert_eq!(roots.config_format, ConfigFormat::Flat);
        assert_eq!(roots.eslint_resolution_dir, package);
    }

    #[test]
    fn formats_rule_api_incompatibility_errors_compactly() {
        let message = format_bridge_error(
            "TypeError: Error while loading rule 'react/display-name': contextOrFilename.getFilename is not a function\n    at some stack frame",
        );

        assert!(message.contains("not compatible with the local ESLint version"));
        assert!(message.contains("@eslint/compat"));
    }

    #[test]
    fn recognizes_missing_eslint_as_unavailable() {
        assert!(is_tool_unavailable(&anyhow!(
            "could not resolve a local eslint installation from /tmp/project"
        )));
        assert!(is_tool_unavailable(&anyhow!(
            "failed to spawn the eslint worker process"
        )));
    }

    #[test]
    fn restarts_worker_after_transport_failures_only() {
        assert!(should_restart_worker_after(&anyhow!(
            "eslint worker exited before replying"
        )));
        assert!(should_restart_worker_after(&anyhow!(
            "eslint worker timed out while handling the request"
        )));
        // A well-framed response that fails to decode means the payload is wrong,
        // not the transport, so the healthy worker must be kept alive.
        assert!(!should_restart_worker_after(&anyhow!(
            "failed to decode eslint worker response"
        )));
        assert!(!should_restart_worker_after(&anyhow!(
            "ESLint failed while evaluating the local project setup"
        )));
    }

    #[test]
    fn ignores_malformed_package_json_during_discovery() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        // A broken package.json in the ancestor chain must not fail discovery.
        fs::write(root.join("package.json"), "{ this is not json").unwrap();

        let roots = discover_project_roots(&nested).unwrap();
        assert_eq!(roots.cwd, root);
        assert_eq!(roots.config_format, ConfigFormat::Default);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restarts_worker_after_the_child_exits_before_replying() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        let bin_dir = temp.path().join("bin");
        let counter_path = temp.path().join("spawn-count.txt");
        let node_path = bin_dir.join("node");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(
            &node_path,
            format!(
                "#!/bin/sh\ncount=0\nif [ -f '{0}' ]; then count=$(cat '{0}'); fi\ncount=$((count + 1))\necho \"$count\" > '{0}'\nif [ \"$count\" = \"1\" ]; then exit 0; fi\nwhile IFS= read -r line; do\n  printf '%s\\n' '{{\"id\":2,\"ok\":true,\"diagnostics\":[],\"fixedText\":\"fixed text\"}}'\ndone\n",
                counter_path.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&node_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&node_path, permissions).unwrap();

        let worker = Worker::spawn_with_node(
            ProjectKey {
                cwd: project.clone(),
                config_format: ConfigFormat::Default,
                eslint_package_json: project.join("node_modules/eslint/package.json"),
            },
            node_path.into_os_string(),
        )
        .await
        .unwrap();
        let response = worker
            .lint(&LintRequest {
                file_path: project.join("src/index.js"),
                text: "const value = 1;\n".into(),
                fix: true,
            })
            .await
            .unwrap();

        assert_eq!(response.fixed_text.as_deref(), Some("fixed text"));
        let counter = fs::read_to_string(counter_path).unwrap();
        assert_eq!(counter.trim(), "2");
    }
}
