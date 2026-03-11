import { createRequire } from "node:module";
import { dirname, isAbsolute, join, relative } from "node:path";
import { pathToFileURL } from "node:url";
import process from "node:process";
import readline from "node:readline";

const require = createRequire(import.meta.url);
const prettierCache = new Map();

function writeResponse(payload) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
}

function isWithinRoot(workspaceRoot, targetPath) {
  if (!workspaceRoot) {
    return false;
  }

  const relativePath = relative(workspaceRoot, targetPath);
  return (
    relativePath === "" ||
    (!relativePath.startsWith("..") && !isAbsolute(relativePath))
  );
}

function resolvePrettier(targetFilePath, workspaceRoot) {
  const stopDir = isWithinRoot(workspaceRoot, targetFilePath)
    ? workspaceRoot
    : null;

  let currentDir = dirname(targetFilePath);
  let lastError;

  while (true) {
    try {
      return require.resolve(join(currentDir, "node_modules", "prettier"));
    } catch (error) {
      lastError = error;
    }

    if (stopDir && currentDir === stopDir) {
      break;
    }

    const parentDir = dirname(currentDir);
    if (parentDir === currentDir) {
      break;
    }

    currentDir = parentDir;
  }

  throw lastError ?? new Error(`could not resolve prettier for ${targetFilePath}`);
}

async function loadPrettier(targetFilePath, workspaceRoot) {
  const resolved = resolvePrettier(targetFilePath, workspaceRoot);

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
  const { file_path: filePath, source, workspace_root: workspaceRoot } = request;

  if (!filePath || typeof source !== "string") {
    return formatError("invalid_request", "expected file_path and source");
  }

  let prettier;

  try {
    prettier = await loadPrettier(filePath, workspaceRoot);
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
