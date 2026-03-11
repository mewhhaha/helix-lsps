import { spawn } from "node:child_process";
import { once } from "node:events";
import { tmpdir } from "node:os";
import path from "node:path";

function encodeMessage(message) {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  return Buffer.concat([
    Buffer.from(`Content-Length: ${body.length}\r\n\r\n`, "utf8"),
    body,
  ]);
}

export function createLspHarness({ repoRoot, captureApplyEdits = false }) {
  const state = {
    nextId: 1,
    pending: new Map(),
    diagnostics: [],
    appliedEdits: [],
  };

  const server = spawn("cargo", ["run", "--quiet"], {
    cwd: repoRoot,
    env: {
      ...process.env,
      CARGO_HOME: path.join(tmpdir(), "eslint-lsp-cargo-home"),
    },
    stdio: ["pipe", "pipe", "inherit"],
  });

  let buffer = Buffer.alloc(0);

  server.stdout.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);

    while (true) {
      const delimiterIndex = buffer.indexOf("\r\n\r\n");
      if (delimiterIndex === -1) {
        return;
      }

      const header = buffer.slice(0, delimiterIndex).toString("utf8");
      const match = header.match(/Content-Length: (\d+)/i);
      if (!match) {
        throw new Error(`missing content length in header: ${header}`);
      }

      const contentLength = Number(match[1]);
      const messageEnd = delimiterIndex + 4 + contentLength;
      if (buffer.length < messageEnd) {
        return;
      }

      const payload = buffer
        .slice(delimiterIndex + 4, messageEnd)
        .toString("utf8");
      buffer = buffer.slice(messageEnd);

      handleServerMessage(JSON.parse(payload));
    }
  });

  function send(message) {
    server.stdin.write(encodeMessage(message));
  }

  function request(method, params) {
    const id = state.nextId++;
    const promise = new Promise((resolve, reject) => {
      state.pending.set(id, { resolve, reject });
    });

    const message = { jsonrpc: "2.0", id, method };
    if (params !== undefined) {
      message.params = params;
    }

    send(message);
    return promise;
  }

  function handleServerMessage(message) {
    if (typeof message.id === "number" && state.pending.has(message.id)) {
      const pending = state.pending.get(message.id);
      state.pending.delete(message.id);

      if (message.error) {
        pending.reject(new Error(JSON.stringify(message.error)));
      } else {
        pending.resolve(message.result);
      }
      return;
    }

    if (message.method === "textDocument/publishDiagnostics") {
      state.diagnostics.push(message.params);
      return;
    }

    if (message.method === "workspace/applyEdit") {
      if (captureApplyEdits) {
        state.appliedEdits.push(message.params.edit);
      }

      send({
        jsonrpc: "2.0",
        id: message.id,
        result: { applied: true },
      });
    }
  }

  function waitForDiagnostics(uri) {
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        reject(new Error("timed out waiting for publishDiagnostics"));
      }, 20000);

      const interval = setInterval(() => {
        const hit = state.diagnostics.find((entry) => entry.uri === uri);
        if (!hit) {
          return;
        }

        clearInterval(interval);
        clearTimeout(timeout);
        resolve(hit);
      }, 50);
    });
  }

  async function shutdown() {
    const exitPromise = once(server, "exit");

    await request("shutdown");
    send({ jsonrpc: "2.0", method: "exit" });
    server.stdin.end();

    const timeout = setTimeout(() => {
      if (server.exitCode === null && server.signalCode === null) {
        server.kill("SIGTERM");
      }
    }, 1000);

    try {
      await exitPromise;
    } finally {
      clearTimeout(timeout);
    }
  }

  return {
    state,
    server,
    send,
    request,
    waitForDiagnostics,
    shutdown,
  };
}
