#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const fixtureRoot = await mkdtemp(path.join(tmpdir(), "eslint-lsp-smoke-"));
const workerStateFile = path.join(fixtureRoot, "eslint-worker-state.json");
const targetFile = path.join(fixtureRoot, "src", "example.js");

const state = {
  nextId: 1,
  pending: new Map(),
  diagnostics: [],
  appliedEdits: [],
};

function encodeMessage(message) {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  return Buffer.concat([
    Buffer.from(`Content-Length: ${body.length}\r\n\r\n`, "utf8"),
    body,
  ]);
}

function send(server, message) {
  server.stdin.write(encodeMessage(message));
}

function request(server, method, params) {
  const id = state.nextId++;
  const promise = new Promise((resolve, reject) => {
    state.pending.set(id, { resolve, reject });
  });

  const message = { jsonrpc: "2.0", id, method };
  if (params !== undefined) {
    message.params = params;
  }

  send(server, message);
  return promise;
}

function installProtocolParser(server) {
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
    state.appliedEdits.push(message.params.edit);
    send(serverRef, {
      jsonrpc: "2.0",
      id: message.id,
      result: { applied: true },
    });
  }
}

let serverRef;

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

async function setupFixture() {
  await mkdir(path.join(fixtureRoot, "src"), { recursive: true });
  await mkdir(path.join(fixtureRoot, "node_modules", "eslint"), {
    recursive: true,
  });
  await writeFile(
    path.join(fixtureRoot, "package.json"),
    JSON.stringify(
      {
        name: "eslint-lsp-smoke",
        private: true,
        type: "module",
      },
      null,
      2,
    ),
  );
  await writeFile(
    path.join(fixtureRoot, "eslint.config.js"),
    `export default [{ rules: { semi: ["error", "always"] } }];\n`,
  );
  await writeFile(
    path.join(fixtureRoot, "node_modules", "eslint", "package.json"),
    JSON.stringify(
      {
        name: "eslint",
        version: "0.0.0-harness",
        main: "index.cjs",
      },
      null,
      2,
    ),
  );
  await writeFile(
    path.join(fixtureRoot, "node_modules", "eslint", "index.cjs"),
    `const fs = require("node:fs");
const path = require("node:path");
const statePath = path.join(__dirname, "..", "..", "eslint-worker-state.json");

function bump(key) {
  let state = { moduleLoads: 0, loadCalls: 0, constructors: 0 };
  if (fs.existsSync(statePath)) {
    state = JSON.parse(fs.readFileSync(statePath, "utf8"));
  }
  state[key] += 1;
  fs.writeFileSync(statePath, JSON.stringify(state));
}

bump("moduleLoads");

class FakeESLint {
  constructor(options = {}) {
    this.fix = Boolean(options.fix);
    bump("constructors");
  }

  async lintText(text) {
    const hasSemi = text.trimEnd().endsWith(";");
    const diagnostics = hasSemi
      ? []
      : [{
          ruleId: "semi",
          severity: 2,
          message: "Missing semicolon.",
          line: 1,
          column: Math.max(text.replace(/\\n$/, "").length + 1, 1),
          endLine: 1,
          endColumn: Math.max(text.replace(/\\n$/, "").length + 1, 1),
        }];

    return [{
      messages: diagnostics,
      output: this.fix && !hasSemi ? text.replace(/\\n?$/, ";\\n") : undefined,
    }];
  }
}

module.exports = {
  loadESLint: async () => {
    bump("loadCalls");
    return FakeESLint;
  },
  ESLint: FakeESLint,
};
`,
  );
  await writeFile(
    workerStateFile,
    JSON.stringify({ moduleLoads: 0, loadCalls: 0, constructors: 0 }),
  );
  await writeFile(targetFile, "const answer = 42\n");
}

async function main() {
  await setupFixture();

  const server = spawn("cargo", ["run", "--quiet"], {
    cwd: repoRoot,
    env: {
      ...process.env,
      CARGO_HOME: path.join(tmpdir(), "eslint-lsp-cargo-home"),
    },
    stdio: ["pipe", "pipe", "inherit"],
  });
  serverRef = server;
  installProtocolParser(server);

  const rootUri = pathToFileURL(fixtureRoot).href;
  const documentUri = pathToFileURL(targetFile).href;

  const init = await request(server, "initialize", {
    processId: process.pid,
    rootUri,
    capabilities: {
      workspace: {
        applyEdit: true,
      },
    },
    workspaceFolders: [{ uri: rootUri, name: "smoke" }],
  });

  assert.equal(init.serverInfo?.name, "eslint-lsp");

  send(server, { jsonrpc: "2.0", method: "initialized", params: {} });
  send(server, {
    jsonrpc: "2.0",
    method: "textDocument/didOpen",
    params: {
      textDocument: {
        uri: documentUri,
        languageId: "javascript",
        version: 1,
        text: "const answer = 42\n",
      },
    },
  });

  const diagnostics = await waitForDiagnostics(documentUri);
  assert.equal(diagnostics.diagnostics.length, 1);
  assert.equal(diagnostics.diagnostics[0].code, "semi");

  const actions = await Promise.race([
    request(server, "textDocument/codeAction", {
      textDocument: { uri: documentUri },
      range: {
        start: { line: 0, character: 0 },
        end: { line: 0, character: 17 },
      },
      context: {
        diagnostics: diagnostics.diagnostics,
        only: ["source.fixAll", "source.fixAll.eslint"],
      },
    }),
    new Promise((_, reject) => {
      setTimeout(() => reject(new Error("codeAction request timed out")), 1000);
    }),
  ]);

  assert.ok(Array.isArray(actions) && actions.length > 0);
  const fixAll = actions.find((entry) => entry.kind === "source.fixAll.eslint");
  assert.ok(fixAll, "expected source.fixAll.eslint code action");
  assert.equal(fixAll.command?.command, "eslint.applyFixAll");

  await request(server, "workspace/executeCommand", {
    command: fixAll.command.command,
    arguments: fixAll.command.arguments,
  });

  assert.equal(state.appliedEdits.length, 1);
  assert.equal(
    state.appliedEdits[0].changes[documentUri][0].newText,
    "const answer = 42;\n",
  );

  const unrelatedActions = await request(server, "textDocument/codeAction", {
    textDocument: { uri: documentUri },
    range: {
      start: { line: 0, character: 0 },
      end: { line: 0, character: 17 },
    },
    context: {
      diagnostics: diagnostics.diagnostics,
      only: ["source.organizeImports"],
    },
  });

  assert.ok(!unrelatedActions || unrelatedActions.length === 0);

  const formatting = await request(server, "textDocument/formatting", {
    textDocument: { uri: documentUri },
    options: { tabSize: 2, insertSpaces: true },
  });

  assert.equal(formatting[0].newText, "const answer = 42;\n");

  const workerState = JSON.parse(await readFile(workerStateFile, "utf8"));
  assert.deepEqual(workerState, {
    moduleLoads: 1,
    loadCalls: 1,
    constructors: 2,
  });

  await request(server, "shutdown");
  send(server, { jsonrpc: "2.0", method: "exit" });

  console.log("smoke harness passed");
}

try {
  await main();
} finally {
  await rm(fixtureRoot, { recursive: true, force: true });
}
