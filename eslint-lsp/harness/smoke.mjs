#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createLspHarness } from "./lsp-harness.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const fixtureRoot = await mkdtemp(path.join(tmpdir(), "eslint-lsp-smoke-"));
const workerStateFile = path.join(fixtureRoot, "eslint-worker-state.json");
const targetFile = path.join(fixtureRoot, "src", "example.js");

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

  const harness = createLspHarness({
    repoRoot,
    captureApplyEdits: true,
  });

  const rootUri = pathToFileURL(fixtureRoot).href;
  const documentUri = pathToFileURL(targetFile).href;

  const init = await harness.request("initialize", {
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

  harness.send({ jsonrpc: "2.0", method: "initialized", params: {} });
  harness.send({
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

  const diagnostics = await harness.waitForDiagnostics(documentUri);
  assert.equal(diagnostics.diagnostics.length, 1);
  assert.equal(diagnostics.diagnostics[0].code, "semi");

  const actions = await Promise.race([
    harness.request("textDocument/codeAction", {
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

  await harness.request("workspace/executeCommand", {
    command: fixAll.command.command,
    arguments: fixAll.command.arguments,
  });

  assert.equal(harness.state.appliedEdits.length, 1);
  assert.equal(
    harness.state.appliedEdits[0].changes[documentUri][0].newText,
    "const answer = 42;\n",
  );

  const unrelatedActions = await harness.request("textDocument/codeAction", {
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

  const formatting = await harness.request("textDocument/formatting", {
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

  await harness.shutdown();

  console.log("smoke harness passed");
}

try {
  await main();
} finally {
  await rm(fixtureRoot, { recursive: true, force: true });
}
