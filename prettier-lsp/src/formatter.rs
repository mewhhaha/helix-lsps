use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, RwLock},
    time::timeout,
};

const PRETTIER_BRIDGE: &str = include_str!("prettier_bridge.mjs");

/// Upper bound on how long a single format round-trip may take before the worker
/// is treated as wedged. One hung prettier plugin must not stall the workspace.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

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
    unavailable: bool,
}

impl FormatError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unavailable: false,
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unavailable: true,
        }
    }

    pub fn is_unavailable(&self) -> bool {
        self.unavailable
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
    request_timeout: Duration,
    workers: RwLock<HashMap<PathBuf, Arc<WorkspaceWorker>>>,
}

impl NodePrettierFormatter {
    pub fn new(node_binary: impl Into<OsString>) -> Self {
        Self {
            node_binary: node_binary.into(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            workers: RwLock::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    async fn worker_for(&self, workspace_dir: &Path) -> Result<Arc<WorkspaceWorker>, FormatError> {
        if let Some(worker) = self.workers.read().await.get(workspace_dir).cloned() {
            return Ok(worker);
        }

        let worker = Arc::new(WorkspaceWorker::spawn(
            &self.node_binary,
            workspace_dir,
            self.request_timeout,
        )?);
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
    request_timeout: Duration,
}

#[derive(Debug)]
struct WorkerState {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl WorkspaceWorker {
    fn spawn(
        node_binary: &OsString,
        workspace_dir: &Path,
        request_timeout: Duration,
    ) -> Result<Self, FormatError> {
        let mut child = Command::new(node_binary)
            .current_dir(workspace_dir)
            .arg("--input-type=module")
            .arg("--eval")
            .arg(PRETTIER_BRIDGE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Surface worker crashes on our own stderr, which the LSP client treats
            // as the diagnostics channel; a silenced worker is undebuggable.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    FormatError::unavailable(format!("failed to spawn {:?}: {error}", node_binary))
                } else {
                    FormatError::new(format!("failed to spawn {:?}: {error}", node_binary))
                }
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
                child,
                stdin,
                stdout,
            }),
            request_timeout,
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
        match timeout(self.request_timeout, Self::exchange(&mut state, &payload)).await {
            Ok(result) => result,
            Err(_) => {
                // The worker is wedged. Kill it so the caller's restart path spawns a
                // fresh one instead of blocking every future format on the dead child.
                let _ = state.child.start_kill();
                Err(FormatError::new(format!(
                    "node worker timed out after {:?}",
                    self.request_timeout
                )))
            }
        }
    }

    async fn exchange(
        state: &mut WorkerState,
        payload: &str,
    ) -> Result<NodeBridgeResponse, FormatError> {
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
        let response = match worker.request(file_path, source, workspace_root).await {
            Ok(response) => response,
            Err(first_error) => {
                self.invalidate_worker(&workspace_dir, &worker).await;
                let worker = self.worker_for(&workspace_dir).await?;
                match worker.request(file_path, source, workspace_root).await {
                    Ok(response) => response,
                    Err(second_error) => {
                        self.invalidate_worker(&workspace_dir, &worker).await;
                        return Err(FormatError::new(format!(
                            "node worker failed after restart: {second_error}; original failure: {first_error}"
                        )));
                    }
                }
            }
        };

        map_response(response)
    }
}

fn map_response(response: NodeBridgeResponse) -> Result<FormatOutcome, FormatError> {
    match response {
        NodeBridgeResponse::Formatted { formatted } => Ok(FormatOutcome::Formatted(formatted)),
        NodeBridgeResponse::Ignored => Ok(FormatOutcome::Ignored),
        NodeBridgeResponse::Unsupported => Ok(FormatOutcome::Unsupported),
        NodeBridgeResponse::Error { code, message } if code == "missing_prettier" => {
            Err(FormatError::unavailable(format!("{code}: {message}")))
        }
        NodeBridgeResponse::Error { code, message } => {
            Err(FormatError::new(format!("{code}: {message}")))
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
    use super::{
        Formatter, NodeBridgeResponse, NodePrettierFormatter, map_response, resolve_workspace_dir,
    };
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};
    use tempfile::{TempDir, tempdir, tempdir_in};

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
        let workspace_dir = create_prettier_workspace();
        let workspace = workspace_dir.path();
        let scratch_dir = tempdir_in(workspace).unwrap();
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
            .format(&file_path, source, Some(workspace))
            .await
            .unwrap();
        formatter
            .format(&file_path, source, Some(workspace))
            .await
            .unwrap();

        let counter = fs::read_to_string(counter_path).unwrap();
        assert_eq!(counter.lines().count(), 1);
    }

    #[tokio::test]
    async fn restarts_worker_after_transport_failure() {
        let workspace_dir = create_prettier_workspace();
        let workspace = workspace_dir.path();
        let scratch_dir = tempdir_in(workspace).unwrap();
        let file_path = scratch_dir.path().join("example.js");
        let wrapper_dir = tempdir().unwrap();
        let counter_path = wrapper_dir.path().join("spawn-count.txt");
        let wrapper_path = wrapper_dir.path().join("node-wrapper");
        fs::write(
            &wrapper_path,
            format!(
                "#!/bin/sh\ncount=0\nif [ -f '{0}' ]; then count=$(cat '{0}'); fi\ncount=$((count + 1))\necho \"$count\" > '{0}'\nif [ \"$count\" = \"1\" ]; then exit 0; fi\nexec node \"$@\"\n",
                counter_path.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper_path, permissions).unwrap();

        let formatter = NodePrettierFormatter::new(&wrapper_path);
        let source = "const answer={value:\"forty two\"}\n";
        let outcome = formatter
            .format(&file_path, source, Some(workspace))
            .await
            .unwrap();

        assert!(matches!(outcome, super::FormatOutcome::Formatted(_)));
        let counter = fs::read_to_string(counter_path).unwrap();
        assert_eq!(counter.trim(), "2");
    }

    #[tokio::test]
    async fn times_out_when_worker_never_responds() {
        let workspace_dir = create_prettier_workspace();
        let workspace = workspace_dir.path();
        let scratch_dir = tempdir_in(workspace).unwrap();
        let file_path = scratch_dir.path().join("example.js");

        // A "node" that reads nothing and never replies, so the request round-trip
        // blocks until the timeout fires.
        let wrapper_dir = tempdir().unwrap();
        let wrapper_path = wrapper_dir.path().join("node-wrapper");
        fs::write(&wrapper_path, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut permissions = fs::metadata(&wrapper_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper_path, permissions).unwrap();

        let formatter =
            NodePrettierFormatter::new(&wrapper_path).with_request_timeout(Duration::from_millis(250));
        let error = formatter
            .format(&file_path, "const answer = 42\n", Some(workspace))
            .await
            .unwrap_err();

        assert!(!error.is_unavailable());
        assert!(
            error.to_string().contains("timed out"),
            "unexpected error: {error}"
        );
    }

    fn create_prettier_workspace() -> TempDir {
        let workspace = tempdir().unwrap();
        let prettier_dir = workspace.path().join("node_modules/prettier");
        fs::create_dir_all(&prettier_dir).unwrap();
        fs::write(
            prettier_dir.join("package.json"),
            "{\n  \"name\": \"prettier\",\n  \"version\": \"0.0.0-test\",\n  \"main\": \"index.cjs\"\n}\n",
        )
        .unwrap();
        fs::write(
            prettier_dir.join("index.cjs"),
            "module.exports = {\n  getFileInfo: async () => ({ ignored: false, inferredParser: 'babel' }),\n  resolveConfig: async () => null,\n  format: async (source) => `formatted:${source}`,\n};\n",
        )
        .unwrap();
        workspace
    }

    #[test]
    fn marks_missing_prettier_as_unavailable() {
        let error = map_response(NodeBridgeResponse::Error {
            code: "missing_prettier".into(),
            message: "not found".into(),
        })
        .unwrap_err();

        assert!(error.is_unavailable());
    }

    #[test]
    fn keeps_other_prettier_errors_loud() {
        let error = map_response(NodeBridgeResponse::Error {
            code: "prettier_error".into(),
            message: "boom".into(),
        })
        .unwrap_err();

        assert!(!error.is_unavailable());
        assert_eq!(error.to_string(), "prettier_error: boom");
    }
}
