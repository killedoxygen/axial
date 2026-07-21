import assert from "node:assert/strict";
import { execFile as execFileCallback } from "node:child_process";
import { mkdtemp, mkdir, readFile, readdir, rm, stat, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  auditCapabilityRegistry,
  capabilityWorkerEnvironment,
  hostPlatform,
  listLinuxProcessGroup,
  livePosixGroupMembers,
  parseLinuxProcessStat,
  runCapability,
} from "../capability.mjs";
import {
  aggregateCapabilityEvidence,
  canonicalJson,
  sha256,
  validateEvidenceDocument,
} from "../capabilities/evidence.mjs";
import { readToolchainIdentity } from "../toolchain.mjs";

const SOURCE = Object.freeze({
  commit: "1".repeat(40),
  tree: "2".repeat(40),
  dirty: false,
});
const TOOLCHAIN = Object.freeze({
  manifest_sha256: "3".repeat(64),
  identity: Object.freeze({ node: "24.13.1", task: "3.52.0" }),
});
const { manifest_sha256: MANIFEST_SHA256, ...MANIFEST } = readToolchainIdentity();
const LINUX = Object.freeze({
  os: "linux",
  arch: "x64",
  runner_image_os: null,
  runner_image_version: null,
});
const execFile = promisify(execFileCallback);
const capabilityCli = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../capability.mjs");

function scenarioSource(body, declaration = {}) {
  const identity = {
    scenario_id: "CP-TEST-PASS",
    proof_id: "CAP-TEST-PASS",
    capability_id: "test-pass",
    ...declaration,
  };
  return `
export const scenario = ${JSON.stringify(identity)};
export async function runScenario(context) {
${body}
}
export async function readCurrentReceipts(context) {
  return {
    observations: context.observations.map((id) => ({
      id,
      receipt: id === "browser-executor"
        ? { engine: "chromium", version: "124.0.1" }
        : { value: "private receipt" },
    })),
  };
}
`;
}

