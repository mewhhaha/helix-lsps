use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};
use tempfile::{tempdir, tempdir_in};
use tower_lsp::lsp_types::Url;

#[test]
fn formats_with_workspace_prettier() {
    let fixture = tempdir().unwrap();
    let workspace_dir = fixture.path().join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    install_fake_prettier(&workspace_dir, "workspace prettier\n");

    let scratch_dir = tempdir_in(&workspace_dir).unwrap();
    let file_path = scratch_dir.path().join("example.js");
    let input = "const answer={value:\"forty two\"}\n";
    fs::write(&file_path, input).unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_dir);
    open_document(&mut harness, &file_path, input);

    let formatting = format_document(&mut harness, &file_path);

    assert_eq!(
        formatting["result"],
        json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "newText": "workspace prettier\n"
        }])
    );

    harness.shutdown();
}

#[test]
fn formats_with_package_local_prettier_inside_monorepo() {
    let fixture = tempdir().unwrap();
    let workspace_dir = fixture.path().join("workspace");
    let root_src = workspace_dir.join("src");
    let package_dir = workspace_dir.join("packages/app");
    let package_src = package_dir.join("src");
    fs::create_dir_all(&root_src).unwrap();
    fs::create_dir_all(&package_src).unwrap();
    fs::write(
        workspace_dir.join("package.json"),
        "{\n  \"private\": true,\n  \"workspaces\": [\"packages/*\"]\n}\n",
    )
    .unwrap();
    fs::write(
        package_dir.join("package.json"),
        "{\n  \"name\": \"app\"\n}\n",
    )
    .unwrap();
    install_fake_prettier(&workspace_dir, "root prettier\n");
    install_fake_prettier(&package_dir, "package prettier\n");

    let root_file = root_src.join("root.js");
    let package_file = package_src.join("app.js");
    let input = "const answer = 42\n";
    fs::write(&root_file, input).unwrap();
    fs::write(&package_file, input).unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_dir);
    open_document(&mut harness, &root_file, input);
    open_document(&mut harness, &package_file, input);

    assert_eq!(
        format_document(&mut harness, &root_file)["result"],
        json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "newText": "root prettier\n"
        }])
    );
    assert_eq!(
        format_document(&mut harness, &package_file)["result"],
        json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "newText": "package prettier\n"
        }])
    );

    harness.shutdown();
}

#[test]
fn picks_up_workspace_folder_changes_after_initialize() {
    let fixture = tempdir().unwrap();
    let outer_dir = fixture.path().join("outer");
    let workspace_a = outer_dir.join("workspace-a");
    let workspace_b = outer_dir.join("workspace-b");
    let source_dir = workspace_b.join("src");

    fs::create_dir_all(&workspace_a).unwrap();
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        outer_dir.join("package.json"),
        "{\n  \"private\": true\n}\n",
    )
    .unwrap();
    install_fake_prettier(&outer_dir, "outer prettier\n");

    let file_path = source_dir.join("example.js");
    let input = "const answer = 42\n";
    fs::write(&file_path, input).unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_a);
    open_document(&mut harness, &file_path, input);

    assert_eq!(
        format_document(&mut harness, &file_path)["result"],
        json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "newText": "outer prettier\n"
        }])
    );

    change_workspace_folders(&mut harness, vec![&workspace_b], Vec::new());

    let formatting = format_document(&mut harness, &file_path);
    assert_eq!(formatting["result"], json!([]));

    harness.shutdown();
}

#[test]
fn does_not_escape_initialized_workspace_when_resolving_prettier() {
    let fixture = tempdir().unwrap();
    let outer_dir = fixture.path().join("outer");
    let workspace_dir = outer_dir.join("workspace");
    let source_dir = workspace_dir.join("src");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        outer_dir.join("package.json"),
        "{\n  \"private\": true\n}\n",
    )
    .unwrap();
    install_fake_prettier(&outer_dir, "outer prettier\n");

    let file_path = source_dir.join("example.js");
    let input = "const answer = 42\n";
    fs::write(&file_path, input).unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_dir);
    open_document(&mut harness, &file_path, input);

    let formatting = format_document(&mut harness, &file_path);
    assert_eq!(formatting["result"], json!([]));

    harness.shutdown();
}

#[test]
fn ignores_stale_did_change_with_older_version() {
    let fixture = tempdir().unwrap();
    let workspace_dir = fixture.path().join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    install_echoing_prettier(&workspace_dir);

    let file_path = workspace_dir.join("example.js");
    fs::write(&file_path, "v1\n").unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_dir);
    open_document(&mut harness, &file_path, "v1\n");
    change_document(&mut harness, &file_path, 3, "v3\n");
    // Arrives late and behind the version we already hold, so it must be dropped.
    change_document(&mut harness, &file_path, 2, "stale\n");

    let formatting = format_document(&mut harness, &file_path);
    assert_eq!(formatting["result"][0]["newText"], json!("formatted:v3\n"));

    harness.shutdown();
}

#[test]
fn declines_to_format_unopened_document() {
    let fixture = tempdir().unwrap();
    let workspace_dir = fixture.path().join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    install_fake_prettier(&workspace_dir, "workspace prettier\n");

    let file_path = workspace_dir.join("example.js");
    fs::write(&file_path, "const answer = 42\n").unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_dir);

    // No didOpen: the server must not format against the on-disk copy.
    let formatting = format_document(&mut harness, &file_path);
    assert_eq!(formatting["result"], json!(null));

    harness.shutdown();
}

