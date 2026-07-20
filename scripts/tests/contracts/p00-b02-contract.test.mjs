import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import test from "node:test";

import { readToolchainIdentity } from "../../toolchain.mjs";
import { parseRepositoryWorkflows } from "./_workflow-contract.mjs";

const identity = readToolchainIdentity();
const workflows = new Map(parseRepositoryWorkflows().map((workflow) => [workflow.path, workflow]));
const ci = workflows.get(".github/workflows/ci.yml");
const release = workflows.get(".github/workflows/release.yml");

function actionSteps(repository) {
  return [...workflows.values()]
    .flatMap((workflow) => workflow.steps)
    .filter((step) => step.actionRepository === repository);
}

function runSteps(job) {
  return job.steps.map((step) => step.run).filter(Boolean);
}

test("the exact toolchain manifest owns every delivery projection", async () => {
  const packageJson = JSON.parse(await readFile("frontend/package.json", "utf8"));
  assert.equal(packageJson.engines.node, identity.node);
  assert.equal(packageJson.packageManager, `pnpm@${identity.pnpm}`);
  assert.equal(packageJson.devDependencies["@types/node"], identity.node_types);

  const rustToolchain = await readFile("rust-toolchain.toml", "utf8");
  assert.match(rustToolchain, new RegExp(`^channel = "${identity.rust.release.replaceAll(".", "\\.")}"$`, "m"));

  for (const step of actionSteps("actions/setup-node")) {
    assert.equal(step.inputs.get("node-version"), identity.node);
  }
  for (const step of actionSteps("pnpm/action-setup")) {
    assert.equal(step.inputs.get("version"), identity.pnpm);
  }
  for (const step of actionSteps("dtolnay/rust-toolchain")) {
    assert.equal(step.inputs.get("toolchain"), identity.rust.release);
  }
  for (const step of actionSteps("go-task/setup-task")) {
    assert.equal(step.inputs.get("version"), identity.task);
  }

  const taskfile = await readFile("Taskfile.yml", "utf8");
  assert.match(taskfile, new RegExp(`TAURI_CLI_VERSION: "${identity.tauri_cli.replaceAll(".", "\\.")}"`));
  assert.doesNotMatch(taskfile, /TAURI_CLI_VERSION_REQ|required_min|required_major|required_minor/);

  const ciSource = await readFile(".github/workflows/ci.yml", "utf8");
  const releaseSource = await readFile(".github/workflows/release.yml", "utf8");
  assert.match(ciSource, new RegExp(`image: ${identity.linux_ci_image.reference}`));
  assert.match(releaseSource, new RegExp(`image: ${identity.linux_ci_image.reference}`));
  assert.doesNotMatch(`${ciSource}\n${releaseSource}`, /axial-linux-ci:latest/);
  assert.match(releaseSource, new RegExp(`tauri-cli --version "=${identity.tauri_cli.replaceAll(".", "\\.")}"`));

  const dockerfile = await readFile(".github/docker/linux-ci/Dockerfile", "utf8");
  assert.equal(dockerfile.split(/\r?\n/, 1)[0], `FROM ${identity.ubuntu_base.reference}`);
  assert.match(dockerfile, new RegExp(`/ubuntu/${identity.ubuntu_apt_snapshot}/`, "g"));

  const imageWorkflow = await readFile(".github/workflows/linux-ci-image.yml", "utf8");
  assert.match(imageWorkflow, /type=sha,format=long/);
  assert.doesNotMatch(imageWorkflow, /type=raw,value=latest|type=sha\s*$/m);
  assert.match(imageWorkflow, /^\s+context: \.github\/docker\/linux-ci$/m);
  assert.match(imageWorkflow, /steps\.build\.outputs\.digest/);
});