async function harness(t, body, options = {}) {
  const root = await mkdtemp(path.join(os.tmpdir(), "axial-capability-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const scenarioRoot = path.join(root, "scripts/capabilities/scenarios");
  const modulePath = path.join(scenarioRoot, "test-pass.mjs");
  await mkdir(scenarioRoot, { recursive: true });
  if (body !== null) {
    await writeFile(modulePath, scenarioSource(body, options.declaration), "utf8");
  }
  if (options.artifact !== false) {
    await writeFile(path.join(root, "artifact.txt"), "artifact contents\n", "utf8");
  }

  const record = {
    scenario_id: "CP-TEST-PASS",
    proof_id: "CAP-TEST-PASS",
    capability_id: "test-pass",
    owner_phase: "P00",
    allowed_platforms: ["linux"],
    timeout_ms: options.timeout ?? 1_500,
    module_url: pathToFileURL(modulePath).href,
    evidence_path: "evidence/capabilities/CAP-TEST-PASS.json",
    ...options.record,
  };
  const overrides = {
    repositoryRoot: root,
    scenarioRoot,
    evidenceRoot: path.join(root, "evidence/capabilities"),
    registry: [record],
    platformHook: async () => LINUX,
    sourceHook: async () => SOURCE,
    toolchainHook: async () => TOOLCHAIN,
  };
  return { root, scenarioRoot, modulePath, record, overrides };
}

function request(changes = {}) {
  return {
    scenario: "CP-TEST-PASS",
    platform: "linux",
    capability: null,
    ...changes,
  };
}

async function rejectsCode(callback, expected) {
  await assert.rejects(callback, (error) => {
    assert.equal(error?.code, expected);
    return true;
  });
}

async function evidenceAbsent(root) {
  await assert.rejects(stat(path.join(root, "evidence/capabilities/CAP-TEST-PASS.json")), {
    code: "ENOENT",
  });
}

async function waitForPidExit(pid) {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    try {
      process.kill(pid, 0);
    } catch (error) {
      if (error.code === "ESRCH") return;
      throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail(`descendant process ${pid} survived dispatcher settlement`);
}

async function waitForLinuxPidSettlement(pid) {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    try {
      const member = parseLinuxProcessStat(pid, await readFile(`/proc/${pid}/stat`, "utf8"));
      if (livePosixGroupMembers([member]).length === 0) return;
    } catch (error) {
      if (error.code === "ENOENT") return;
      throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail(`descendant process ${pid} remained live after dispatcher settlement`);
}

const PASS_BODY = `
  if (!Object.isFrozen(context)) throw new Error("context must be frozen");
  return {
    ok: true,
    observations: [{ id: "fixture-receipt", outcome: "pass", receipt: { value: "private receipt" } }],
    artifacts: [{ id: "fixture-artifact", repo_relative_path: "artifact.txt" }],
  };
`;

test("a closed registered scenario writes dispatcher-authored canonical evidence", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  const output = await runCapability(request(), fixture.overrides);
  assert.equal(output.evidence_path, "evidence/capabilities/CAP-TEST-PASS.json");
  validateEvidenceDocument(output.evidence);

  const destination = path.join(fixture.root, output.evidence_path);
  const source = await readFile(destination, "utf8");
  assert.equal(source, canonicalJson(output.evidence));
  assert.equal(source.includes("private receipt"), false);
  assert.equal(
    output.evidence.observations[0].receipt_sha256,
    sha256(canonicalJson({ value: "private receipt" })),
  );
  assert.equal(output.evidence.artifacts[0].sha256, sha256("artifact contents\n"));
  assert.deepEqual(output.evidence.source, { commit: SOURCE.commit, tree: SOURCE.tree });
  assert.deepEqual(output.evidence.toolchain, TOOLCHAIN);
  assert.equal(output.evidence.result, "verified");

  const entries = await readdir(path.dirname(destination));
  assert.deepEqual(entries, ["CAP-TEST-PASS.json"]);
});

test("unknown and malformed requests cannot create an evidence root", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  await rejectsCode(
    () => runCapability(request({ scenario: "CP-UNKNOWN" }), { ...fixture.overrides, registry: [] }),
    "unknown_scenario",
  );
  await rejectsCode(
    () => runCapability(request({ scenario: "../../escape" }), fixture.overrides),
    "invalid_scenario_id",
  );
  await rejectsCode(
    () => runCapability(request({ platform: "freebsd" }), fixture.overrides),
    "invalid_requested_platform",
  );
  await assert.rejects(stat(path.join(fixture.root, "evidence")), { code: "ENOENT" });
});

test("the CLI accepts closed Task environment inputs and rejects mixed input authorities", async () => {
  const environment = { ...process.env, SCENARIO: "CP-UNKNOWN", PLATFORM: "linux" };
  delete environment.CAPABILITY;
  await assert.rejects(
    execFile(process.execPath, [capabilityCli, "run"], { env: environment, encoding: "utf8" }),
    (error) => {
      assert.equal(error.code, 1);
      assert.equal(error.stdout, "");
      assert.equal(error.stderr, "capability_failed:unknown_scenario\n");
      return true;
    },
  );
  await assert.rejects(
    execFile(
      process.execPath,
      [capabilityCli, "run", "--scenario", "CP-UNKNOWN", "--platform", "linux"],
      { env: environment, encoding: "utf8" },
    ),
    (error) => {
      assert.equal(error.code, 1);
      assert.equal(error.stderr, "capability_failed:conflicting_argument_source\n");
      return true;
    },
  );
});

test("the CLI audit accepts no scenario authority and audits the production registry", async () => {
  const environment = { ...process.env };
  delete environment.SCENARIO;
  delete environment.PLATFORM;
  delete environment.CAPABILITY;
  const audited = await execFile(process.execPath, [capabilityCli, "audit"], {
    env: environment,
    encoding: "utf8",
  });
  assert.equal(audited.stdout, "capability_registry_audited:5\n");
  assert.equal(audited.stderr, "");

  await assert.rejects(
    execFile(process.execPath, [capabilityCli, "audit"], {
      env: { ...environment, SCENARIO: "CP-UNKNOWN" },
      encoding: "utf8",
    }),
    (error) => {
      assert.equal(error.code, 1);
      assert.equal(error.stderr, "capability_failed:invalid_audit_input\n");
      return true;
    },
  );
});

test("workers receive only reviewed tool and platform environment fields", () => {
  const environment = capabilityWorkerEnvironment(
    { scenario_id: "CP-TEST-PASS" },
    {
      Path: "C:\\Tools",
      SystemRoot: "C:\\Windows",
      TEMP: "C:\\Temp",
      PATHEXT: ".EXE;.CMD",
      HOME: "/home/test",
      GITHUB_TOKEN: "secret",
      AXIAL_TEST_SECRET: "secret",
      NODE_OPTIONS: "--inspect",
    },
  );
  assert.deepEqual(environment, {
    Path: "C:\\Tools",
    SystemRoot: "C:\\Windows",
    TEMP: "C:\\Temp",
    PATHEXT: ".EXE;.CMD",
    HOME: "/home/test",
    AXIAL_CAPABILITY_CONTEXT: '{"scenario_id":"CP-TEST-PASS"}',
  });
  assert.equal(Object.isFrozen(environment), true);
});

test("missing and mismatched implementations fail closed", async (t) => {
  const absent = await harness(t, null);
  await rejectsCode(() => runCapability(request(), absent.overrides), "implementation_absent");
  await evidenceAbsent(absent.root);

  const noFunction = await harness(t, null);
  await writeFile(
    noFunction.modulePath,
    `export const scenario = ${JSON.stringify({ scenario_id: "CP-TEST-PASS", proof_id: "CAP-TEST-PASS", capability_id: "test-pass" })};\n`,
    "utf8",
  );
  await rejectsCode(() => runCapability(request(), noFunction.overrides), "implementation_absent");
  await evidenceAbsent(noFunction.root);

  const mismatch = await harness(t, PASS_BODY, {
    declaration: { scenario_id: "CP-WRONG" },
  });
  await rejectsCode(() => runCapability(request(), mismatch.overrides), "implementation_identity_mismatch");
  await evidenceAbsent(mismatch.root);
});

test("normal runs inspect only the selected module while the explicit audit checks all modules", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  const brokenPath = path.join(fixture.scenarioRoot, "broken.mjs");
  await writeFile(brokenPath, `throw new Error("unrelated import must not execute during a selected run");\n`, "utf8");
  const broken = {
    ...fixture.record,
    scenario_id: "CP-TEST-BROKEN",
    proof_id: "CAP-TEST-BROKEN",
    capability_id: "test-broken",
    module_url: pathToFileURL(brokenPath).href,
    evidence_path: "evidence/capabilities/CAP-TEST-BROKEN.json",
  };
  const registry = [fixture.record, broken];
  await runCapability(request(), { ...fixture.overrides, registry });
  await rejectsCode(
    () => auditCapabilityRegistry({ ...fixture.overrides, registry }),
    "implementation_load_failed",
  );
});

test("duplicate identities and path escapes invalidate the complete registry", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  await rejectsCode(
    () => runCapability(request(), { ...fixture.overrides, registry: [fixture.record, { ...fixture.record }] }),
    "duplicate_registry_identity",
  );

  await rejectsCode(
    () =>
      runCapability(request(), {
        ...fixture.overrides,
        registry: [{ ...fixture.record, evidence_path: "evidence/capabilities/../escape.json" }],
      }),
    "invalid_evidence_path",
  );

  const outsideModule = path.join(fixture.root, "outside.mjs");
  await writeFile(outsideModule, scenarioSource(PASS_BODY), "utf8");
  await rejectsCode(
    () =>
      runCapability(request(), {
        ...fixture.overrides,
        registry: [{ ...fixture.record, module_url: pathToFileURL(outsideModule).href }],
      }),
    "invalid_module_url",
  );
  await evidenceAbsent(fixture.root);
});

