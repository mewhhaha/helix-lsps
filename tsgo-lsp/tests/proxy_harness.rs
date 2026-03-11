#![cfg(unix)]

use std::{
    fs,
    io::{BufReader, BufWriter, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use lsp_server::{Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use tempfile::tempdir;
use url::Url;

#[test]
fn routes_open_files_to_each_projects_local_tsgo() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_tsgo = Path::new(env!("CARGO_BIN_EXE_fake-tsgo"));

    let project_a = create_project(root, "project-a", fake_tsgo)?;
    let project_b = create_project(root, "project-b", fake_tsgo)?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_tsgo-lsp")))?;
    let project_a_uri = file_url(&project_a)?;
    let file_a = project_a.join("src/index.ts");
    let file_b = project_b.join("src/index.ts");
    let file_a_uri = file_url(&file_a)?;
    let file_b_uri = file_url(&file_b)?;

    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": project_a_uri,
            "capabilities": {},
            "workspaceFolders": [
                {
                    "uri": project_a_uri,
                    "name": "project-a"
                }
            ]
        }),
    ))?;

    let initialize = harness.expect_response(RequestId::from(1))?;
    let server_name = initialize
        .result
        .as_ref()
        .and_then(|result| result.get("serverInfo"))
        .and_then(|result| result.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(server_name, "fake-tsgo:project-a");

    harness.send(Notification::new("initialized".into(), json!({})))?;

    harness.send(Notification::new(
        "textDocument/didOpen".into(),
        json!({
            "textDocument": {
                "uri": file_a_uri,
                "languageId": "typescript",
                "version": 1,
                "text": "export const a = 1;\n"
            }
        }),
    ))?;
    let diagnostics_a = harness.expect_diagnostics(&file_a_uri)?;
    assert_eq!(
        first_diagnostic_message(&diagnostics_a),
        "session=project-a"
    );

    harness.send(Notification::new(
        "textDocument/didOpen".into(),
        json!({
            "textDocument": {
                "uri": file_b_uri,
                "languageId": "typescript",
                "version": 1,
                "text": "export const b = 2;\n"
            }
        }),
    ))?;
    let diagnostics_b = harness.expect_diagnostics(&file_b_uri)?;
    assert_eq!(
        first_diagnostic_message(&diagnostics_b),
        "session=project-b"
    );

    harness.send(Request::new(
        RequestId::from(2),
        "textDocument/hover".into(),
        json!({
            "textDocument": {"uri": file_b_uri},
            "position": {"line": 0, "character": 0}
        }),
    ))?;
    let hover = harness.expect_response(RequestId::from(2))?;
    let hover_text = hover
        .result
        .as_ref()
        .and_then(|value| value.get("contents"))
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(hover_text.contains("session=project-b"));

    harness.send(Request::new(
        RequestId::from(3),
        "shutdown".into(),
        json!(null),
    ))?;
    harness.expect_response(RequestId::from(3))?;
    harness.send(Notification::new("exit".into(), json!(null)))?;
    harness.wait()?;

    Ok(())
}

#[test]
fn returns_initialize_error_when_no_tsgo_is_available() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let empty_path = root.join("empty-bin");
    fs::create_dir_all(&empty_path)?;

    let mut harness = Harness::spawn_with_env(
        Path::new(env!("CARGO_BIN_EXE_tsgo-lsp")),
        &[("PATH", empty_path.to_string_lossy().into_owned())],
    )?;
    let root_uri = file_url(root)?;

    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {}
        }),
    ))?;

    let response = harness.expect_response(RequestId::from(1))?;
    let error = response.error.expect("initialize should fail");
    assert!(error.message.contains("no global tsgo"));

    Ok(())
}

#[test]
fn initializes_from_workspace_root_using_descendant_project_tsgo() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_tsgo = Path::new(env!("CARGO_BIN_EXE_fake-tsgo"));
    let packages = root.join("packages");

    fs::write(
        root.join("package.json"),
        r#"{"name":"workspace","private":true,"workspaces":["packages/*"]}"#,
    )?;
    let project = create_project(&packages, "project-a", fake_tsgo)?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_tsgo-lsp")))?;
    let root_uri = file_url(root)?;
    let file = project.join("src/index.ts");
    let file_uri = file_url(&file)?;

    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {}
        }),
    ))?;

    let initialize = harness.expect_response(RequestId::from(1))?;
    let server_name = initialize
        .result
        .as_ref()
        .and_then(|result| result.get("serverInfo"))
        .and_then(|result| result.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(server_name, "fake-tsgo:project-a");

    harness.send(Notification::new("initialized".into(), json!({})))?;
    harness.send(Notification::new(
        "textDocument/didOpen".into(),
        json!({
            "textDocument": {
                "uri": file_uri.clone(),
                "languageId": "typescript",
                "version": 1,
                "text": "export const a = 1;\n"
            }
        }),
    ))?;

    let diagnostics = harness.expect_diagnostics(&file_uri)?;
    assert_eq!(first_diagnostic_message(&diagnostics), "session=project-a");

    harness.send(Request::new(
        RequestId::from(2),
        "shutdown".into(),
        json!(null),
    ))?;
    harness.expect_response(RequestId::from(2))?;
    harness.send(Notification::new("exit".into(), json!(null)))?;
    harness.wait()?;

    Ok(())
}

