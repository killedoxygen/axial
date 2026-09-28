import { readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const maximumSourceBytes = 1024 * 1024;
const maximumSourceLines = 10_000;

function fail(relativePath, lineNumber, message) {
  const location = lineNumber === undefined ? relativePath : `${relativePath}:${lineNumber}`;
  throw new Error(`${location}: ${message}`);
}

function indentation(relativePath, line) {
  if (line.text.includes("\t")) {
    fail(relativePath, line.number, "tabs are not supported in workflow indentation");
  }
  return line.text.length - line.text.trimStart().length;
}

function scalar(value) {
  const source = value.trim();
  let quote;
  let commentStart = source.length;
  for (let index = 0; index < source.length; index += 1) {
    const character = source[index];
    if (quote === '"' && character === "\\") {
      index += 1;
    } else if (quote === "'" && character === "'" && source[index + 1] === "'") {
      index += 1;
    } else if (quote && character === quote) {
      quote = undefined;
    } else if (!quote && (character === '"' || character === "'")) {
      quote = character;
    } else if (!quote && character === "#" && (index === 0 || /\s/.test(source[index - 1]))) {
      commentStart = index;
      break;
    }
  }

  const trimmed = source.slice(0, commentStart).trimEnd();
  const quoted = trimmed.match(/^(["'])(.*)\1$/);
  if (quoted) return quoted[2];
  return trimmed;
}

function parseActionReference(relativePath, line) {
  const match = line.text.match(
    /^\s*(?:-\s+)?uses:\s*([^\s#]+)(?:\s+#\s*(.*))?\s*$/,
  );
  if (!match) fail(relativePath, line.number, "could not parse action reference");

  const action = scalar(match[1]);
  const at = action.lastIndexOf("@");
  return {
    action,
    actionComment: match[2]?.trim(),
    actionLine: line.number,
    actionRef: at === -1 ? undefined : action.slice(at + 1),
    actionRepository: at === -1 ? undefined : action.slice(0, at),
  };
}

function keyValue(relativePath, line) {
  const match = line.text.trim().match(/^([A-Za-z0-9_-]+):(?:\s*(.*))?$/);
  if (!match) {
    fail(relativePath, line.number, "expected a simple YAML key/value entry");
  }
  return { key: match[1], value: match[2] ?? "" };
}

function readLines(relativePath) {
  const absolutePath = resolve(repositoryRoot, relativePath);
  if (!absolutePath.startsWith(`${repositoryRoot}${sep}`)) {
    fail(relativePath, undefined, "path escapes the repository root");
  }

  const stats = statSync(absolutePath);
  if (!stats.isFile()) {
    fail(relativePath, undefined, "expected a regular file");
  }
  if (stats.size > maximumSourceBytes) {
    fail(relativePath, undefined, `source exceeds ${maximumSourceBytes} bytes`);
  }

  const source = readFileSync(absolutePath, "utf8");
  const lines = source.split(/\r?\n/).map((text, index) => ({
    text,
    number: index + 1,
  }));
  if (lines.length > maximumSourceLines) {
    fail(relativePath, undefined, `source exceeds ${maximumSourceLines} lines`);
  }
  return { absolutePath, lines, source };
}

function parseMapping(relativePath, lines, headerIndex, entryIndent) {
  const header = lines[headerIndex];
  const headerEntry = keyValue(relativePath, header);
  if (headerEntry.value !== "") {
    return {
      entries: new Map(),
      line: header.number,
      scalar: scalar(headerEntry.value),
    };
  }

  const entries = new Map();
  for (let index = headerIndex + 1; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.text.trim() === "" || line.text.trimStart().startsWith("#")) {
      continue;
    }
    const indent = indentation(relativePath, line);
    if (indent < entryIndent) break;
    if (indent !== entryIndent) continue;
    const entry = keyValue(relativePath, line);
    entries.set(entry.key, scalar(entry.value));
  }
  return { entries, line: header.number, scalar: undefined };
}

function parseNeeds(relativePath, lines, index, jobEnd) {
  const entry = keyValue(relativePath, lines[index]);
  if (entry.value !== "") {
    const value = scalar(entry.value);
    if (value.startsWith("[") && value.endsWith("]")) {
      return value
        .slice(1, -1)
        .split(",")
        .map((item) => scalar(item))
        .filter(Boolean);
    }
    return value ? [value] : [];
  }

  const needs = [];
  for (let cursor = index + 1; cursor < jobEnd; cursor += 1) {
    const line = lines[cursor];
    if (line.text.trim() === "") continue;
    const indent = indentation(relativePath, line);
    if (indent <= 4) break;
    const match = line.text.trim().match(/^-\s+(.+)$/);
    if (indent === 6 && match) needs.push(scalar(match[1]));
  }
  return needs;
}

function parseStepInputs(relativePath, lines, start, end) {
  const inputs = new Map();
  const withIndex = lines.findIndex(
    (line, index) => index >= start && index < end && line.text.trim() === "with:",
  );
  if (withIndex === -1) return inputs;

  const withIndent = indentation(relativePath, lines[withIndex]);
  for (let index = withIndex + 1; index < end; index += 1) {
    const line = lines[index];
    if (line.text.trim() === "" || line.text.trimStart().startsWith("#")) continue;
    const indent = indentation(relativePath, line);
    if (indent <= withIndent) break;
    if (indent !== withIndent + 2) continue;
    const entry = keyValue(relativePath, line);
    if (entry.value === "|" || entry.value === ">") {
      const block = [];
      for (let cursor = index + 1; cursor < end; cursor += 1) {
        const blockLine = lines[cursor];
        if (blockLine.text.trim() === "") continue;
        if (indentation(relativePath, blockLine) <= indent) break;
        block.push(blockLine.text.trim());
      }
      inputs.set(entry.key, block.join("\n"));
    } else {
      inputs.set(entry.key, scalar(entry.value));
    }
  }
  return inputs;
}

function parseSteps(relativePath, lines, jobStart, jobEnd, jobId) {
  const starts = [];
  for (let index = jobStart + 1; index < jobEnd; index += 1) {
    if (/^\s{6}-\s+/.test(lines[index].text)) starts.push(index);
  }

  return starts.map((start, position) => {
    const end = starts[position + 1] ?? jobEnd;
    const slice = lines.slice(start, end);
    const usesLine = slice.find(
      (line, index) =>
        (index === 0 && /^\s{6}-\s+uses:\s*/.test(line.text)) ||
        /^\s{8}uses:\s*/.test(line.text),
    );
    const runLineIndex = slice.findIndex((line) => /^\s*(?:-\s+)?run:\s*/.test(line.text));
    const nameEntry = slice
      .map((line) => line.text.trim().match(/^-?\s*name:\s*(.+)$/))
      .find(Boolean);

    const actionReference = usesLine ? parseActionReference(relativePath, usesLine) : {};

    let run;
    if (runLineIndex !== -1) {
      const runLine = slice[runLineIndex];
      const entry = runLine.text.trim().replace(/^-\s+/, "").match(/^run:\s*(.*)$/);
      const chunks = [];
      if (entry?.[1] && entry[1] !== "|" && entry[1] !== ">") chunks.push(entry[1]);
      const runIndent = indentation(relativePath, runLine);
      for (let index = runLineIndex + 1; index < slice.length; index += 1) {
        const candidate = slice[index];
        if (candidate.text.trim() === "") continue;
        if (indentation(relativePath, candidate) <= runIndent) break;
        chunks.push(candidate.text.trim());
      }
      run = chunks.join("\n");
    }

    return {
      ...actionReference,
      endLine: lines[end - 1]?.number ?? lines[start].number,
      inputs: parseStepInputs(relativePath, lines, start, end),
      jobId,
      name: nameEntry ? scalar(nameEntry[1]) : undefined,
      run,
      startLine: lines[start].number,
    };
  });
}

export function readRepositorySource(relativePath) {
  return readLines(relativePath).source;
}

export function parseWorkflowScalar(value) {
  return scalar(value);
}

export function parseWorkflow(relativePath) {
  const { lines, source } = readLines(relativePath);
  for (const line of lines) {
    if (
      /^(?:["']permissions["']|\s{4}["'](?:uses|permissions)["']|\s{6}-\s+["']uses["']|\s{8}["']uses["'])\s*:/.test(
        line.text,
      )
    ) {
      fail(relativePath, line.number, "quoted workflow control keys are not supported");
    }
  }
  const jobsHeader = lines.findIndex((line) => /^jobs:\s*(?:#.*)?$/.test(line.text));
  if (jobsHeader === -1) fail(relativePath, undefined, "missing top-level jobs mapping");

  const permissionsHeader = lines.findIndex((line) => /^permissions:\s*(?:[^#]*)?(?:#.*)?$/.test(line.text));
  if (permissionsHeader === -1 || permissionsHeader > jobsHeader) {
    fail(relativePath, undefined, "missing workflow-level permissions before jobs");
  }
  const topPermissions = parseMapping(relativePath, lines, permissionsHeader, 2);

  const jobStarts = [];
  for (let index = jobsHeader + 1; index < lines.length; index += 1) {
    const match = lines[index].text.match(/^\s{2}([A-Za-z0-9_-]+):\s*(?:#.*)?$/);
    if (match) jobStarts.push({ id: match[1], index });
  }
  if (jobStarts.length === 0) fail(relativePath, lines[jobsHeader].number, "jobs mapping is empty");

  const jobs = new Map();
  for (let position = 0; position < jobStarts.length; position += 1) {
    const { id, index: start } = jobStarts[position];
    const end = jobStarts[position + 1]?.index ?? lines.length;
    const permissionsIndex = lines.findIndex(
      (line, index) =>
        index > start && index < end && /^\s{4}permissions:\s*/.test(line.text),
    );
    const needsIndex = lines.findIndex(
      (line, index) => index > start && index < end && /^\s{4}needs:\s*/.test(line.text),
    );
    const permissions =
      permissionsIndex === -1
        ? { entries: new Map(), line: undefined, scalar: undefined }
        : parseMapping(relativePath, lines, permissionsIndex, 6);
    const needs =
      needsIndex === -1 ? [] : parseNeeds(relativePath, lines, needsIndex, end);
    const steps = parseSteps(relativePath, lines, start, end, id);
    const reusableWorkflowLines = lines.filter(
      (line, lineIndex) =>
        lineIndex > start && lineIndex < end && /^\s{4}uses:\s*/.test(line.text),
    );
    if (reusableWorkflowLines.length > 1) {
      fail(relativePath, reusableWorkflowLines[1].number, `${id} has multiple reusable workflows`);
    }
    jobs.set(id, {
      endLine: lines[end - 1]?.number ?? lines[start].number,
      id,
      needs,
      permissions,
      reusableWorkflow: reusableWorkflowLines[0]
        ? parseActionReference(relativePath, reusableWorkflowLines[0])
        : undefined,
      source: lines.slice(start, end).map((line) => line.text).join("\n"),
      startLine: lines[start].number,
      steps,
    });
  }

  return {
    actionReferences: [...jobs.values()].flatMap((job) => [
      ...(job.reusableWorkflow ? [job.reusableWorkflow] : []),
      ...job.steps.filter((step) => step.action),
    ]),
    jobs,
    path: relativePath,
    source,
    steps: [...jobs.values()].flatMap((job) => job.steps),
    topPermissions,
  };
}

export function parseRepositoryWorkflows() {
  return readdirSync(resolve(repositoryRoot, ".github/workflows"), { withFileTypes: true })
    .filter((entry) => entry.isFile() && /\.ya?ml$/.test(entry.name))
    .map((entry) => `.github/workflows/${entry.name}`)
    .sort()
    .map(parseWorkflow);
}

export function remoteActionSteps(workflow) {
  return workflow.steps.filter(
    (step) =>
      step.action?.startsWith("docker://") ||
      (step.actionRepository && !step.actionRepository.startsWith("./")),
  );
}

export function remoteActionReferences(workflow) {
  return workflow.actionReferences.filter(
    (reference) =>
      reference.action?.startsWith("docker://") ||
      (reference.actionRepository && !reference.actionRepository.startsWith("./")),
  );
}