test("capability roots permit a shared ancestor alias but reject a module leaf symlink", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  const aliasParent = await mkdtemp(path.join(os.tmpdir(), "axial-capability-alias-"));
  t.after(() => rm(aliasParent, { recursive: true, force: true }));
  const aliasRoot = path.join(aliasParent, "repository");
  await symlink(fixture.root, aliasRoot, process.platform === "win32" ? "junction" : "dir");
  const aliasedScenarioRoot = path.join(aliasRoot, "scripts/capabilities/scenarios");
  const aliasedModule = path.join(aliasedScenarioRoot, path.basename(fixture.modulePath));
  await runCapability(request(), {
    ...fixture.overrides,
    repositoryRoot: aliasRoot,
    scenarioRoot: aliasedScenarioRoot,
    evidenceRoot: path.join(aliasRoot, "evidence/capabilities"),
    registry: [{ ...fixture.record, module_url: pathToFileURL(aliasedModule).href }],
  });

  if (process.platform !== "win32") {
    const linkedModule = path.join(fixture.scenarioRoot, "linked.mjs");
    await symlink(fixture.modulePath, linkedModule, "file");
    await rejectsCode(
      () =>
        runCapability(request(), {
          ...fixture.overrides,
          registry: [{ ...fixture.record, module_url: pathToFileURL(linkedModule).href }],
        }),
      "invalid_module_url",
    );
  }
});

