# oxc-lsp

`oxc-lsp` is a Rust stdio wrapper that combines `oxlint --lsp` diagnostics and
code actions with `oxfmt --lsp` formatting.

## Behavior

- Prefers project-local `oxlint` and `oxfmt` binaries from the nearest package.
- Falls back to package-resolved binaries or global `PATH` binaries when needed.
- Routes open files to the matching project session, so monorepos can use
  package-local Oxc installs.
- Forwards normal LSP requests and notifications to `oxlint`.
- Serves `textDocument/formatting` through `oxfmt`.

## Run

```bash
cargo run --quiet
```

The server speaks standard LSP over stdio.

## Testing

```bash
cargo test
```
