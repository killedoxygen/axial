import { execFile as execFileCallback } from "node:child_process";
import path from "node:path";
import { performance } from "node:perf_hooks";
import process from "node:process";
import { promisify, TextDecoder } from "node:util";

export const scenario = Object.freeze({
  scenario_id: "CP-ANCHORED-FILESYSTEM",
  proof_id: "CAP-ANCHORED-FILESYSTEM",
  capability_id: "anchored-filesystem",
});

export const anchoredFsChecks = Object.freeze([
  Object.freeze({
    id: "api-behavioral-contract",
    cargo_arguments: Object.freeze([
      "test",
      "--locked",
      "-p",
      "axial-api",
      "--lib",
      "--no-default-features",
      "state::managed_library::tests::anchored_filesystem_contract",
      "--",
      "--exact",
    ]),
    passed_requirement: "exactly-one",
  }),
  Object.freeze({
    id: "api-cross-owner-contract",
    cargo_arguments: Object.freeze([
      "test",
      "--locked",
      "-p",
      "axial-api",
      "--lib",
      "--no-default-features",
      "state::managed_library::tests::anchored_filesystem_contract_cross_owner",
      "--",
      "--exact",
    ]),
    passed_requirement: "exactly-one",
  }),
  Object.freeze({
    id: "native-filesystem-suite",
    cargo_arguments: Object.freeze([
      "test",
      "--locked",
      "-p",
      "axial-fs",
      "--lib",
    ]),
    passed_requirement: "nonempty",
  }),
]);

const execFile = promisify(execFileCallback);
const scenarioBudgetMs = 285_000;
const maximumOutputBytes = 4 * 1024 * 1024;
const minimumCommandBudgetMs = 1_000;
const commandResultKeys = Object.freeze(
  [
    "exit_code",
    "failed",
    "filtered_out",
    "ignored",
    "measured",
    "passed",
    "stderr_bytes",
    "stdout_bytes",
    "test_result",
  ].sort(),
);
const libtestSummaryPattern =
  /^test result: (ok|FAILED)\. (0|[1-9]\d*) passed; (0|[1-9]\d*) failed; (0|[1-9]\d*) ignored; (0|[1-9]\d*) measured; (0|[1-9]\d*) filtered out; finished in (?:0|[1-9]\d*)(?:\.\d+)?s$/;

export class AnchoredFsScenarioError extends Error {
  constructor(code) {
    super(code);
    this.name = "AnchoredFsScenarioError";
    this.code = code;
  }
}

function fail(code) {
  throw new AnchoredFsScenarioError(code);
}

function validateContext(context) {
  if (
    context === null ||
    typeof context !== "object" ||
    context.platform !== "linux"
  ) {
    fail("unsupported_native_platform");
  }
  if (
    typeof context.repository_root !== "string" ||
    !path.isAbsolute(context.repository_root) ||
    path.normalize(context.repository_root) !== context.repository_root
  ) {
    fail("invalid_repository_root");
  }
}

function boundedCount(value) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0) {
    fail("invalid_libtest_summary");
  }
  return parsed;
}

export function parseLibtestSummary(output) {
  if (!Buffer.isBuffer(output) || output.length > maximumOutputBytes) {
    fail("invalid_libtest_summary");
  }
  let source;
  try {
    source = new TextDecoder("utf-8", { fatal: true }).decode(output);
  } catch {
    fail("invalid_libtest_summary");
  }
  const candidates = source
    .split(/\r?\n/)
    .filter((line) => line.startsWith("test result:"));
  if (candidates.length !== 1) fail("invalid_libtest_summary");
  const match = libtestSummaryPattern.exec(candidates[0]);
  if (match === null) fail("invalid_libtest_summary");
  return Object.freeze({
    test_result: match[1],
    passed: boundedCount(match[2]),
    failed: boundedCount(match[3]),
    ignored: boundedCount(match[4]),
    measured: boundedCount(match[5]),
    filtered_out: boundedCount(match[6]),
  });
}

