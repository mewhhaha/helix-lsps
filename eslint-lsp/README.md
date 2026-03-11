# eslint-lsp

`eslint-lsp` is a Rust language server that shells out to the local Node.js toolchain and loads the workspace's own ESLint installation and configuration.

## What it does

- resolves `eslint` from the file's project tree instead of using a bundled/global install
- keeps the config root and the local `eslint` package lookup separate, so monorepo packages can use their own ESLint install
- discovers nearby flat-config and legacy ESLint config roots
- publishes diagnostics on open, change, and save
- exposes `source.fixAll.eslint` code actions
- exposes document formatting backed by ESLint auto-fixes

## Run

```bash
cargo run --quiet
```

The server speaks standard LSP over stdio.

## Smoke harness

The harness creates a temporary project, installs `eslint` locally, starts the Rust server, opens a file over LSP, and asserts that diagnostics plus a fix-all action come back from the local project setup.

```bash
node harness/smoke.mjs
```

There is also a monorepo-focused smoke harness that verifies files inside `packages/*` keep using the workspace root config while resolving `eslint` from the nearest package.

```bash
node harness/monorepo-smoke.mjs
```
