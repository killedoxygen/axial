import assert from "node:assert/strict";
import { execFile as execFileCallback } from "node:child_process";
import { mkdtemp, mkdir, open, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";

const execFile = promisify(execFileCallback);
const repositoryRoot = path.resolve(import.meta.dirname, "../..");
const verifier = path.join(repositoryRoot, "scripts/verify-behavioral-names.mjs");

async function temporaryRepository(t) {
  const directory = await mkdtemp(path.join(os.tmpdir(), "axial-behavioral-names-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  await execFile("git", ["init", "--quiet"], { cwd: directory });
  return directory;
}

async function runVerifier(cwd) {
  try {
    const result = await execFile(process.execPath, [verifier], {
      cwd,
      maxBuffer: 4 * 1024 * 1024,
    });
    return { code: 0, stderr: result.stderr, stdout: result.stdout };
  } catch (error) {
    return {
      code: Number(error.code),
      stderr: String(error.stderr ?? ""),
      stdout: String(error.stdout ?? ""),
    };
  }
}

test("behavioral name verifier inspects every non-opaque token and untracked file", async (t) => {
  const directory = await temporaryRepository(t);
  const planLabel = ["p", "03", "_b", "03"].join("");
  const reviewLabel = ["Cda", "123"].join("");
  const slashCdaReview = ["CDA", "/", "123"].join("");
  const lowercaseReview = ["mc", "-review-", "sample"].join("");
  const slashReview = ["CT", "/P", "01"].join("");
  const camelReview = ["MC", "Review", "Sample"].join("");
  const numberedLabel = ["P", "01"].join("");
  const lifecycleLabel = ["phase", "1", "Native"].join("");
  const ordinalLabel = ["Fixture", "R", "1", "Measurement"].join("");
  const lowercaseOrdinal = ["r", "01"].join("");

  await mkdir(path.join(directory, "scripts"), { recursive: true });
  const verifierSource = await readFile(verifier, "utf8");
  await writeFile(
    path.join(directory, "scripts/verify-behavioral-names.mjs"),
    `${verifierSource}\n// ${planLabel}\n`,
  );
  await execFile("git", ["add", "scripts/verify-behavioral-names.mjs"], { cwd: directory });

  await writeFile(
    path.join(directory, "candidate.txt"),
    [
      `data:image/png;base64,AAAA // ${reviewLabel}`,
      `managed-install-v1.Abc_def-123 // p95 ${numberedLabel}`,
      slashCdaReview,
      lowercaseReview,
      slashReview,
      camelReview,
      lifecycleLabel,
      ordinalLabel,
      lowercaseOrdinal,
    ].join("\n"),
  );

  const result = await runVerifier(directory);
  assert.notEqual(result.code, 0);
  for (const expected of [
    planLabel,
    reviewLabel,
    slashCdaReview,
    lowercaseReview,
    slashReview,
    camelReview,
    numberedLabel,
    lifecycleLabel,
    ordinalLabel,
    lowercaseOrdinal,
  ]) {
    assert.ok(result.stderr.includes(expected), `missing violation for ${expected}`);
  }
  assert.match(result.stderr, /candidate\.txt/);
  assert.match(result.stderr, /scripts\/verify-behavioral-names\.mjs/);
});

test("behavioral name verifier permits domain versions metrics and opaque payloads", async (t) => {
  const directory = await temporaryRepository(t);
  await writeFile(
    path.join(directory, "domain-data.txt"),
    [
      "p50 p95",
      "openjdk 1.8.0_312-b07",
      "j0-s0-r0-u0",
      "mc_dir capped cp-pill",
      "P7dR8mSH",
      "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA",
      "managed-install-v1.T7ghN0PBffcxr4Rg08bVvTPOl9fRcUh9qyNnWZtd93c",
      "integrity: sha512-WPs+bbQw5aCj+x6laNGWLH3wviHtoCv/P3+otBhbOhJgG8qtpdAMlTCxLtsTWA7L",
    ].join("\n"),
  );
  await execFile("git", ["add", "domain-data.txt"], { cwd: directory });

  const result = await runVerifier(directory);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /behavioral-name policy passed/);
});

test("behavioral name verifier fails closed for oversized text", async (t) => {
  const directory = await temporaryRepository(t);
  const candidate = path.join(directory, "oversized.txt");
  const handle = await open(candidate, "w");
  try {
    await handle.write(Buffer.alloc(8192, "a"));
    await handle.truncate(16 * 1024 * 1024 + 1);
  } finally {
    await handle.close();
  }
  await execFile("git", ["add", "oversized.txt"], { cwd: directory });

  const result = await runVerifier(directory);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /text file exceeds the behavioral-name inspection bound/);
});

test("repository behavioral names and verifier source pass their own gate", async () => {
  const result = await runVerifier(repositoryRoot);
  assert.equal(result.code, 0, result.stderr);
});
