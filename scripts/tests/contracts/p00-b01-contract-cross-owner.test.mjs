import assert from "node:assert/strict";
import test from "node:test";

import {
  parseRepositoryWorkflows,
  parseWorkflowScalar,
  remoteActionSteps,
} from "./_workflow-contract.mjs";

const workflows = new Map(
  parseRepositoryWorkflows().map((workflow) => [
    workflow.path.replace(/^\.github\/workflows\//, "").replace(/\.ya?ml$/, ""),
    workflow,
  ]),
);

const checkoutRepository = "actions/checkout";
const uploadRepository = "actions/upload-artifact";
const downloadRepository = "actions/download-artifact";
const releaseRepository = "softprops/action-gh-release";

test("workflow scalar parsing cannot hide commented write permissions", () => {
  assert.equal(parseWorkflowScalar('"write" # sole writer'), "write");
  assert.equal(parseWorkflowScalar("'write' # sole writer"), "write");
  assert.equal(parseWorkflowScalar('"#literal" # trailing comment'), "#literal");
  assert.equal(
    parseWorkflowScalar("${{ contains(inputs.value, '#') }}"),
    "${{ contains(inputs.value, '#') }}",
  );
});

function writePermissions() {
  const grants = [];
  for (const [workflowId, workflow] of workflows) {
    if (workflow.topPermissions.scalar === "write-all") {
      grants.push(`${workflowId}/<workflow>:write-all`);
    }
    for (const [permission, value] of workflow.topPermissions.entries) {
      if (value === "write") grants.push(`${workflowId}/<workflow>:${permission}`);
    }
    for (const job of workflow.jobs.values()) {
      if (job.permissions.scalar === "write-all") {
        grants.push(`${workflowId}/${job.id}:write-all`);
      }
      for (const [permission, value] of job.permissions.entries) {
        if (value === "write") grants.push(`${workflowId}/${job.id}:${permission}`);
      }
    }
  }
  return grants.sort();
}

test("checkout credentials are never persisted", () => {
  const checkouts = [...workflows.values()]
    .flatMap(remoteActionSteps)
    .filter((step) => step.actionRepository === checkoutRepository);
  assert.ok(checkouts.length > 0, "expected checkout actions");
  for (const checkout of checkouts) {
    assert.equal(
      checkout.inputs.get("persist-credentials"),
      "false",
      `${checkout.jobId}:${checkout.actionLine}: checkout must set persist-credentials: false`,
    );
  }
});

test("write authority belongs only to the image publisher and release publisher", () => {
  for (const [workflowId, workflow] of workflows) {
    assert.equal(
      workflow.topPermissions.scalar,
      undefined,
      `${workflowId}: workflow permissions must use an inspectable block mapping`,
    );
    assert.equal(
      workflow.topPermissions.entries.get("contents"),
      "read",
      `${workflowId}: workflow must default to contents: read`,
    );
    for (const [permission, value] of workflow.topPermissions.entries) {
      assert.notEqual(value, "write", `${workflowId}: workflow grants ${permission}: write`);
    }
    for (const job of workflow.jobs.values()) {
      assert.equal(
        job.permissions.scalar,
        undefined,
        `${workflowId}/${job.id}: job permissions must use an inspectable block mapping`,
      );
    }
  }

  assert.deepEqual(writePermissions(), [
    "linux-ci-image/publish:packages",
    "release/publish-release:contents",
  ]);

  const imagePublisher = workflows.get("linux-ci-image").jobs.get("publish");
  assert.equal(imagePublisher.permissions.entries.get("contents"), "read");
  assert.equal(imagePublisher.permissions.entries.get("packages"), "write");

  const releasePublisher = workflows.get("release").jobs.get("publish-release");
  assert.equal(releasePublisher.permissions.entries.get("contents"), "write");
});

test("one non-executing release publisher owns every platform handoff", () => {
  const release = workflows.get("release");
  const platformIds = ["linux-desktop", "windows-desktop", "macos-desktop"];
  const publisher = release.jobs.get("publish-release");
  assert.ok(publisher, "release: missing publish-release job");
  assert.deepEqual([...publisher.needs].sort(), [...platformIds].sort());

  const publisherActions = publisher.steps.filter((step) => step.action);
  assert.equal(
    publisherActions.filter((step) => step.actionRepository === checkoutRepository).length,
    0,
    "publish-release must not checkout source",
  );
  assert.equal(
    publisher.steps.filter((step) => step.run !== undefined).length,
    0,
    "publish-release must not execute shell commands before the B12 validation owner exists",
  );

  const releaseSteps = release.steps.filter(
    (step) => step.actionRepository === releaseRepository,
  );
  assert.equal(releaseSteps.length, 1, "release workflow must have exactly one public publisher");
  assert.equal(releaseSteps[0].jobId, "publish-release");
  assert.deepEqual(
    [...releaseSteps[0].inputs.keys()].sort(),
    ["files", "prerelease"],
    "the release action may only receive the staged files and prerelease policy",
  );
  const publishedPaths = (releaseSteps[0].inputs.get("files") ?? "")
    .split("\n")
    .map((path) => path.trim())
    .filter(Boolean);
  assert.ok(publishedPaths.length > 0, "publish-release has no staged files");
  for (const path of publishedPaths) {
    assert.ok(path.startsWith("dist/"), `publish-release path escapes dist: ${path}`);
    assert.doesNotMatch(path, /(?:^|\/)\.\.(?:\/|$)/, `publish-release path traverses: ${path}`);
  }

  const handoffNames = [];
  const concreteHandoffNames = [];
  const expectedHandoffPaths = new Map([
    ["linux-desktop", "dist/axial-linux-amd64-*"],
    ["windows-desktop", "dist/axial-windows-amd64-*"],
    ["macos-desktop", "dist/axial-macos-${{ matrix.arch }}-*"],
  ]);
  for (const platformId of platformIds) {
    const job = release.jobs.get(platformId);
    assert.ok(job, `release: missing ${platformId} job`);
    const uploads = job.steps.filter((step) => step.actionRepository === uploadRepository);
    assert.equal(uploads.length, 1, `${platformId}: expected one internal artifact handoff`);
    const [upload] = uploads;
    const name = upload.inputs.get("name");
    assert.ok(name?.startsWith("release-assets-"), `${platformId}: handoff name is not scoped`);
    assert.equal(
      upload.inputs.get("path"),
      expectedHandoffPaths.get(platformId),
      `${platformId}: handoff includes files outside its packaged platform assets`,
    );
    assert.equal(upload.inputs.get("if-no-files-found"), "error");
    const retention = Number(upload.inputs.get("retention-days"));
    assert.ok(retention >= 1 && retention <= 3, `${platformId}: retention must be 1-3 days`);
    handoffNames.push(name);
    if (name.includes("${{ matrix.arch }}")) {
      const architectures = [...job.source.matchAll(/^\s+arch:\s*([A-Za-z0-9_-]+)\s*$/gm)].map(
        (match) => match[1],
      );
      assert.ok(architectures.length > 0, `${platformId}: handoff uses an empty arch matrix`);
      concreteHandoffNames.push(
        ...architectures.map((architecture) =>
          name.replace("${{ matrix.arch }}", architecture),
        ),
      );
    } else {
      concreteHandoffNames.push(name);
    }
    assert.equal(
      job.steps.filter((step) => step.actionRepository === releaseRepository).length,
      0,
      `${platformId}: platform job still publishes directly`,
    );
  }
  assert.equal(new Set(handoffNames).size, platformIds.length, "platform handoffs must be unique");

  const downloads = publisher.steps.filter(
    (step) => step.actionRepository === downloadRepository,
  );
  assert.ok(downloads.length > 0, "publish-release must download platform handoffs");
  assert.deepEqual(
    publisherActions.map((step) => step.actionRepository).sort(),
    [releaseRepository, ...downloads.map(() => downloadRepository)].sort(),
    "publish-release may only download handoffs and invoke the one release action",
  );
  assert.equal(
    new Set(concreteHandoffNames).size,
    concreteHandoffNames.length,
    "expanded platform handoffs must be unique",
  );
  assert.deepEqual(
    downloads.map((download) => download.inputs.get("name")).sort(),
    [...concreteHandoffNames].sort(),
    "publish-release downloads must exactly match the platform handoff allowlist",
  );
  for (const download of downloads) {
    assert.deepEqual(
      [...download.inputs.keys()].sort(),
      ["name", "path"],
      `publish-release:${download.actionLine}: download may only select a current-run artifact by name`,
    );
    assert.equal(
      download.inputs.get("path"),
      "dist",
      `publish-release:${download.actionLine}: handoff must stage under dist`,
    );
    assert.equal(
      download.inputs.get("pattern"),
      undefined,
      "publish-release must download explicit artifact names",
    );
  }
});

test("release verification follows the publisher without write authority", () => {
  const release = workflows.get("release");
  const verifier = release.jobs.get("verify-release");
  assert.ok(verifier, "release: missing verify-release job");
  assert.deepEqual(verifier.needs, ["publish-release"]);
  assert.notEqual(verifier.permissions.scalar, "write-all");
  for (const [permission, value] of verifier.permissions.entries) {
    assert.notEqual(value, "write", `verify-release grants ${permission}: write`);
  }
});
