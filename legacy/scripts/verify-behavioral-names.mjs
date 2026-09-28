import { execFile as execFileCallback } from "node:child_process";
import { open, readFile, stat } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";

const execFile = promisify(execFileCallback);
const repositoryRoot = process.cwd();
const maximumInspectedFileBytes = 16 * 1024 * 1024;
const binaryProbeBytes = 8192;
const twoDigits = "[0-9]{2}";
const planTokenSource = `(?:p${twoDigits}[-_ /]?b${twoDigits}|(?:MC|RI|CP|CAP)[-_ /]?P${twoDigits})`;
const planToken = new RegExp(planTokenSource, "i");
const planTokens = new RegExp(planTokenSource, "gi");
const reviewTokenSource =
  `\\b(?:CDA[-_ /]?[0-9]{3,}|(?:MC|RI|CT|INV|CP|CAP)[-_ /]?P[0-9]{2}|(?:MC|RI|CT)[-_ ]?(?:REVIEW|MEASUREMENT|CONTRACT)[-_ ]?[A-Z0-9-]+)\\b`;
const reviewToken = new RegExp(reviewTokenSource, "i");
const reviewTokens = new RegExp(reviewTokenSource, "gi");
const numberedLabel = new RegExp(`(?:\\bP${twoDigits}\\b|\\bB${twoDigits}\\b)`, "i");
const numberedLabels = new RegExp(numberedLabel.source, "gi");
const stableMetricTokens = new Set([["p", "50"].join(""), ["p", "95"].join("")]);
const versionLabel = new RegExp(
  `\\b[0-9]+(?:\\.[0-9]+){1,2}(?:[_-][0-9]+)?[_-]b${twoDigits}\\b`,
  "i",
);
const opaquePayload = /data:[^,]+,[A-Za-z0-9+/=]+/gi;
const signedEvidence = /managed-install-v1\.[A-Za-z0-9._-]+/g;
const lockIntegrity = /\bintegrity:\s*sha(?:256|384|512)-[A-Za-z0-9+/=]+/gi;
const policyContext = /\bj[01]-s[01]-r[01]-u[01]\b/g;

function permittedNumberedLabel(line, match) {
  const token = match[0].toLowerCase();
  if (stableMetricTokens.has(token)) return true;
  for (const version of line.matchAll(new RegExp(versionLabel.source, "gi"))) {
    const start = version.index;
    const end = start + version[0].length;
    if (match.index >= start && match.index + match[0].length <= end) return true;
  }
  return false;
}
const phaseLabel = /\b(?:phase|boundary)[\s_-]*[0-9]+(?:[A-Za-z][A-Za-z0-9]*)?/i;
const phaseLabels = new RegExp(phaseLabel.source, "gi");
const ordinalContext =
  "(?:Phase|Boundary|Contract|Review|Measurement|Fixture|Invariant|Requirement)";
const ordinalLabel = new RegExp(
  `(?:\\b(?:R|I)[0-9]{1,2}(?=\\b|[_-])|${ordinalContext}(?:R|I)[0-9]{1,2}(?:${ordinalContext})?|(?:R|I)[0-9]{1,2}${ordinalContext}|\\b(?:r|i)0[0-9]\\b|\\b(?:r|i)[0-9]{1,2}[_-])`,
);
const ordinalLabels = new RegExp(ordinalLabel.source, "g");

function withoutOpaquePayloads(line) {
  return line
    .replace(opaquePayload, "")
    .replace(signedEvidence, "")
    .replace(lockIntegrity, "")
    .replace(policyContext, "");
}

function findMatches(relativePath, source) {
  const matches = [];
  const lines = source.split(/\r?\n/);
  for (const [index, line] of lines.entries()) {
    const inspected = withoutOpaquePayloads(line);
    for (const pattern of [planTokens, reviewTokens, phaseLabels, ordinalLabels]) {
      for (const match of inspected.matchAll(pattern)) {
        matches.push(`${relativePath}:${index + 1}: ${match[0]}`);
      }
    }
    for (const match of inspected.matchAll(numberedLabels)) {
      if (!permittedNumberedLabel(inspected, match)) {
        matches.push(`${relativePath}:${index + 1}: ${match[0]}`);
      }
    }
  }
  return matches;
}

const { stdout } = await execFile("git", ["ls-files", "-co", "--exclude-standard", "-z"], {
  cwd: repositoryRoot,
  maxBuffer: 4 * 1024 * 1024,
});
const paths = stdout.split("\0").filter(Boolean);
const violations = [];

for (const relativePath of paths) {
  if (
    planToken.test(relativePath) ||
    reviewToken.test(relativePath) ||
    numberedLabel.test(relativePath) ||
    phaseLabel.test(relativePath) ||
    ordinalLabel.test(relativePath)
  ) {
    violations.push(`${relativePath}: filename contains an execution-plan identifier`);
    continue;
  }
  const absolutePath = path.join(repositoryRoot, relativePath);
  const metadata = await stat(absolutePath).catch((error) => {
    if (error.code === "ENOENT") return null;
    throw error;
  });
  if (metadata === null) continue;
  if (!metadata.isFile()) continue;
  if (metadata.size > maximumInspectedFileBytes) {
    const handle = await open(absolutePath, "r");
    try {
      const probe = Buffer.allocUnsafe(binaryProbeBytes);
      const { bytesRead } = await handle.read(probe, 0, probe.length, 0);
      if (!probe.subarray(0, bytesRead).includes(0)) {
        violations.push(
          `${relativePath}: text file exceeds the behavioral-name inspection bound`,
        );
      }
    } finally {
      await handle.close();
    }
    continue;
  }
  const source = await readFile(absolutePath, "utf8");
  if (source.includes("\0")) continue;
  violations.push(...findMatches(relativePath, source));
}

if (violations.length > 0) {
  process.stderr.write(`behavioral-name policy failed with ${violations.length} violation(s):\n`);
  process.stderr.write(`${violations.join("\n")}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write("behavioral-name policy passed\n");
}