test("module and evidence roots reject repository-internal symlink escapes", async (t) => {
  const moduleFixture = await harness(t, PASS_BODY);
  const outsideModules = await mkdtemp(path.join(os.tmpdir(), "axial-capability-modules-"));
  t.after(() => rm(outsideModules, { recursive: true, force: true }));
  const escapedModule = path.join(outsideModules, "escaped.mjs");
  await writeFile(escapedModule, scenarioSource(PASS_BODY), "utf8");
  const linkedModules = path.join(moduleFixture.scenarioRoot, "linked");
  await symlink(outsideModules, linkedModules, process.platform === "win32" ? "junction" : "dir");
  await rejectsCode(
    () =>
      runCapability(request(), {
        ...moduleFixture.overrides,
        registry: [
          {
            ...moduleFixture.record,
            module_url: pathToFileURL(path.join(linkedModules, "escaped.mjs")).href,
          },
        ],
      }),
    "invalid_module_url",
  );
  await evidenceAbsent(moduleFixture.root);

  const evidenceFixture = await harness(t, PASS_BODY);
  const outsideEvidence = await mkdtemp(path.join(os.tmpdir(), "axial-capability-evidence-"));
  t.after(() => rm(outsideEvidence, { recursive: true, force: true }));
  await symlink(
    outsideEvidence,
    path.join(evidenceFixture.root, "evidence"),
    process.platform === "win32" ? "junction" : "dir",
  );
  await rejectsCode(
    () => runCapability(request(), evidenceFixture.overrides),
    "unsafe_evidence_root",
  );
  assert.deepEqual(await readdir(outsideEvidence), []);
});

test("capability and concrete platform bindings are checked before execution", async (t) => {
  const sentinelBody = `
    const { writeFile } = await import("node:fs/promises");
    await writeFile(new URL("../../../sentinel", import.meta.url), "ran");
    ${PASS_BODY}
  `;
  const fixture = await harness(t, sentinelBody);
  await rejectsCode(
    () => runCapability(request({ capability: "wrong-capability" }), fixture.overrides),
    "capability_mismatch",
  );
  await rejectsCode(
    () =>
      runCapability(request(), {
        ...fixture.overrides,
        platformHook: async () => ({ ...LINUX, os: "windows" }),
      }),
    "platform_mismatch",
  );
  await rejectsCode(
    () => runCapability(request({ platform: "matrix" }), fixture.overrides),
    "matrix_not_required",
  );
  await assert.rejects(stat(path.join(fixture.root, "sentinel")), { code: "ENOENT" });
  await evidenceAbsent(fixture.root);
});

test("browser execution requires a trusted executor receipt and platform scenarios remain executable", async (t) => {
  assert.notEqual(hostPlatform().os, "browser");
  const browser = await harness(
    t,
    `return {
      ok: true,
      observations: [
        { id: "browser-executor", outcome: "pass", receipt: { engine: "chromium", version: "124.0.1" } },
        { id: "fixture-receipt", outcome: "pass", receipt: { value: "private receipt" } },
      ],
      artifacts: [{ id: "fixture-artifact", repo_relative_path: "artifact.txt" }],
    };`,
    { record: { allowed_platforms: ["browser"] } },
  );
  await rejectsCode(
    () => runCapability(request({ platform: "browser" }), browser.overrides),
    "browser_executor_unavailable",
  );
  const browserOutput = await runCapability(request({ platform: "browser" }), {
    ...browser.overrides,
    browserExecutorHook: async () => ({ engine: "chromium", version: "124.0.1" }),
  });
  assert.equal(browserOutput.evidence.platform.os, "browser");

  const platformScenario = await harness(t, PASS_BODY, {
    declaration: {
      scenario_id: "PM-TEST-LINUX",
      proof_id: "PM-TEST-LINUX",
    },
    record: {
      scenario_id: "PM-TEST-LINUX",
      proof_id: "PM-TEST-LINUX",
      evidence_path: "evidence/platform/PM-TEST-LINUX.json",
    },
  });
  const output = await runCapability(
    { scenario: "PM-TEST-LINUX", platform: "linux", capability: "test-pass" },
    platformScenario.overrides,
  );
  assert.equal(output.evidence_path, "evidence/platform/PM-TEST-LINUX.json");
});

