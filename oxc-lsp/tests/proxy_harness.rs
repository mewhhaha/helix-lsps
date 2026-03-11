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
fn routes_open_files_to_each_projects_local_oxlint() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_oxlint = Path::new(env!("CARGO_BIN_EXE_fake-oxlint"));
    let fake_oxfmt = Path::new(env!("CARGO_BIN_EXE_fake-oxfmt"));

    let project_a = create_project(root, "project-a", fake_oxlint, fake_oxfmt)?;
    let project_b = create_project(root, "project-b", fake_oxlint, fake_oxfmt)?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_oxc-lsp")))?;
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
    assert_eq!(server_name, "oxc-lsp");
    assert_eq!(
        initialize
            .result
            .as_ref()
            .and_then(|result| result.get("capabilities"))
            .and_then(|caps| caps.get("documentFormattingProvider")),
        Some(&json!(true))
    );

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
        "lint-session=project-a"
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
        "lint-session=project-b"
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
    assert!(hover_text.contains("lint-session=project-b"));

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
fn routes_formatting_to_project_local_oxfmt() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_oxlint = Path::new(env!("CARGO_BIN_EXE_fake-oxlint"));
    let fake_oxfmt = Path::new(env!("CARGO_BIN_EXE_fake-oxfmt"));

    let project = create_project(root, "project-a", fake_oxlint, fake_oxfmt)?;
    let file = project.join("src/index.ts");
    let file_uri = file_url(&file)?;
    let project_uri = file_url(&project)?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_oxc-lsp")))?;
    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": project_uri,
            "capabilities": {},
            "workspaceFolders": [
                {
                    "uri": project_uri,
                    "name": "project-a"
                }
            ]
        }),
    ))?;
    harness.expect_response(RequestId::from(1))?;
    harness.send(Notification::new("initialized".into(), json!({})))?;

    harness.send(Notification::new(
        "textDocument/didOpen".into(),
        json!({
            "textDocument": {
                "uri": file_uri.clone(),
                "languageId": "typescript",
                "version": 1,
                "text": "export const value=1\n"
            }
        }),
    ))?;
    harness.expect_diagnostics(&file_uri)?;

    harness.send(Request::new(
        RequestId::from(2),
        "textDocument/formatting".into(),
        json!({
            "textDocument": {"uri": file_uri.clone()},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    ))?;
    let formatting = harness.expect_response(RequestId::from(2))?;
    let formatted = formatting
        .result
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|edits| edits.first())
        .and_then(|edit| edit.get("newText"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(formatted.contains("formatted-session=project-a"));

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
fn initializes_quietly_when_no_oxc_tooling_is_available() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let root_uri = file_url(root)?;

    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_oxc-lsp")))?;
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
    assert_eq!(initialize.result.unwrap()["capabilities"], json!({}));

    harness.send(Notification::new("initialized".into(), json!({})))?;
    harness.send(Request::new(
        RequestId::from(2),
        "textDocument/formatting".into(),
        json!({
            "textDocument": {"uri": "file:///tmp/example.ts"},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    ))?;
    let formatting = harness.expect_response(RequestId::from(2))?;
    assert_eq!(formatting.result.unwrap(), json!([]));

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
fn slow_formatter_only_receives_initialized_once() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    let fake_oxlint = Path::new(env!("CARGO_BIN_EXE_fake-oxlint"));
    let fake_oxfmt = Path::new(env!("CARGO_BIN_EXE_fake-oxfmt"));
    let state_path = root.join("formatter-initialized-count.txt");
    fs::write(&state_path, "0")?;

    let project = root.join("project-a");
    let bin_dir = project.join("node_modules/.bin");
    let src_dir = project.join("src");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&src_dir)?;
    fs::write(project.join("package.json"), r#"{"name":"project-a"}"#)?;
    write_wrapper(
        &bin_dir.join("oxlint"),
        fake_oxlint,
        &[("OXC_FAKE_LABEL", "project-a")],
    )?;
    write_wrapper(
        &bin_dir.join("oxfmt"),
        fake_oxfmt,
        &[
            ("OXC_FAKE_LABEL", "project-a"),
            ("OXC_FAKE_INIT_DELAY_MS", "250"),
            (
                "OXC_FAKE_INITIALIZED_STATE",
                state_path.to_str().expect("utf-8 path"),
            ),
        ],
    )?;

    let file = project.join("src/index.ts");
    let file_uri = file_url(&file)?;
    let project_uri = file_url(&project)?;
    let mut harness = Harness::spawn(Path::new(env!("CARGO_BIN_EXE_oxc-lsp")))?;

    harness.send(Request::new(
        RequestId::from(1),
        "initialize".into(),
        json!({
            "processId": std::process::id(),
            "rootUri": project_uri,
            "capabilities": {},
            "workspaceFolders": [
                {
                    "uri": project_uri,
                    "name": "project-a"
                }
            ]
        }),
    ))?;
    harness.expect_response(RequestId::from(1))?;
    harness.send(Notification::new("initialized".into(), json!({})))?;
    harness.send(Notification::new(
        "textDocument/didOpen".into(),
        json!({
            "textDocument": {
                "uri": file_uri.clone(),
                "languageId": "typescript",
                "version": 1,
                "text": "export const value=1\n"
            }
        }),
    ))?;
    harness.expect_diagnostics(&file_uri)?;
    harness.send(Request::new(
        RequestId::from(2),
        "textDocument/formatting".into(),
        json!({
            "textDocument": {"uri": file_uri},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    ))?;
    harness.expect_response(RequestId::from(2))?;

    assert_eq!(fs::read_to_string(state_path)?.trim(), "1");

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

fn create_project(root: &Path, name: &str, fake_oxlint: &Path, fake_oxfmt: &Path) -> Result<std::path::PathBuf> {
    let project = root.join(name);
    let bin_dir = project.join("node_modules/.bin");
    let src_dir = project.join("src");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&src_dir)?;
    fs::write(project.join("package.json"), format!(r#"{{"name":"{name}"}}"#))?;
    write_wrapper(
        &bin_dir.join("oxlint"),
        fake_oxlint,
        &[("OXC_FAKE_LABEL", name)],
    )?;
    write_wrapper(
        &bin_dir.join("oxfmt"),
        fake_oxfmt,
        &[("OXC_FAKE_LABEL", name)],
    )?;
    Ok(project)
}

fn write_wrapper(path: &Path, target: &Path, envs: &[(&str, &str)]) -> Result<()> {
    let mut script = String::from("#!/bin/sh\n");
    for (key, value) in envs {
        script.push_str(&format!("export {key}='{}'\n", value.replace('\'', "'\"'\"'")));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", target.display()));
    fs::write(path, script)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn file_url(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow!("failed to convert {} to file URL", path.display()))
}

fn first_diagnostic_message(notification: &Notification) -> &str {
    notification
        .params
        .get("diagnostics")
        .and_then(Value::as_array)
        .and_then(|diagnostics| diagnostics.first())
        .and_then(|diagnostic| diagnostic.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

struct Harness {
    child: Child,
    stdin: ChildStdin,
    receiver: mpsc::Receiver<Message>,
}

impl Harness {
    fn spawn(binary: &Path) -> Result<Self> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn {}", binary.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to acquire stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to acquire stdout"))?;

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(message)) = Message::read(&mut reader) {
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
        let mut writer = BufWriter::new(&mut self.stdin);
        message.into().write(&mut writer)?;
        writer.flush()?;
        Ok(())
    }

    fn expect_response(&self, id: RequestId) -> Result<Response> {
        loop {
            let message = self
                .receiver
                .recv_timeout(Duration::from_secs(20))
                .context("timed out waiting for response")?;
            if let Message::Response(response) = message {
                if response.id == id {
                    return Ok(response);
                }
            }
        }
    }

    fn expect_diagnostics(&self, uri: &str) -> Result<Notification> {
        loop {
            let message = self
                .receiver
                .recv_timeout(Duration::from_secs(20))
                .context("timed out waiting for diagnostics")?;
            if let Message::Notification(notification) = message {
                if notification.method == "textDocument/publishDiagnostics"
                    && notification
                        .params
                        .get("uri")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value == uri)
                {
                    return Ok(notification);
                }
            }
        }
    }

    fn wait(&mut self) -> Result<()> {
        let status = self.child.wait()?;
        if !status.success() {
            return Err(anyhow!("process exited with {status}"));
        }
        Ok(())
    }
}
