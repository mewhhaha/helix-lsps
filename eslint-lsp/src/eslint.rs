use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

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
    pub line: u32,
    pub column: u32,
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

        let (cwd, config_format) = discover_cwd(file_dir)?;
        let eslint_package_json = self.resolve_eslint_package_json(&cwd).await?;

        Ok(ProjectContext {
            cwd,
            config_format,
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

struct Worker {
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
        Ok(Self {
            next_request_id: Mutex::new(0),
            process: Mutex::new(spawn_worker_process(&key).await?),
        })
    }

    async fn lint(&self, request: &LintRequest) -> Result<LintResponse> {
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
        {
            let mut process = self.process.lock().await;
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
                .context("failed to read response from eslint worker")?;

            if line.is_empty() {
                return Err(anyhow!("eslint worker exited before replying"));
            }

            if process.child.try_wait()?.is_some() {
                return Err(anyhow!("eslint worker exited unexpectedly"));
            }
        }

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

async fn spawn_worker_process(key: &ProjectKey) -> Result<WorkerProcess> {
    let bridge_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("node")
        .join("eslint-bridge.mjs");

    let mut child = Command::new("node")
        .arg(bridge_path)
        .arg(&key.eslint_package_json)
        .arg(&key.cwd)
        .arg(key.config_format.as_bridge_value())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

fn discover_cwd(start_dir: &Path) -> Result<(PathBuf, ConfigFormat)> {
    let mut fallback_package_dir = None;

    for candidate in start_dir.ancestors() {
        if contains_any(candidate, FLAT_CONFIG_NAMES) {
            return Ok((candidate.to_path_buf(), ConfigFormat::Flat));
        }

        if contains_any(candidate, LEGACY_CONFIG_NAMES)
            || package_json_has_eslint_config(candidate)?
        {
            return Ok((candidate.to_path_buf(), ConfigFormat::Eslintrc));
        }

        if fallback_package_dir.is_none() && candidate.join("package.json").exists() {
            fallback_package_dir = Some(candidate.to_path_buf());
        }
    }

    Ok((
        fallback_package_dir.unwrap_or_else(|| start_dir.to_path_buf()),
        ConfigFormat::Default,
    ))
}

fn contains_any(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| dir.join(name).exists())
}

fn package_json_has_eslint_config(dir: &Path) -> Result<bool> {
    let package_json = dir.join("package.json");
    if !package_json.exists() {
        return Ok(false);
    }

    let raw = fs::read_to_string(&package_json)
        .with_context(|| format!("failed to read {}", package_json.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", package_json.display()))?;

    Ok(value.get("eslintConfig").is_some())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{ConfigFormat, discover_cwd, format_bridge_error};

    #[test]
    fn prefers_nearest_flat_config() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("packages/app/src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("eslint.config.mjs"), "export default [];").unwrap();

        let (cwd, format) = discover_cwd(&nested).unwrap();
        assert_eq!(cwd, root);
        assert_eq!(format, ConfigFormat::Flat);
    }

    #[test]
    fn falls_back_to_package_json_without_explicit_config() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"fixture"}"#).unwrap();

        let (cwd, format) = discover_cwd(&nested).unwrap();
        assert_eq!(cwd, root);
        assert_eq!(format, ConfigFormat::Default);
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

        let (cwd, format) = discover_cwd(&nested).unwrap();
        assert_eq!(cwd, root);
        assert_eq!(format, ConfigFormat::Eslintrc);
    }

    #[test]
    fn formats_rule_api_incompatibility_errors_compactly() {
        let message = format_bridge_error(
            "TypeError: Error while loading rule 'react/display-name': contextOrFilename.getFilename is not a function\n    at some stack frame",
        );

        assert!(message.contains("not compatible with the local ESLint version"));
        assert!(message.contains("@eslint/compat"));
    }
}