test("dirty source and malformed tool identity cannot produce evidence", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  await rejectsCode(
    () =>
      runCapability(request(), {
        ...fixture.overrides,
        sourceHook: async () => ({ ...SOURCE, dirty: true }),
      }),
    "dirty_source",
  );
  await rejectsCode(
    () =>
      runCapability(request(), {
        ...fixture.overrides,
        toolchainHook: async () => ({ manifest_sha256: "not-a-hash", identity: {} }),
      }),
    "invalid_toolchain_identity",
  );
  await evidenceAbsent(fixture.root);
});

test("explicit failure and malformed dispatcher inputs leave no verified claim", async (t) => {
  const failed = await harness(t, `return { ok: false };`);
  await rejectsCode(() => runCapability(request(), failed.overrides), "scenario_failed");
  await evidenceAbsent(failed.root);

  const threw = await harness(t, `throw new Error("fixture failure");`);
  await rejectsCode(() => runCapability(request(), threw.overrides), "scenario_failed");
  await evidenceAbsent(threw.root);

  const injected = await harness(
    t,
    `return {
      ok: true,
      result: "verified",
      evidence_path: "/tmp/forged.json",
      source: {},
      toolchain: {},
      observations: [],
      artifacts: [],
    };`,
  );
  await rejectsCode(() => runCapability(request(), injected.overrides), "malformed_scenario_result");
  await evidenceAbsent(injected.root);

  const malformedReceipt = await harness(
    t,
    `let receipt = {}; for (let index = 0; index < 20; index += 1) receipt = { nested: receipt };
     return { ok: true, observations: [{ id: "receipt", outcome: "pass", receipt }], artifacts: [] };`,
  );
  await rejectsCode(() => runCapability(request(), malformedReceipt.overrides), "invalid_json_depth");
  await evidenceAbsent(malformedReceipt.root);
});

test("artifact paths cannot escape the repository or traverse symlinks", async (t) => {
  const escaped = await harness(
    t,
    `return { ok: true, observations: [{ id: "receipt", outcome: "pass", receipt: true }], artifacts: [{ id: "bad", repo_relative_path: "../outside" }] };`,
  );
  await rejectsCode(() => runCapability(request(), escaped.overrides), "invalid_artifact_path");
  await evidenceAbsent(escaped.root);

  const linked = await harness(
    t,
    `return { ok: true, observations: [{ id: "receipt", outcome: "pass", receipt: true }], artifacts: [{ id: "bad", repo_relative_path: "linked/secret.txt" }] };`,
  );
  const outside = await mkdtemp(path.join(os.tmpdir(), "axial-capability-outside-"));
  await writeFile(path.join(outside, "secret.txt"), "outside", "utf8");
  t.after(() => rm(outside, { recursive: true, force: true }));
  const { symlink } = await import("node:fs/promises");
  await symlink(outside, path.join(linked.root, "linked"), process.platform === "win32" ? "junction" : "dir");
  await rejectsCode(() => runCapability(request(), linked.overrides), "invalid_artifact_path");
  await evidenceAbsent(linked.root);
});

test("timeouts settle the worker and remove stale or temporary evidence", async (t) => {
  const fixture = await harness(t, `await new Promise((resolve) => setTimeout(resolve, 60_000));`, { timeout: 200 });
  const destination = path.join(fixture.root, fixture.record.evidence_path);
  await mkdir(path.dirname(destination), { recursive: true });
  await writeFile(destination, '{"result":"verified","stale":true}\n', "utf8");
  await writeFile(path.join(path.dirname(destination), ".CAP-TEST-PASS.json.1.1.tmp"), "stale", "utf8");

  const started = Date.now();
  await rejectsCode(() => runCapability(request(), fixture.overrides), "scenario_timeout");
  assert.ok(Date.now() - started < 5_000);
  await evidenceAbsent(fixture.root);
  assert.deepEqual(await readdir(path.dirname(destination)), []);
});

