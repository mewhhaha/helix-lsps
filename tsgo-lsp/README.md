# tsgo-lsp

`tsgo-lsp` is a thin Rust stdio wrapper around the native TypeScript compiler's
`--lsp --stdio` mode. It supports both `@typescript/native-preview` and the
native compiler shipped by TypeScript 7 and newer.

## Behavior

- On startup and when a file is first opened, it discovers the nearest project
  that can provide `tsgo`.
- If a project has its own local install, that project gets its own child `tsgo`
  process.
- If no local install is found, the wrapper falls back to a global `tsgo` on
  `PATH`.
- Requests and notifications for open files are routed to the matching child
  session by file URI.
- Project roots are polled for TypeScript, JavaScript, and JSON file changes so
  external file creation is forwarded to `tsgo` as `workspace/didChangeWatchedFiles`.
- If a child exits or fails initialization, the wrapper returns LSP errors
  instead of leaving requests hanging.

## Discovery

Discovery prefers, in order:

1. `node_modules/.bin/tsgo`
2. `node_modules/@typescript/native-preview/package.json`
3. `node_modules/typescript/package.json` when its major version is 7 or newer
4. Node resolution of either supported package
5. Global `tsgo` on `PATH`

When a package provides a native platform binary, the wrapper launches it
directly so terminating the LSP session also terminates the compiler process.

## Testing

The project uses a harness-driven test setup with a fake `tsgo` binary to cover:

- per-project routing
- startup failure when no `tsgo` is available
- background child initialization failure
- child exit during an in-flight request
