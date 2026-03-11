# helix-lsps

This repo is a Cargo workspace for the Helix-oriented LSP wrappers in this
directory:

- `eslint-lsp`
- `prettier-lsp`
- `tsgo-lsp`
- `oxc-lsp`

## Install all LSPs

From the repo root:

```bash
cargo install-lsps
```

That command installs only the real LSP binaries:

- `eslint-lsp`
- `prettier-lsp`
- `tsgo-lsp`
- `oxc-lsp`

The installer uses the workspace `Cargo.lock` by default.

Extra arguments after `--` are forwarded to each underlying `cargo install`
invocation. For example:

```bash
cargo install-lsps -- --root ~/.local
```