test("POSIX zombie classification and Linux process-group inspection are exact", async (t) => {
  const members = [
    { pid: 11, processGroup: 7, state: "Z+" },
    { pid: 12, processGroup: 7, state: "R" },
    { pid: 13, processGroup: 7, state: "T" },
  ];
  assert.deepEqual(livePosixGroupMembers([]), []);
  assert.deepEqual(
    livePosixGroupMembers([{ pid: 10, processGroup: 7, state: "Z" }, members[0]]),
    [],
  );
  assert.deepEqual(livePosixGroupMembers(members).map(({ pid }) => pid), [12, 13]);
  assert.throws(() => livePosixGroupMembers([{ pid: 14, processGroup: 7, state: "" }]));
  assert.deepEqual(parseLinuxProcessStat(21, "21 (name ) with spaces) S 1 7 7 0"), {
    pid: 21,
    processGroup: 7,
    state: "S",
  });

  const procRoot = await mkdtemp(path.join(os.tmpdir(), "axial-fake-proc-"));
  t.after(() => rm(procRoot, { recursive: true, force: true }));
  for (const [pid, source] of [
    [2, "2 (kernel) S 1 0 0 0"],
    [21, "21 (leader) S 1 7 7 0"],
    [22, "22 (reparented) R 1 7 7 0"],
    [30, "30 (other) S 1 9 9 0"],
  ]) {
    await mkdir(path.join(procRoot, String(pid)));
    await writeFile(path.join(procRoot, String(pid), "stat"), source, "utf8");
  }
  await mkdir(path.join(procRoot, "23"));
  assert.deepEqual((await listLinuxProcessGroup(7, procRoot)).map(({ pid }) => pid), [21, 22]);
  await mkdir(path.join(procRoot, "31"));
  await writeFile(path.join(procRoot, "31/stat"), "malformed", "utf8");
  await assert.rejects(() => listLinuxProcessGroup(7, procRoot));
});

test("successful scenarios settle a reparented live same-PGID process", { skip: process.platform !== "linux" }, async (t) => {
  const fixture = await harness(t, `
    const { spawn } = await import("node:child_process");
    const { readFile } = await import("node:fs/promises");
    const path = await import("node:path");
    const pidPath = path.join(context.repository_root, "reparented.pid");
    const heartbeatPath = path.join(context.repository_root, "reparented.alive");
    const launcher = spawn(process.execPath, ["-e", ${JSON.stringify(`
      const { spawn } = require("node:child_process");
      const fs = require("node:fs");
      const child = spawn(process.execPath, ["-e", ${JSON.stringify(`
        const fs = require("node:fs");
        process.on("SIGTERM", () => {});
        setInterval(() => fs.appendFileSync(process.argv[1], "x"), 20);
      `)}, process.argv[2]], { stdio: "ignore" });
      fs.writeFileSync(process.argv[1], String(child.pid));
      child.unref();
    `)}, pidPath, heartbeatPath], { stdio: "ignore" });
    const launcherCode = await new Promise((resolve, reject) => {
      launcher.once("error", reject);
      launcher.once("exit", (code, signal) => signal ? reject(new Error(signal)) : resolve(code));
    });
    if (launcherCode !== 0) throw new Error("reparenting launcher failed");
    const descendantPid = Number(await readFile(pidPath, "utf8"));
    const stat = await readFile(\`/proc/\${descendantPid}/stat\`, "utf8");
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
    if (Number(fields[1]) === launcher.pid || Number(fields[2]) !== process.pid) {
      throw new Error("descendant was not reparented inside the worker process group");
    }
    let heartbeatObserved = false;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      try {
        heartbeatObserved = (await readFile(heartbeatPath, "utf8")).length > 0;
        if (heartbeatObserved) break;
      } catch {}
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    if (!heartbeatObserved) throw new Error("reparented descendant never became live");
    ${PASS_BODY}
  `, { timeout: 2_500 });
  await runCapability(request(), fixture.overrides);
  await waitForLinuxPidSettlement(Number(await readFile(path.join(fixture.root, "reparented.pid"), "utf8")));
});

