use std::{
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
};

use anyhow::{Result, anyhow};
use lsp_server::{ErrorCode, Message, Notification, Response};
use serde_json::json;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args != ["--lsp", "--stdio"] {
        return Err(anyhow!("unexpected fake-tsgo arguments: {args:?}"));
    }

    let label = std::env::var("TSGO_FAKE_LABEL").unwrap_or_else(|_| session_label_from_cwd());
    let init_error = std::env::var("TSGO_FAKE_INIT_ERROR").ok();
    let exit_on_hover = std::env::var_os("TSGO_FAKE_EXIT_ON_HOVER").is_some();
    let empty_hover_response = std::env::var_os("TSGO_FAKE_EMPTY_HOVER_RESPONSE").is_some();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    while let Some(message) = Message::read(&mut reader)? {
        match message {
            Message::Request(request) => {
                if request.method == "initialize" {
                    if let Some(message) = &init_error {
                        let response = Response::new_err(
                            request.id,
                            ErrorCode::RequestFailed as i32,
                            message.clone(),
                        );
                        Message::Response(response).write(&mut writer)?;
                        writer.flush()?;
                        continue;
                    }

                    let response = Response::new_ok(
                        request.id,
                        json!({
                            "capabilities": {
                                "hoverProvider": true,
                                "textDocumentSync": 1,
                            },
                            "serverInfo": {
                                "name": format!("fake-tsgo:{label}"),
                                "version": "0.1.0",
                            },
                        }),
                    );
                    Message::Response(response).write(&mut writer)?;
                    writer.flush()?;
                    continue;
                }

                if request.method == "shutdown" {
                    Message::Response(Response::new_ok(request.id, json!(null)))
                        .write(&mut writer)?;
                    writer.flush()?;
                    continue;
                }

                if request.method == "textDocument/hover" {
                    if exit_on_hover {
                        return Ok(());
                    }

                    if empty_hover_response {
                        Message::Response(Response {
                            id: request.id,
                            result: None,
                            error: None,
                        })
                        .write(&mut writer)?;
                        writer.flush()?;
                        continue;
                    }

                    let uri = request
                        .params
                        .get("textDocument")
                        .and_then(|value| value.get("uri"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    let response = Response::new_ok(
                        request.id,
                        json!({
                            "contents": {
                                "kind": "plaintext",
                                "value": format!("session={label};uri={uri}"),
                            }
                        }),
                    );
                    Message::Response(response).write(&mut writer)?;
                    writer.flush()?;
                }
            }
            Message::Notification(notification) => {
                if notification.method == "exit" {
                    break;
                }

                if notification.method == "textDocument/didOpen" {
                    let uri = notification
                        .params
                        .get("textDocument")
                        .and_then(|value| value.get("uri"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    let publish = Notification::new(
                        "textDocument/publishDiagnostics".into(),
                        json!({
                            "uri": uri,
                            "diagnostics": [
                                {
                                    "range": {
                                        "start": {"line": 0, "character": 0},
                                        "end": {"line": 0, "character": 1}
                                    },
                                    "severity": 2,
                                    "source": "fake-tsgo",
                                    "message": format!("session={label}"),
                                }
                            ]
                        }),
                    );
                    Message::Notification(publish).write(&mut writer)?;
                    writer.flush()?;
                }
            }
            Message::Response(_) => {}
        }
    }

    Ok(())
}

fn session_label_from_cwd() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.file_name()
        .and_then(|segment| segment.to_str())
        .unwrap_or("unknown")
        .to_owned()
}
