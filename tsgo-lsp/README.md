# tsgo-lsp

`tsgo-lsp` is a thin Rust stdio wrapper around the `tsgo --lsp --stdio` command
from `@typescript/native-preview`.

## Behavior

- On startup and when a file is first opened, it discovers the nearest project
  that can provide `tsgo`.
- If a project has its own local install, that project gets its own child `tsgo`
  process.
- If no local install is found, the wrapper falls back to a global `tsgo` on
  `PATH`.
- Requests and notifications for open files are routed to the matching child
  session by file URI.
- If a child exits or fails initialization, the wrapper returns LSP errors
  instead of leaving requests hanging.

## Discovery

Discovery prefers, in order:

1. `node_modules/.bin/tsgo`
2. `node_modules/@typescript/native-preview/package.json`
3. Node resolution of `@typescript/native-preview/package.json`
4. Global `tsgo` on `PATH`

## Testing

The project uses a harness-driven test setup with a fake `tsgo` binary to cover:

- per-project routing
- startup failure when no `tsgo` is available
- background child initialization failure
- child exit during an in-flight request
