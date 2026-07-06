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

## Helix languages.toml

These are the configurations you can add to your `<config_dir>/helix/languages.toml`. This includes the `tailwindcss-language-server` which isn't from this repo.

```toml
[language-server.oxc]
command = "oxc-lsp"

[language-server.prettier]
command = "prettier-lsp"

[language-server.eslint]
command = "eslint-lsp"

[language-server.tsgo]
command = "tsgo-lsp"

[language-server.tailwindcss]
command = "tailwindcss-language-server"

[[language]]
name = "javascript"
language-servers = ["oxc", "tailwindcss", "prettier", "eslint", "tsgo"]
auto-format = true

[[language]]
name = "typescript"
language-servers = ["oxc", "tailwindcss", "prettier", "eslint", "tsgo"]
auto-format = true

[[language]]
name = "jsx"
language-servers = ["oxc", "tailwindcss", "prettier", "eslint", "tsgo"]
auto-format = true

[[language]]
name = "tsx"
language-servers = ["oxc", "tailwindcss", "prettier", "eslint", "tsgo"]
auto-format = true
```