#[test]
fn returns_error_when_secondary_project_init_fails() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_tsgo = Path::new(env!("CARGO_BIN_EXE_fake-tsgo"));

    let project_a = create_project(root, "project-a", fake_tsgo)?;
    let project_b = create_project_with_env(
        root,
        "project-b",
        fake_tsgo,
        &[("TSGO_FAKE_INIT_ERROR", "project-b failed to initialize")],
    )?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_tsgo-lsp")))?;
    let project_a_uri = file_url(&project_a)?;
    let file_b = project_b.join("src/index.ts");
    let file_b_uri = file_url(&file_b)?;

    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": project_a_uri,
            "capabilities": {},
            "workspaceFolders": [{"uri": project_a_uri, "name": "project-a"}]
        }),
    ))?;
    harness.expect_response(RequestId::from(1))?;
    harness.send(Notification::new("initialized".into(), json!({})))?;

    harness.send(Notification::new(
        "textDocument/didOpen".into(),
        json!({
            "textDocument": {
                "uri": file_b_uri,
                "languageId": "typescript",
                "version": 1,
                "text": "export const b = 2;\n"
            }
        }),
    ))?;
    harness.send(Request::new(
        RequestId::from(2),
        "textDocument/hover".into(),
        json!({
            "textDocument": {"uri": file_b_uri},
            "position": {"line": 0, "character": 0}
        }),
    ))?;

    let response = harness.expect_response(RequestId::from(2))?;
    let error = response.error.expect("hover should fail");
    assert!(error.message.contains("project-b failed to initialize"));

    Ok(())
}

#[test]
fn returns_error_when_child_exits_during_request() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_tsgo = Path::new(env!("CARGO_BIN_EXE_fake-tsgo"));
    let project = create_project_with_env(
        root,
        "project-a",
        fake_tsgo,
        &[("TSGO_FAKE_EXIT_ON_HOVER", "1")],
    )?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_tsgo-lsp")))?;
    let project_uri = file_url(&project)?;
    let file = project.join("src/index.ts");
    let file_uri = file_url(&file)?;

    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": project_uri,
            "capabilities": {}
        }),
    ))?;
    harness.expect_response(RequestId::from(1))?;
    harness.send(Notification::new("initialized".into(), json!({})))?;

    harness.send(Request::new(
        RequestId::from(2),
        "textDocument/hover".into(),
        json!({
            "textDocument": {"uri": file_uri},
            "position": {"line": 0, "character": 0}
        }),
    ))?;

    let response = harness.expect_response(RequestId::from(2))?;
    let error = response.error.expect("hover should fail");
    assert!(error.message.contains("tsgo child exited unexpectedly"));

    Ok(())
}

fn create_project(root: &Path, name: &str, fake_tsgo: &Path) -> Result<std::path::PathBuf> {
    create_project_with_env(root, name, fake_tsgo, &[])
}

fn create_project_with_env(
    root: &Path,
    name: &str,
    fake_tsgo: &Path,
    extra_env: &[(&str, &str)],
) -> Result<std::path::PathBuf> {
    let project = root.join(name);
    fs::create_dir_all(project.join("src"))?;
    fs::create_dir_all(project.join("node_modules/.bin"))?;
    fs::write(
        project.join("package.json"),
        format!(r#"{{"name":"{name}","private":true}}"#),
    )?;
    fs::write(
        project.join("src/index.ts"),
        format!("export const name = '{name}';\n"),
    )?;

    let extra_exports = extra_env
        .iter()
        .map(|(key, value)| format!("export {key}=\"{value}\"\n"))
        .collect::<String>();
    let script = format!(
        "#!/bin/sh\nexport TSGO_FAKE_LABEL=\"{name}\"\n{extra_exports}exec \"{}\" \"$@\"\n",
        fake_tsgo.display()
    );
    let script_path = project.join("node_modules/.bin/tsgo");
    fs::write(&script_path, script)?;
    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;

    Ok(project)
}

fn file_url(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow!("failed to convert {} to a file URI", path.display()))
}

fn first_diagnostic_message(response: &Response) -> &str {
    response
        .result
        .as_ref()
        .and_then(|value| value.get("diagnostics"))
        .and_then(Value::as_array)
        .and_then(|diagnostics| diagnostics.first())
        .and_then(|diagnostic| diagnostic.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

struct Harness {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    receiver: mpsc::Receiver<Message>,
}

impl Harness {
    fn spawn(binary: &Path) -> Result<Self> {
        Self::spawn_with_env(binary, &[])
    }

    fn spawn_with_env(binary: &Path, envs: &[(&str, String)]) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in envs {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", binary.display()))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("failed to acquire harness stdin"))?,
        );
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to acquire harness stdout"))?;

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            while let Ok(Some(message)) = Message::read(&mut stdout) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            receiver,
        })
    }

    fn send(&mut self, message: impl Into<Message>) -> Result<()> {
        let message = message.into();
        message.write(&mut self.stdin)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn expect_response(&self, id: RequestId) -> Result<Response> {
        self.recv_matching(|message| match message {
            Message::Response(response) if response.id == id => Some(response),
            _ => None,
        })
    }

    fn expect_diagnostics(&self, uri: &str) -> Result<Response> {
        self.recv_matching(|message| match message {
            Message::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics"
                    && notification.params.get("uri").and_then(Value::as_str) == Some(uri) =>
            {
                Some(Response::new_ok(RequestId::from(0), notification.params))
            }
            _ => None,
        })
    }

    fn recv_matching<T>(&self, mut matcher: impl FnMut(Message) -> Option<T>) -> Result<T> {
        let timeout = Duration::from_secs(5);
        loop {
            let message = self
                .receiver
                .recv_timeout(timeout)
                .context("timed out waiting for LSP message")?;
            if let Some(result) = matcher(message) {
                return Ok(result);
            }
        }
    }

    fn wait(&mut self) -> Result<()> {
        let status = self.child.wait()?;
        if status.success() {
            return Ok(());
        }

        Err(anyhow!("wrapper exited with status {status}"))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            Err(_) => {}
        }
    }
}
