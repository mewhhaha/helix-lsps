import { createRequire } from "node:module";
import { dirname } from "node:path";
import { pathToFileURL } from "node:url";
import process from "node:process";
import readline from "node:readline";

const require = createRequire(import.meta.url);
const prettierCache = new Map();

function writeResponse(payload) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
}

async function loadPrettier(targetFilePath) {
  const resolved = require.resolve("prettier", {
    paths: [dirname(targetFilePath)],
  });

  let prettier = prettierCache.get(resolved);
  if (!prettier) {
    const loaded = await import(pathToFileURL(resolved).href);
    prettier = loaded.default ?? loaded;
    prettierCache.set(resolved, prettier);
  }

  return prettier;
}

function formatError(code, error) {
  return {
    kind: "error",
    code,
    message: error instanceof Error ? error.stack ?? error.message : String(error),
  };
}

async function handleRequest(request) {
  const { file_path: filePath, source } = request;

  if (!filePath || typeof source !== "string") {
    return formatError("invalid_request", "expected file_path and source");
  }

  let prettier;

  try {
    prettier = await loadPrettier(filePath);
  } catch (error) {
    return formatError("missing_prettier", error);
  }

  const fileInfo = await prettier.getFileInfo(filePath, {
    resolveConfig: false,
    withNodeModules: false,
  });

  if (fileInfo.ignored) {
    return { kind: "ignored" };
  }

  if (!fileInfo.inferredParser) {
    return { kind: "unsupported" };
  }

  const config =
    (await prettier.resolveConfig(filePath, { editorconfig: true })) ?? {};
  const formatted = await prettier.format(source, {
    ...config,
    filepath: filePath,
  });

  return { kind: "formatted", formatted };
}

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

for await (const line of input) {
  if (!line.trim()) {
    continue;
  }

  try {
    const request = JSON.parse(line);
    const response = await handleRequest(request);
    writeResponse(response);
  } catch (error) {
    writeResponse(formatError("prettier_error", error));
  }
}