test("successful and timed-out scenarios cannot leak descendant processes", async (t) => {
  const descendantSetup = `
    const { spawn } = await import("node:child_process");
    const { readFile } = await import("node:fs/promises");
    const path = await import("node:path");
    const pidPath = path.join(context.repository_root, "descendant.pid");
    const descendant = spawn(process.execPath, ["-e", ${JSON.stringify(`
      const fs = require("node:fs");
      fs.writeFileSync(process.argv[1], String(process.pid));
      if (process.platform !== "win32") process.on("SIGTERM", () => {});
      setInterval(() => fs.appendFileSync(process.argv[1] + ".alive", "x"), 20);
    `)}, pidPath], { stdio: "ignore", windowsHide: true });
    for (let attempt = 0; attempt < 50; attempt += 1) {
      try { await readFile(pidPath, "utf8"); break; } catch { await new Promise((resolve) => setTimeout(resolve, 10)); }
    }
  `;

  const successful = await harness(t, `${descendantSetup}\n${PASS_BODY}`, { timeout: 1_500 });
  await runCapability(request(), successful.overrides);
  const successfulPid = Number(await readFile(path.join(successful.root, "descendant.pid"), "utf8"));
  await waitForPidExit(successfulPid);

  const failed = await harness(t, `${descendantSetup}\nreturn { ok: false };`, { timeout: 1_500 });
  await rejectsCode(() => runCapability(request(), failed.overrides), "scenario_failed");
  const failedPid = Number(await readFile(path.join(failed.root, "descendant.pid"), "utf8"));
  await waitForPidExit(failedPid);
  await evidenceAbsent(failed.root);

  const prematureExit = await harness(t, `${descendantSetup}\nprocess.exit(7);`, { timeout: 1_500 });
  await rejectsCode(() => runCapability(request(), prematureExit.overrides), "scenario_failed");
  const prematurePid = Number(await readFile(path.join(prematureExit.root, "descendant.pid"), "utf8"));
  await waitForPidExit(prematurePid);
  await evidenceAbsent(prematureExit.root);

  const timedOut = await harness(
    t,
    `${descendantSetup}\nawait new Promise((resolve) => setTimeout(resolve, 60_000));`,
    { timeout: 400 },
  );
  await rejectsCode(() => runCapability(request(), timedOut.overrides), "scenario_timeout");
  const timedOutPid = Number(await readFile(path.join(timedOut.root, "descendant.pid"), "utf8"));
  await waitForPidExit(timedOutPid);
  await evidenceAbsent(timedOut.root);
});

test("matrix execution joins only current registry-bound platform evidence", async (t) => {
  const fixture = await harness(t, `return {
    ok: true,
    observations: [
      { id: "browser-executor", outcome: "pass", receipt: { engine: "chromium", version: "124.0.1" } },
      { id: "fixture-receipt", outcome: "pass", receipt: { value: "private receipt" } },
    ],
    artifacts: [{ id: "fixture-artifact", repo_relative_path: "artifact.txt" }],
  };`, {
    record: { allowed_platforms: ["linux", "browser"] },
  });
  const platformHook = async (selected) => ({
    ...LINUX,
    os: selected === "browser" ? "browser" : "linux",
  });
  const observedToolchain = (profile) => ({
    manifest_sha256: MANIFEST_SHA256,
    identity: {
      manifest: MANIFEST,
      profiles: [profile],
      mirrors: {
        frontend_package: {
          node: MANIFEST.node,
          node_types: MANIFEST.node_types,
          pnpm: `pnpm@${MANIFEST.pnpm}`,
        },
        ...(profile === "desktop"
          ? {
              rust_toolchain: {
                channel: MANIFEST.rust.release,
                profile: "minimal",
                components: ["clippy", "rustfmt"],
              },
            }
          : {}),
      },
      executables: {
        node: { release: MANIFEST.node },
        pnpm: { release: MANIFEST.pnpm },
        task: { release: MANIFEST.task },
        ...(profile === "desktop"
          ? {
              cargo: { release: MANIFEST.rust.release, commit: MANIFEST.rust.cargo_commit },
              rustc: { release: MANIFEST.rust.release, commit: MANIFEST.rust.rustc_commit },
              tauri_cli: { release: MANIFEST.tauri_cli },
            }
          : {}),
      },
    },
  });
  const overrides = {
    ...fixture.overrides,
    platformHook,
    browserExecutorHook: async () => ({ engine: "chromium", version: "124.0.1" }),
    toolchainHook: async (_root, platform) =>
      observedToolchain(platform.os === "browser" ? "frontend" : "desktop"),
    matrixManifestHook: async () => ({ manifest_sha256: MANIFEST_SHA256, identity: MANIFEST }),
  };
  await runCapability(request(), overrides);
  await runCapability(request({ platform: "browser" }), overrides);

  const aggregate = await runCapability(request({ platform: "matrix" }), overrides);
  assert.equal(aggregate.evidence_path, "evidence/capabilities/CAP-TEST-PASS.json");
  assert.deepEqual(aggregate.evidence.platforms, ["browser", "linux"]);
  assert.deepEqual(Object.keys(aggregate.evidence.evidence_sha256).sort(), ["browser", "linux"]);

  await writeFile(path.join(fixture.root, "artifact.txt"), "mutated artifact\n", "utf8");
  await rejectsCode(
    () =>
      runCapability(request({ platform: "matrix" }), overrides),
    "artifact_evidence_mismatch",
  );
  await evidenceAbsent(fixture.root);
});

