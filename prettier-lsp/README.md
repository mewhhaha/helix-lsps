# prettier-lsp

`prettier-lsp` is a Rust language server that delegates formatting to the
workspace's installed `prettier` instead of embedding a JavaScript runtime or a
bundled prettier version.

## Behavior

- Resolves `prettier` from the target file's project tree, including package-local installs inside a monorepo.
- Bounds discovery to the initialized LSP workspace root so it never leaks into unrelated parent folders.
- Tracks `workspace/didChangeWorkspaceFolders` so newly added roots take effect without restarting the server.
- Keeps a warm Node worker per workspace to avoid paying process startup on every format.
- Uses Prettier's own config resolution for `.prettierrc` and `.editorconfig`.
- Exposes `textDocument/formatting` over stdio.
- Returns no edits for ignored or unsupported files.

## Development

Install the harness dependency once:

```bash
cd harness/workspace
npm install
```

Run the test harness:

```bash
cargo test
```

Run the language server:

```bash
cargo run
```