function validateCommandResult(check, result) {
  if (
    result === null ||
    typeof result !== "object" ||
    Array.isArray(result) ||
    Object.keys(result).sort().join("\0") !== commandResultKeys.join("\0") ||
    !Number.isSafeInteger(result.exit_code) ||
    result.exit_code < 0 ||
    !["ok", "FAILED"].includes(result.test_result) ||
    !Number.isSafeInteger(result.passed) ||
    result.passed < 0 ||
    !Number.isSafeInteger(result.failed) ||
    result.failed < 0 ||
    !Number.isSafeInteger(result.ignored) ||
    result.ignored < 0 ||
    !Number.isSafeInteger(result.measured) ||
    result.measured < 0 ||
    !Number.isSafeInteger(result.filtered_out) ||
    result.filtered_out < 0 ||
    !Number.isSafeInteger(result.stdout_bytes) ||
    result.stdout_bytes < 0 ||
    result.stdout_bytes > maximumOutputBytes ||
    !Number.isSafeInteger(result.stderr_bytes) ||
    result.stderr_bytes < 0 ||
    result.stderr_bytes > maximumOutputBytes
  ) {
    fail("invalid_command_result");
  }
  if (
    result.exit_code !== 0 ||
    result.test_result !== "ok" ||
    result.failed !== 0
  ) {
    fail("command_failed");
  }
  if (
    (check.passed_requirement === "exactly-one" && result.passed !== 1) ||
    (check.passed_requirement === "nonempty" && result.passed < 1)
  ) {
    fail("test_count_mismatch");
  }
}

async function executeCargoCheck(command) {
  let output;
  try {
    output = await execFile(command.executable, command.arguments, {
      cwd: command.cwd,
      env: {
        ...process.env,
        CARGO_TERM_COLOR: "never",
        NO_COLOR: "1",
      },
      encoding: "buffer",
      timeout: command.timeout_ms,
      killSignal: "SIGTERM",
      maxBuffer: command.max_output_bytes,
      windowsHide: true,
    });
  } catch (error) {
    fail(error?.killed === true ? "command_timeout" : "command_failed");
  }
  const summary = parseLibtestSummary(output.stdout);
  return {
    exit_code: 0,
    stdout_bytes: output.stdout.length,
    stderr_bytes: output.stderr.length,
    ...summary,
  };
}

export async function runAnchoredFsScenario(context, options = {}) {
  validateContext(context);
  const commandRunner = options.commandRunner ?? executeCargoCheck;
  const monotonicNow = options.monotonicNow ?? (() => performance.now());
  const startedAt = monotonicNow();
  if (!Number.isFinite(startedAt) || startedAt < 0) {
    fail("invalid_monotonic_clock");
  }
  const deadline = startedAt + scenarioBudgetMs;
  const cargoTarget = path.join(
    context.repository_root,
    "scripts",
    "cargo-target.mjs",
  );
  const observations = [];

  for (const check of anchoredFsChecks) {
    const current = monotonicNow();
    if (!Number.isFinite(current) || current < startedAt) {
      fail("invalid_monotonic_clock");
    }
    const remaining = Math.floor(deadline - current);
    if (remaining < minimumCommandBudgetMs) {
      fail("scenario_budget_exhausted");
    }
    const result = await commandRunner(
      Object.freeze({
        executable: process.execPath,
        arguments: Object.freeze([
          cargoTarget,
          "run",
          "--",
          "cargo",
          ...check.cargo_arguments,
        ]),
        cwd: context.repository_root,
        timeout_ms: remaining,
        max_output_bytes: maximumOutputBytes,
      }),
    );
    validateCommandResult(check, result);
    observations.push({
      id: check.id,
      outcome: "pass",
      receipt: {
        schema_version: 1,
        command_id: check.id,
        cargo_arguments: [...check.cargo_arguments],
        exit_code: result.exit_code,
        test_result: result.test_result,
        passed: result.passed,
        failed: result.failed,
        ignored: result.ignored,
        measured: result.measured,
        filtered_out: result.filtered_out,
        stdout_bytes: result.stdout_bytes,
        stderr_bytes: result.stderr_bytes,
      },
    });
  }

  return {
    ok: true,
    observations,
    artifacts: [],
  };
}

export async function runScenario(context) {
  return runAnchoredFsScenario(context);
}