test("a failed rerun invalidates a previously verified canonical record", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  await runCapability(request(), fixture.overrides);
  const destination = path.join(fixture.root, fixture.record.evidence_path);
  assert.equal((await stat(destination)).isFile(), true);

  await writeFile(fixture.modulePath, scenarioSource(`return { ok: false };`), "utf8");
  await rejectsCode(() => runCapability(request(), fixture.overrides), "scenario_failed");
  await evidenceAbsent(fixture.root);
});

test("an evidence writer failure cannot leave a canonical or temporary claim", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  await rejectsCode(
    () =>
      runCapability(request(), {
        ...fixture.overrides,
        evidenceWriter: async (destination) => {
          await mkdir(path.dirname(destination), { recursive: true });
          await writeFile(destination, "partial", "utf8");
          throw new Error("simulated write failure");
        },
      }),
    "evidence_write_failed",
  );
  await evidenceAbsent(fixture.root);
});

test("matrix aggregation rejects mixed commits and toolchain manifests", async (t) => {
  const fixture = await harness(t, PASS_BODY);
  const linux = (await runCapability(request(), fixture.overrides)).evidence;
  const windows = structuredClone(linux);
  windows.platform.os = "windows";
  const expected = {
    record: {
      scenario_id: linux.scenario_id,
      proof_id: linux.proof_id,
      capability_id: linux.capability_id,
      owner_phase: linux.owner_phase,
      allowed_platforms: ["linux", "windows"],
    },
    required_platforms: ["linux", "windows"],
    repository_root: fixture.root,
    source: linux.source,
    manifest_sha256: linux.toolchain.manifest_sha256,
    manifest_identity: linux.toolchain.identity,
    tool_identity_validator: async () => {},
    receipt_provider: async () => ({ value: "private receipt" }),
  };
  assert.deepEqual((await aggregateCapabilityEvidence([linux, windows], expected)).platforms, ["linux", "windows"]);

  const mixedCommit = structuredClone(windows);
  mixedCommit.source.commit = "4".repeat(40);
  await rejectsCode(
    async () => aggregateCapabilityEvidence([linux, mixedCommit], expected),
    "mixed_source_identity",
  );

  const mixedManifest = structuredClone(windows);
  mixedManifest.toolchain.manifest_sha256 = "5".repeat(64);
  await rejectsCode(
    async () => aggregateCapabilityEvidence([linux, mixedManifest], expected),
    "mixed_toolchain_identity",
  );

  const editedReceipt = structuredClone(windows);
  editedReceipt.observations[0].receipt_sha256 = "6".repeat(64);
  await rejectsCode(
    async () => aggregateCapabilityEvidence([linux, editedReceipt], expected),
    "receipt_evidence_mismatch",
  );

  const wrongOwner = structuredClone(windows);
  wrongOwner.owner_phase = "P01";
  await rejectsCode(
    async () => aggregateCapabilityEvidence([linux, wrongOwner], expected),
    "mixed_evidence_identity",
  );
});
