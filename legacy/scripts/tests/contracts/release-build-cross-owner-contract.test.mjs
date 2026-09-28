import assert from "node:assert/strict";
import { access } from "node:fs/promises";
import test from "node:test";

import {
  parseRepositoryWorkflows,
  remoteActionSteps,
} from "./_workflow-contract.mjs";

const workflows = new Map(
  parseRepositoryWorkflows().map((workflow) => [
    workflow.path.replace(/^\.github\/workflows\//, "").replace(/\.ya?ml$/, ""),
    workflow,
  ]),
);
const release = workflows.get("release");
const platformJobIds = ["linux-desktop", "windows-desktop", "macos-desktop"];
const exactHandoffs = new Map([
  ["release-publication-contract", "publication"],
  ["release-assets-linux-amd64", "handoffs/linux-amd64"],
  ["release-assets-windows-amd64", "handoffs/windows-amd64"],
  ["release-assets-macos-amd64", "handoffs/macos-amd64"],
  ["release-assets-macos-arm64", "handoffs/macos-arm64"],
]);

function stepNamed(job, name) {
  const matches = job.steps.filter((step) => step.name === name);
  assert.equal(matches.length, 1, `${job.id}: expected one '${name}' step`);
  return matches[0];
}

function stepPosition(job, step) {
  return job.steps.indexOf(step);
}

function stepSource(workflow, step) {
  return workflow.source
    .split(/\r?\n/)
    .slice(step.startLine - 1, step.endLine)
    .join("\n");
}

test("release DAG gates one writer on every platform handoff", () => {
  assert.ok(release, "missing release workflow");
  assert.deepEqual(
    [...release.jobs.keys()],
    ["verify", ...platformJobIds, "publish-release"],
  );
  assert.deepEqual(release.jobs.get("verify").needs, []);
  for (const jobId of platformJobIds) {
    const job = release.jobs.get(jobId);
    assert.deepEqual(
      job.needs,
      ["verify"],
      `${jobId}: must consume verified source`,
    );
    assert.notEqual(job.permissions.scalar, "write-all");
    for (const [permission, value] of job.permissions.entries) {
      assert.notEqual(value, "write", `${jobId}: grants ${permission}: write`);
    }
    assert.doesNotMatch(
      job.source,
      /release-contract\.mjs\s+publish\b/,
      `${jobId}: platform job must not mutate a release`,
    );
  }

  const publisher = release.jobs.get("publish-release");
  assert.deepEqual([...publisher.needs].sort(), [...platformJobIds].sort());
  assert.match(
    publisher.source,
    /^\s{4}timeout-minutes:\s*30\s*$/m,
    "credentialed publication must have a bounded outer runtime",
  );
  assert.equal(publisher.permissions.entries.get("contents"), "write");
  const workflowWriters = [...release.jobs.values()].filter(
    (job) => job.permissions.entries.get("contents") === "write",
  );
  assert.deepEqual(
    workflowWriters.map((job) => job.id),
    ["publish-release"],
  );
  assert.match(release.source, /group:\s*release-\$\{\{ github\.ref \}\}/);
  assert.match(release.source, /cancel-in-progress:\s*false/);
});

test("publisher has no source checkout and isolates every named handoff", () => {
  const publisher = release.jobs.get("publish-release");
  const actions = remoteActionSteps(release).filter(
    (step) => step.jobId === publisher.id,
  );
  const checkouts = actions.filter(
    (step) => step.actionRepository === "actions/checkout",
  );
  assert.equal(
    checkouts.length,
    0,
    "writer must not expose its token to checkout",
  );
  assert.equal(
    actions.filter((step) => step.actionRepository === "dtolnay/rust-toolchain")
      .length,
    0,
    "writer must consume the verified source receipt instead of ambient Cargo",
  );
  const node = actions.filter(
    (step) => step.actionRepository === "actions/setup-node",
  );
  assert.equal(node.length, 1);
  assert.equal(
    node[0].inputs.get("token"),
    "",
    "setup-node must not receive the write token",
  );

  const downloads = actions.filter(
    (step) => step.actionRepository === "actions/download-artifact",
  );
  assert.deepEqual(
    downloads.map((step) => step.inputs.get("name")),
    [...exactHandoffs.keys()],
  );
  for (const download of downloads) {
    assert.deepEqual([...download.inputs.keys()].sort(), ["name", "path"]);
    assert.equal(
      download.inputs.get("path"),
      exactHandoffs.get(download.inputs.get("name")),
    );
  }
  assert.equal(
    new Set(downloads.map((step) => step.inputs.get("path"))).size,
    5,
  );
});

test("read-only verification stages the exact publication contract before heavy builds", () => {
  const verifier = release.jobs.get("verify");
  const sourceVerification = stepNamed(
    verifier,
    "Stage release publication contract",
  );
  assert.equal(
    sourceVerification.run,
    'node scripts/release-contract.mjs stage-publication --tag "$GITHUB_REF_NAME" --sha "$GITHUB_SHA" --output "$RUNNER_TEMP/release-publication-contract"',
  );
  const verifyRust = verifier.steps.find(
    (step) => step.actionRepository === "dtolnay/rust-toolchain",
  );
  const toolchainPreflight = stepNamed(verifier, "Verify delivery toolchain");
  assert.ok(verifyRust, "verify: missing pinned Rust setup");
  assert.ok(
    stepPosition(verifier, verifyRust) <
      stepPosition(verifier, sourceVerification),
  );
  assert.ok(
    stepPosition(verifier, toolchainPreflight) <
      stepPosition(verifier, sourceVerification),
  );
  const uploads = verifier.steps.filter(
    (step) =>
      step.actionRepository === "actions/upload-artifact" &&
      step.inputs.get("name") === "release-publication-contract",
  );
  assert.equal(uploads.length, 1);
  const [upload] = uploads;
  assert.deepEqual([...upload.inputs.keys()].sort(), [
    "if-no-files-found",
    "name",
    "path",
    "retention-days",
  ]);
  assert.equal(
    upload.inputs.get("path"),
    "${{ runner.temp }}/release-publication-contract",
  );
  assert.equal(upload.inputs.get("if-no-files-found"), "error");
  assert.equal(upload.inputs.get("retention-days"), "1");
  assert.ok(
    stepPosition(verifier, sourceVerification) < stepPosition(verifier, upload),
  );
  assert.ok(
    stepPosition(verifier, upload) <
      stepPosition(
        verifier,
        stepNamed(verifier, "Install frontend dependencies"),
      ),
  );
});

test("one repository-owned transaction is the complete release mutation surface", () => {
  const publisher = release.jobs.get("publish-release");
  const commands = publisher.steps.filter((step) => step.run !== undefined);
  assert.equal(
    commands.length,
    1,
    "publish-release must have one executable transaction",
  );
  const [publish] = commands;
  assert.equal(publish.name, "Publish complete release");
  assert.equal(
    publish.run,
    'node publication/release-contract.mjs publish --receipt publication/source.json --tag "$GITHUB_REF_NAME" --sha "$GITHUB_SHA" --assets handoffs',
  );
  const publishSource = stepSource(release, publish);
  assert.match(
    publishSource,
    /GITHUB_TOKEN:\s*\$\{\{ secrets\.GITHUB_TOKEN \}\}/,
  );
  assert.equal(
    [...release.source.matchAll(/GITHUB_TOKEN:/g)].length,
    1,
    "only the transaction may receive the release token",
  );

  assert.equal(
    release.steps.filter(
      (step) => step.actionRepository === "softprops/action-gh-release",
    ).length,
    0,
  );
  assert.equal(
    release.steps.filter((step) =>
      /release-contract\.mjs\s+publish\b/.test(step.run ?? ""),
    ).length,
    1,
  );
  assert.doesNotMatch(release.source, /\bgh\s+(?:api|release)\b/);
  assert.doesNotMatch(release.source, /\bcurl\b[^\n]*api\.github/);
  assert.doesNotMatch(release.source, /contains\s*\(\s*github\.ref_name/);
  assert.doesNotMatch(release.source, /release-contract\.mjs\s+prepare\b/);
  assert.doesNotMatch(release.source, /--notes\b|--github-output\b/);
});

test("platform checksums and retired post-public verification stay canonical", async () => {
  const windowsPackaging = stepNamed(
    release.jobs.get("windows-desktop"),
    "Package windows desktop binary",
  );
  assert.equal(
    [...windowsPackaging.run.matchAll(/\[System\.IO\.File\]::WriteAllText\(/g)]
      .length,
    2,
  );
  assert.equal(
    [...windowsPackaging.run.matchAll(/\[System\.Text\.Encoding\]::ASCII/g)]
      .length,
    2,
  );
  assert.equal([...windowsPackaging.run.matchAll(/`n"/g)].length, 2);
  assert.doesNotMatch(windowsPackaging.run, /\bSet-Content\b|-NoNewline\b/);
  assert.equal(release.jobs.has("verify-release"), false);
  assert.doesNotMatch(release.source, /verify-release-assets\.mjs/);
  await assert.rejects(access("scripts/verify-release-assets.mjs"), {
    code: "ENOENT",
  });
});
