import assert from "node:assert/strict";
import path from "node:path";
import process from "node:process";
import test from "node:test";

import {
  anchoredFsChecks,
  parseLibtestSummary,
  runAnchoredFsScenario,
  scenario,
} from "../capabilities/scenarios/anchored-filesystem.mjs";

const repositoryRoot = path.resolve("/tmp/axial-anchored-filesystem-capability");
const passingResult = (passed = 1, filteredOut = 0) => ({
  exit_code: 0,
  test_result: "ok",
  passed,
  failed: 0,
  ignored: 0,
  measured: 0,
  filtered_out: filteredOut,
  stdout_bytes: 1,
  stderr_bytes: 0,
});

test("anchored-filesystem capability binds the declared proof to exact serialized Cargo checks", async () => {
  const commands = [];
  const result = await runAnchoredFsScenario(
    {
      platform: "linux",
      repository_root: repositoryRoot,
    },
    {
      monotonicNow: () => 1_000,
      commandRunner: async (command) => {
        commands.push(command);
        return passingResult(commands.length === 3 ? 12 : 1, 40);
      },
    },
  );

  assert.deepEqual(scenario, {
    scenario_id: "CP-ANCHORED-FILESYSTEM",
    proof_id: "CAP-ANCHORED-FILESYSTEM",
    capability_id: "anchored-filesystem",
  });
  assert.deepEqual(
    anchoredFsChecks.map(({ cargo_arguments }) => cargo_arguments),
    [
      [
        "test",
        "--locked",
        "-p",
        "axial-api",
        "--lib",
        "--no-default-features",
        "state::managed_library::tests::anchored_filesystem_contract",
        "--",
        "--exact",
      ],
      [
        "test",
        "--locked",
        "-p",
        "axial-api",
        "--lib",
        "--no-default-features",
        "state::managed_library::tests::anchored_filesystem_contract_cross_owner",
        "--",
        "--exact",
      ],
      ["test", "--locked", "-p", "axial-fs", "--lib"],
    ],
  );
  assert.deepEqual(
    commands.map(({ executable, arguments: commandArguments, cwd }) => ({
      executable,
      arguments: commandArguments,
      cwd,
    })),
    anchoredFsChecks.map((check) => ({
      executable: process.execPath,
      arguments: [
        path.join(repositoryRoot, "scripts", "cargo-target.mjs"),
        "run",
        "--",
        "cargo",
        ...check.cargo_arguments,
      ],
      cwd: repositoryRoot,
    })),
  );
  assert.ok(
    commands.every(
      ({ timeout_ms, max_output_bytes }) =>
        timeout_ms === 285_000 && max_output_bytes === 4 * 1024 * 1024,
    ),
  );
  assert.deepEqual(
    result.observations.map(({ id, outcome, receipt }) => ({
      id,
      outcome,
      command_id: receipt.command_id,
      cargo_arguments: receipt.cargo_arguments,
      exit_code: receipt.exit_code,
      test_result: receipt.test_result,
      passed: receipt.passed,
      failed: receipt.failed,
    })),
    anchoredFsChecks.map((check, index) => {
      const passed = index === 2 ? 12 : 1;
      return {
        id: check.id,
        outcome: "pass",
        command_id: check.id,
        cargo_arguments: [...check.cargo_arguments],
        exit_code: 0,
        test_result: "ok",
        passed,
        failed: 0,
      };
    }),
  );
  assert.deepEqual(result.artifacts, []);
});

test("anchored-filesystem capability parses exactly one typed libtest summary", () => {
  assert.deepEqual(
    parseLibtestSummary(
      Buffer.from(
        "running 1 test\n" +
          "test state::managed_library::tests::anchored_filesystem_contract ... ok\n\n" +
          "test result: ok. 1 passed; 0 failed; 2 ignored; 0 measured; 44 filtered out; finished in 0.01s\n",
      ),
    ),
    {
      test_result: "ok",
      passed: 1,
      failed: 0,
      ignored: 2,
      measured: 0,
      filtered_out: 44,
    },
  );

  for (const output of [
    "running 1 test\n",
    "test result: ok. one passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n" +
      "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
  ]) {
    assert.throws(
      () => parseLibtestSummary(Buffer.from(output)),
      (error) => {
        assert.equal(error?.code, "invalid_libtest_summary");
        return true;
      },
    );
  }
});

test("anchored-filesystem capability fails closed on unsupported native platforms", async () => {
  for (const platform of ["windows", "macos"]) {
    await assert.rejects(
      runAnchoredFsScenario({
        platform,
        repository_root: repositoryRoot,
      }),
      (error) => {
        assert.equal(error?.code, "unsupported_native_platform");
        return true;
      },
    );
  }
});

test("anchored-filesystem capability rejects zero-match API and zero-test native summaries", async () => {
  const context = {
    platform: "linux",
    repository_root: repositoryRoot,
  };
  await assert.rejects(
    runAnchoredFsScenario(context, {
      monotonicNow: () => 1_000,
      commandRunner: async () => passingResult(0, 1),
    }),
    (error) => {
      assert.equal(error?.code, "test_count_mismatch");
      return true;
    },
  );

  let commandIndex = 0;
  await assert.rejects(
    runAnchoredFsScenario(context, {
      monotonicNow: () => 1_000,
      commandRunner: async () => {
        commandIndex += 1;
        return commandIndex < 3 ? passingResult() : passingResult(0);
      },
    }),
    (error) => {
      assert.equal(error?.code, "test_count_mismatch");
      return true;
    },
  );
});

test("anchored-filesystem capability rejects failed, malformed, and over-budget command evidence", async () => {
  const context = {
    platform: "linux",
    repository_root: repositoryRoot,
  };
  for (const [result, code] of [
    [{ ...passingResult(), exit_code: 1 }, "command_failed"],
    [{ ...passingResult(), test_result: "FAILED" }, "command_failed"],
    [{ ...passingResult(), failed: 1 }, "command_failed"],
    [
      { ...passingResult(), stdout_bytes: 4 * 1024 * 1024 + 1 },
      "invalid_command_result",
    ],
    [
      {
        exit_code: 0,
        stdout_bytes: 0,
        stderr_bytes: 0,
      },
      "invalid_command_result",
    ],
    [{ ...passingResult(), unexpected: true }, "invalid_command_result"],
  ]) {
    await assert.rejects(
      runAnchoredFsScenario(context, {
        monotonicNow: () => 1_000,
        commandRunner: async () => result,
      }),
      (error) => {
        assert.equal(error?.code, code);
        return true;
      },
    );
  }

  await assert.rejects(
    runAnchoredFsScenario(context, {
      monotonicNow: (() => {
        const readings = [1_000, 285_001];
        return () => readings.shift();
      })(),
      commandRunner: async () => assert.fail("runner must not execute"),
    }),
    (error) => {
      assert.equal(error?.code, "scenario_budget_exhausted");
      return true;
    },
  );
});