#[test]
fn honors_prettierignore_when_present() {
    let fixture = tempdir().unwrap();
    let workspace_dir = fixture.path().join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    install_ignore_aware_prettier(&workspace_dir);
    fs::write(workspace_dir.join(".prettierignore"), "example.js\n").unwrap();

    let file_path = workspace_dir.join("example.js");
    let input = "const answer = 42\n";
    fs::write(&file_path, input).unwrap();

    let mut harness = LspHarness::spawn();
    initialize_workspace(&mut harness, &workspace_dir);
    open_document(&mut harness, &file_path, input);

    // The ignore-aware fake reports the file as ignored only when the bridge hands
    // it an ignorePath, so an empty result proves .prettierignore was located.
    let formatting = format_document(&mut harness, &file_path);
    assert_eq!(formatting["result"], json!([]));

    harness.shutdown();
}

fn install_fake_prettier(dir: &Path, formatted: &str) {
    let prettier_dir = dir.join("node_modules/prettier");
    fs::create_dir_all(&prettier_dir).unwrap();
    fs::write(
        prettier_dir.join("package.json"),
        "{\n  \"name\": \"prettier\",\n  \"version\": \"0.0.0-harness\",\n  \"main\": \"index.cjs\"\n}\n",
    )
    .unwrap();
    fs::write(
        prettier_dir.join("index.cjs"),
        format!(
            "module.exports = {{\n  getFileInfo: async () => ({{ ignored: false, inferredParser: 'babel' }}),\n  resolveConfig: async () => null,\n  format: async () => {},\n}};\n",
            serde_json::to_string(formatted).unwrap()
        ),
    )
    .unwrap();
}

fn install_echoing_prettier(dir: &Path) {
    let prettier_dir = dir.join("node_modules/prettier");
    fs::create_dir_all(&prettier_dir).unwrap();
    fs::write(
        prettier_dir.join("package.json"),
        "{\n  \"name\": \"prettier\",\n  \"version\": \"0.0.0-harness\",\n  \"main\": \"index.cjs\"\n}\n",
    )
    .unwrap();
    fs::write(
        prettier_dir.join("index.cjs"),
        "module.exports = {\n  getFileInfo: async () => ({ ignored: false, inferredParser: 'babel' }),\n  resolveConfig: async () => null,\n  format: async (source) => `formatted:${source}`,\n};\n",
    )
    .unwrap();
}

fn install_ignore_aware_prettier(dir: &Path) {
    let prettier_dir = dir.join("node_modules/prettier");
    fs::create_dir_all(&prettier_dir).unwrap();
    fs::write(
        prettier_dir.join("package.json"),
        "{\n  \"name\": \"prettier\",\n  \"version\": \"0.0.0-harness\",\n  \"main\": \"index.cjs\"\n}\n",
    )
    .unwrap();
    fs::write(
        prettier_dir.join("index.cjs"),
        "module.exports = {\n  getFileInfo: async (filePath, options) => ({ ignored: Boolean(options && options.ignorePath), inferredParser: 'babel' }),\n  resolveConfig: async () => null,\n  format: async (source) => `formatted:${source}`,\n};\n",
    )
    .unwrap();
}

fn initialize_workspace(harness: &mut LspHarness, workspace_dir: &Path) {
    let initialize = harness.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": path_to_url(workspace_dir).to_string(),
            "workspaceFolders": [{
                "uri": path_to_url(workspace_dir).to_string(),
                "name": workspace_dir.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
            }],
            "capabilities": {},
            "clientInfo": { "name": "cargo-test" }
        }),
    );

    assert_eq!(
        initialize["result"]["capabilities"]["documentFormattingProvider"],
        json!(true)
    );
    assert_eq!(
        initialize["result"]["capabilities"]["workspace"]["workspaceFolders"]["supported"],
        json!(true)
    );
    assert_eq!(
        initialize["result"]["capabilities"]["workspace"]["workspaceFolders"]["changeNotifications"],
        json!(true)
    );

    harness.notify("initialized", json!({}));
}

fn change_workspace_folders(harness: &mut LspHarness, added: Vec<&Path>, removed: Vec<&Path>) {
    harness.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": added
                    .into_iter()
                    .map(|path| json!({
                        "uri": path_to_url(path).to_string(),
                        "name": path.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
                    }))
                    .collect::<Vec<_>>(),
                "removed": removed
                    .into_iter()
                    .map(|path| json!({
                        "uri": path_to_url(path).to_string(),
                        "name": path.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
                    }))
                    .collect::<Vec<_>>()
            }
        }),
    );
}

fn change_document(harness: &mut LspHarness, file_path: &Path, version: i64, text: &str) {
    harness.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {
                "uri": path_to_url(file_path).to_string(),
                "version": version
            },
            "contentChanges": [{ "text": text }]
        }),
    );
}

fn open_document(harness: &mut LspHarness, file_path: &Path, text: &str) {
    harness.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": path_to_url(file_path).to_string(),
                "languageId": "javascript",
                "version": 1,
                "text": text
            }
        }),
    );
}

fn format_document(harness: &mut LspHarness, file_path: &Path) -> Value {
    harness.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": path_to_url(file_path).to_string() },
            "options": {
                "tabSize": 2,
                "insertSpaces": true
            }
        }),
    )
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
            if self.stdout.read_line(&mut header).unwrap() == 0 {
                panic!("language server closed stdout before responding");
            }

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
