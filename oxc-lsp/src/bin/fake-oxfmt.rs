use std::{
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};
use lsp_server::{ErrorCode, Message, Response};
use serde_json::json;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args != ["--lsp"] {
        return Err(anyhow!("unexpected fake-oxfmt arguments: {args:?}"));
    }

    let label = std::env::var("OXC_FAKE_LABEL").unwrap_or_else(|_| session_label_from_cwd());
    let initialized_state = std::env::var_os("OXC_FAKE_INITIALIZED_STATE").map(PathBuf::from);
    let init_delay_ms = std::env::var("OXC_FAKE_INIT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    while let Some(message) = Message::read(&mut reader)? {
        match message {
            Message::Request(request) => {
                if request.method == "initialize" {
                    if let Some(delay_ms) = init_delay_ms {
                        thread::sleep(Duration::from_millis(delay_ms));
                    }
                    let response = Response::new_ok(
                        request.id,
                        json!({
                            "capabilities": {
                                "documentFormattingProvider": true,
                                "textDocumentSync": 1,
                            },
                            "serverInfo": {
                                "name": format!("fake-oxfmt:{label}"),
                                "version": "0.1.0",
                            },
                        }),
                    );
                    Message::Response(response).write(&mut writer)?;
                    writer.flush()?;
                    continue;
                }

                if request.method == "shutdown" {
                    Message::Response(Response::new_ok(request.id, json!(null))).write(&mut writer)?;
                    writer.flush()?;
                    continue;
                }

                if request.method == "textDocument/formatting" {
                    let uri = request
                        .params
                        .get("textDocument")
                        .and_then(|value| value.get("uri"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    let response = Response::new_ok(
                        request.id,
                        json!([{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 0}
                            },
                            "newText": format!("formatted-session={label};uri={uri}\n"),
                        }]),
                    );
                    Message::Response(response).write(&mut writer)?;
                    writer.flush()?;
                    continue;
                }

                Message::Response(Response::new_err(
                    request.id,
                    ErrorCode::MethodNotFound as i32,
                    format!("fake-oxfmt does not implement {}", request.method),
                ))
                .write(&mut writer)?;
                writer.flush()?;
            }
            Message::Notification(notification) => {
                if notification.method == "exit" {
                    break;
                }

                if notification.method == "initialized" {
                    if let Some(state_path) = &initialized_state {
                        let count = std::fs::read_to_string(state_path)
                            .ok()
                            .and_then(|value| value.trim().parse::<u64>().ok())
                            .unwrap_or(0)
                            + 1;
                        std::fs::write(state_path, count.to_string())?;
                    }
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
