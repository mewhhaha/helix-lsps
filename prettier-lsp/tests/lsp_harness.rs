use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};
use tempfile::tempdir_in;
use tower_lsp::lsp_types::Url;

#[test]
fn formats_with_workspace_prettier() {
    let workspace_dir = harness_workspace_dir();
    assert!(
        workspace_dir
            .join("node_modules/prettier/package.json")
            .is_file(),
        "install harness dependencies first with `cd harness/workspace && npm install`"
    );

    let scratch_dir = tempdir_in(&workspace_dir).unwrap();
    let file_path = scratch_dir.path().join("example.js");
    let input = "const answer={value:\"forty two\"}\n";
    fs::write(&file_path, input).unwrap();

    let mut harness = LspHarness::spawn();
    let initialize = harness.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": path_to_url(&workspace_dir).to_string(),
            "capabilities": {},
            "clientInfo": { "name": "cargo-test" }
        }),
    );

    assert_eq!(
        initialize["result"]["capabilities"]["documentFormattingProvider"],
        json!(true)
    );

    let file_uri = path_to_url(&file_path).to_string();

    harness.notify("initialized", json!({}));
    harness.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": file_uri,
                "languageId": "javascript",
                "version": 1,
                "text": input
            }
        }),
    );

    let formatting = harness.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": path_to_url(&file_path).to_string() },
            "options": {
                "tabSize": 2,
                "insertSpaces": true
            }
        }),
    );

    assert_eq!(
        formatting["result"],
        json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "newText": "const answer = { value: \"forty two\" };\n"
        }])
    );

    harness.shutdown();
}

fn harness_workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("harness/workspace")
}

fn path_to_url(path: &Path) -> Url {
    Url::from_file_path(path).expect("expected an absolute file path")
}

struct LspHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl LspHarness {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_prettier-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));

        loop {
            let message = self.read_message();
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }));
    }

    fn shutdown(mut self) {
        let response = self.request("shutdown", json!(null));
        assert_eq!(response["result"], json!(null));
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": "exit"
        }));
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "language server exited with {status}");
    }

    fn write_message(&mut self, payload: &Value) {
        let body = payload.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn read_message(&mut self) -> Value {
        let mut content_length = None;

        loop {
            let mut header = String::new();
            self.stdout.read_line(&mut header).unwrap();

            if header == "\r\n" {
                break;
            }

            if let Some(value) = header.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            }
        }

        let mut body = vec![0; content_length.expect("missing content length header")];
        self.stdout.read_exact(&mut body).unwrap();

        serde_json::from_slice(&body).unwrap()
    }
}
