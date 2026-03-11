import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { tmpdir } from "node:os";
import { createLspHarness } from "./lsp-harness.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.join(__dirname, "..");
const fixtureRoot = await mkdtemp(path.join(tmpdir(), "eslint-lsp-monorepo-"));
const workerStateFile = path.join(fixtureRoot, "eslint-worker-state.json");
const packageRoot = path.join(fixtureRoot, "packages", "app");
const targetFile = path.join(packageRoot, "src", "index.js");

function fakeEslintModule(scope) {
  return `
const fs = require("node:fs");
const statePath = ${JSON.stringify(workerStateFile)};

function record(event) {
  let state = { moduleLoads: [], constructorScopes: [] };
  if (fs.existsSync(statePath)) {
    state = JSON.parse(fs.readFileSync(statePath, "utf8"));
  }

  if (event.type === "moduleLoad") {
    state.moduleLoads.push(event.scope);
  }

  if (event.type === "constructor") {
    state.constructorScopes.push(event.scope);
  }

  fs.writeFileSync(statePath, JSON.stringify(state));
}

record({ type: "moduleLoad", scope: ${JSON.stringify(scope)} });

class FakeESLint {
  constructor() {
    record({ type: "constructor", scope: ${JSON.stringify(scope)} });
  }

  async lintText(text) {
    const hasSemi = text.trimEnd().endsWith(";");
    const diagnostics = hasSemi
      ? []
      : [{
          ruleId: ${JSON.stringify(`${scope}/semi`)},
          severity: 2,
          message: ${JSON.stringify(`${scope} eslint selected`)},
          line: 1,
          column: Math.max(text.replace(/\\n$/, "").length + 1, 1),
          endLine: 1,
          endColumn: Math.max(text.replace(/\\n$/, "").length + 1, 1),
        }];

    return [{ messages: diagnostics }];
  }
}

module.exports = {
  loadESLint: async () => FakeESLint,
  ESLint: FakeESLint,
};
`;
}

async function setupFixture() {
  await mkdir(path.join(packageRoot, "src"), { recursive: true });
  await mkdir(path.join(fixtureRoot, "node_modules", "eslint"), {
    recursive: true,
  });
  await mkdir(path.join(packageRoot, "node_modules", "eslint"), {
    recursive: true,
  });

  await writeFile(
    path.join(fixtureRoot, "package.json"),
    JSON.stringify(
      {
        name: "eslint-lsp-monorepo",
        private: true,
        workspaces: ["packages/*"],
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
    path.join(packageRoot, "package.json"),
    JSON.stringify(
      {
        name: "@smoke/app",
        private: true,
        type: "module",
      },
      null,
      2,
    ),
  );

  for (const [moduleRoot, scope] of [
    [path.join(fixtureRoot, "node_modules", "eslint"), "root"],
    [path.join(packageRoot, "node_modules", "eslint"), "package"],
  ]) {
    await writeFile(
      path.join(moduleRoot, "package.json"),
      JSON.stringify(
        {
          name: "eslint",
          version: `0.0.0-${scope}`,
          main: "index.cjs",
        },
        null,
        2,
      ),
    );
    await writeFile(path.join(moduleRoot, "index.cjs"), fakeEslintModule(scope));
  }

  await writeFile(
    workerStateFile,
    JSON.stringify({ moduleLoads: [], constructorScopes: [] }),
  );
  await writeFile(targetFile, "const answer = 42\n");
}

async function main() {
  await setupFixture();

  const harness = createLspHarness({ repoRoot });

  const rootUri = pathToFileURL(fixtureRoot).href;
  const documentUri = pathToFileURL(targetFile).href;

  await harness.request("initialize", {
    processId: process.pid,
    rootUri,
    capabilities: {},
    workspaceFolders: [{ uri: rootUri, name: "monorepo" }],
  });

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
  assert.equal(diagnostics.diagnostics[0].code, "package/semi");
  assert.equal(diagnostics.diagnostics[0].message, "package eslint selected");

  const workerState = JSON.parse(await readFile(workerStateFile, "utf8"));
  assert.deepEqual(workerState, {
    moduleLoads: ["package"],
    constructorScopes: ["package"],
  });

  await harness.shutdown();

  console.log("monorepo smoke harness passed");
}

try {
  await main();
} finally {
  await rm(fixtureRoot, { recursive: true, force: true });
}
