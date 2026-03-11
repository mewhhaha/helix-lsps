import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import readline from "node:readline";

const [eslintPackageJson, cwd, configFormat] = process.argv.slice(2);
const requireFromProject = createRequire(pathToFileURL(eslintPackageJson));
const eslintModule = requireFromProject("eslint");

async function loadCtor(format) {
  if (typeof eslintModule.loadESLint === "function") {
    if (format === "flat") {
      return eslintModule.loadESLint({ useFlatConfig: true });
    }
    if (format === "eslintrc") {
      return eslintModule.loadESLint({ useFlatConfig: false });
    }
    return eslintModule.loadESLint();
  }

  if (format === "flat") {
    try {
      const fallback = requireFromProject("eslint/use-at-your-own-risk");
      return fallback.FlatESLint ?? fallback.ESLint ?? eslintModule.ESLint;
    } catch {
      return eslintModule.ESLint ?? eslintModule.default ?? eslintModule;
    }
  }

  if (format === "eslintrc") {
    try {
      const fallback = requireFromProject("eslint/use-at-your-own-risk");
      return (
        fallback.LegacyESLint ??
        eslintModule.ESLint ??
        eslintModule.default ??
        eslintModule
      );
    } catch {
      return eslintModule.ESLint ?? eslintModule.default ?? eslintModule;
    }
  }

  return eslintModule.ESLint ?? eslintModule.default ?? eslintModule;
}

const ESLintCtor = await loadCtor(configFormat);
let lintEngine;
let fixEngine;

function engineForFix(fix) {
  if (fix) {
    fixEngine ??= new ESLintCtor({ cwd, fix: true });
    return fixEngine;
  }

  lintEngine ??= new ESLintCtor({ cwd, fix: false });
  return lintEngine;
}

async function handleRequest(line) {
  const request = JSON.parse(line);

  try {
    const eslint = engineForFix(Boolean(request.fix));
    const [result = { messages: [] }] = await eslint.lintText(request.text, {
      filePath: request.filePath,
    });

    return {
      id: request.id,
      ok: true,
      diagnostics: result.messages.map((message) => ({
        ruleId: message.ruleId ?? null,
        severity: message.severity ?? 1,
        message: message.message,
        line: message.line ?? 1,
        column: message.column ?? 1,
        endLine: message.endLine ?? message.line ?? 1,
        endColumn: message.endColumn ?? message.column ?? 1,
      })),
      fixedText: typeof result.output === "string" ? result.output : null,
    };
  } catch (error) {
    return {
      id: request.id,
      ok: false,
      error: error && error.stack ? error.stack : String(error),
    };
  }
}

const rl = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

for await (const line of rl) {
  if (!line.trim()) {
    continue;
  }

  const response = await handleRequest(line);
  process.stdout.write(`${JSON.stringify(response)}\n`);
}