test("CI and release invoke one Task-owned verification inventory", () => {
  assert.deepEqual([...ci.jobs.keys()].sort(), ["platform-macos", "platform-windows", "verify-linux"]);
  assert.match(ci.jobs.get("verify-linux").source, /^\s{4}runs-on: ubuntu-24\.04$/m);
  assert.match(ci.jobs.get("platform-windows").source, /^\s{4}runs-on: windows-2025$/m);
  assert.match(ci.jobs.get("platform-macos").source, /^\s{4}runs-on: macos-15$/m);

  assert.deepEqual(
    runSteps(ci.jobs.get("verify-linux")).filter((command) => command.startsWith("task ")),
    ["task toolchain:preflight", "task verify:linux"],
  );
  assert.deepEqual(runSteps(ci.jobs.get("platform-windows")), ["task verify:native:windows"]);
  assert.deepEqual(runSteps(ci.jobs.get("platform-macos")), ["task verify:native:macos"]);
  assert.ok(
    runSteps(release.jobs.get("verify")).includes("task verify:linux"),
    "release verification must use the canonical Linux inventory",
  );
  for (const job of [ci.jobs.get("verify-linux"), release.jobs.get("verify")]) {
    const preflight = job.steps.findIndex((step) => step.run === "task toolchain:preflight");
    const install = job.steps.findIndex((step) => step.run?.startsWith("pnpm --dir frontend install"));
    assert.ok(preflight >= 0 && install > preflight, `${job.id}: toolchain preflight must precede install`);
  }

  for (const job of [
    ci.jobs.get("verify-linux"),
    ci.jobs.get("platform-windows"),
    ci.jobs.get("platform-macos"),
    release.jobs.get("verify"),
  ]) {
    const commands = runSteps(job).join("\n");
    assert.doesNotMatch(commands, /cargo (?:check|clippy|test|build)|pnpm .* run (?:check|test|build|format:check)/);
  }
});

test("Task exposes the closed verification and capability entry points", async () => {
  const listed = spawnSync("task", ["--list", "--json"], { encoding: "utf8", timeout: 10_000 });
  assert.equal(listed.status, 0, listed.stderr);
  const names = new Set(JSON.parse(listed.stdout).tasks.map((entry) => entry.name));
  for (const name of [
    "toolchain:verify",
    "toolchain:preflight",
    "frontend:test",
    "verify:frontend",
    "verify:rust",
    "verify:linux",
    "verify:native:windows",
    "verify:native:macos",
    "capability:self-test",
    "capability:audit",
    "capability:run",
    "capability:platform",
  ]) {
    assert.ok(names.has(name), `missing Task entry point ${name}`);
  }

  const taskfile = await readFile("Taskfile.yml", "utf8");
  assert.doesNotMatch(taskfile, /cargo check --workspace --locked\s*\n\s*- cargo clippy/);
  assert.match(taskfile, /node scripts\/capability\.mjs run/);
  assert.match(taskfile, /SCENARIO: '\{\{\.SCENARIO \| default ""\}\}'/);
  assert.match(taskfile, /verify:contracts:\n[\s\S]*?scripts\/tests\/toolchain\.test\.mjs scripts\/tests\/capability\.test\.mjs/);
  assert.match(taskfile, /verify:contracts:\n[\s\S]*?- task: capability:audit/);
  assert.match(taskfile, /capability:self-test:\n[\s\S]*?- task: verify:contracts[\s\S]*?- task: frontend:test[\s\S]*?TEST: test\/contracts\/runner-contract\.test\.ts/);
  for (const task of ["verify:native:windows", "verify:native:macos"]) {
    const escaped = task.replaceAll(":", "\\:");
    assert.match(
      taskfile,
      new RegExp(`^  ${escaped}:\\n(?:(?: {4,}.*)?\\n)*? {6}- node --test scripts/tests/capability\\.test\\.mjs$`, "m"),
    );
  }
  assert.match(taskfile, /verify:\n[\s\S]*?- task: verify:delivery/);
  assert.match(taskfile, /verify:delivery:\n\s+internal: true/);
  assert.match(taskfile, /verify:linux:\n[\s\S]*?platforms: \[linux\][\s\S]*?- task: verify:delivery/);
  assert.match(taskfile, /corepack install --global "pnpm@\$pnpm_version"/);

  const packageJson = JSON.parse(await readFile("frontend/package.json", "utf8"));
  assert.equal(packageJson.scripts.check, undefined);
  assert.equal(packageJson.scripts["test:look-guardian"], undefined);
  await assert.rejects(access("frontend/tsconfig.test.json"), { code: "ENOENT" });
});

test("native platform contracts execute portable behavior rather than source scans", async () => {
  const containment = await readFile("core/minecraft/src/loaders/bound_processors.rs", "utf8");
  assert.doesNotMatch(containment, /Command::new\("sh"\)/);
  assert.match(containment, /CONTAINMENT_FIXTURE_TEST/);
  assert.match(containment, /contained_nonzero_cancel_and_output_limit_are_reaped/);
  assert.match(containment, /contained_successful_leader_exit_terminates_surviving_descendants/);
  assert.match(containment, /contained_tree_timeout_is_reaped/);

  const priority = await readFile("apps/api/src/execution/low_priority.rs", "utf8");
  assert.match(priority, /macos_system_low_priority_round_trip_restores_disk_policy/);
  assert.match(priority, /getiopolicy_np/);
  assert.doesNotMatch(priority, /macos_disk_policy_constants_match_the_system_abi/);
});
