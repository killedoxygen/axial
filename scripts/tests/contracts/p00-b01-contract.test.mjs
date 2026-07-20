import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import test from "node:test";

import {
  parseRepositoryWorkflows,
  readRepositorySource,
  remoteActionReferences,
} from "./_workflow-contract.mjs";

const workflows = parseRepositoryWorkflows();

const acceptedActions = new Map([
  ["actions/checkout", ["34e114876b0b11c390a56381ad16ebd13914f8d5", "v4.3.1"]],
  ["pnpm/action-setup", ["b906affcce14559ad1aafd4ab0e942779e9f58b1", "v4.3.0"]],
  ["actions/setup-node", ["49933ea5288caeca8642d1e84afbd3f7d6820020", "v4.4.0"]],
  ["dtolnay/rust-toolchain", ["4cda84d5c5c54efe2404f9d843567869ab1699d4", "stable"]],
  ["Swatinem/rust-cache", ["c19371144df3bb44fab255c43d04cbc2ab54d1c4", "v2.9.1"]],
  ["actions/upload-artifact", ["ea165f8d65b6e75b540449e92b4886f43607fa02", "v4.6.2"]],
  ["actions/download-artifact", ["d3f86a106a0bac45b974a628896c90dbdf5c8093", "v4.3.0"]],
  ["softprops/action-gh-release", ["3bb12739c298aeb8a4eeaf626c5b8d85266b0e65", "v2.6.2"]],
  ["docker/setup-buildx-action", ["8d2750c68a42422c14e847fe6c8ac0403b4cbd6f", "v3.12.0"]],
  ["docker/login-action", ["c94ce9fb468520275223c153574b00df6fe4bcc9", "v3.7.0"]],
  ["docker/metadata-action", ["c299e40c65443455700f0fdfc63efafe5b349051", "v5.10.0"]],
  ["docker/build-push-action", ["10e90e3645eae34f1e60eeb005ba3a3d33f178e8", "v6.19.2"]],
]);

test("repository identity is Axial and obsolete migration residue is absent", async () => {
  await assert.rejects(access(".codex"), { code: "ENOENT" });

  for (const [path, heading] of [
    ["AGENTS.md", "# Agents for Axial"],
    ["CLAUDE.md", "# Claude Code for Axial"],
  ]) {
    const source = await readFile(path, "utf8");
    assert.equal(source.split(/\r?\n/, 1)[0], heading, `${path}: stale product heading`);
    assert.match(
      source,
      /\[docs\/CONVENTIONS\.md\]\(docs\/CONVENTIONS\.md\)/,
      `${path}: direct conventions link is missing`,
    );
  }

  assert.doesNotMatch(
    readRepositorySource(".github/workflows/ci.yml"),
    /\brewrite-in-rust\b/,
    ".github/workflows/ci.yml: stale rewrite branch trigger remains",
  );
});

test("every remote workflow action uses the exact reviewed immutable identity", () => {
  const occurrences = workflows.flatMap(remoteActionReferences);
  assert.ok(occurrences.length > 0, "expected remote workflow actions");

  for (const step of occurrences) {
    const accepted = acceptedActions.get(step.actionRepository);
    assert.ok(
      accepted,
      `${step.actionRepository}@${step.actionRef}: action repository is not in the accepted map`,
    );
    const [commit, versionComment] = accepted;
    assert.match(
      step.actionRef,
      /^[0-9a-f]{40}$/,
      `${step.actionRepository}@${step.actionRef}: action ref is mutable`,
    );
    assert.equal(
      step.actionRef,
      commit,
      `${step.actionRepository}: action commit differs from the reviewed identity`,
    );
    assert.equal(
      step.actionComment,
      versionComment,
      `${step.actionRepository}@${commit}: expected version comment ${versionComment}`,
    );
  }

  for (const repository of acceptedActions.keys()) {
    assert.ok(
      occurrences.some((step) => step.actionRepository === repository),
      `${repository}: accepted action has no workflow owner`,
    );
  }
});

test("Dependabot proposes bounded weekly GitHub Actions updates", () => {
  const source = readRepositorySource(".github/dependabot.yml");
  const lines = source.split(/\r?\n/);
  assert.equal(lines[0], "version: 2");
  assert.equal(lines[1], "updates:");
  let updatesEnd = 2;
  while (
    updatesEnd < lines.length &&
    (lines[updatesEnd].trim() === "" || lines[updatesEnd].startsWith("  "))
  ) {
    updatesEnd += 1;
  }
  const updatesSource = lines.slice(2, updatesEnd).join("\n");
  const updateBlocks = updatesSource.split(/(?=^  - package-ecosystem:)/m).filter(Boolean);
  const actionBlocks = updateBlocks.filter((block) =>
    /^  - package-ecosystem:\s*["']?github-actions["']?\s*$/m.test(block),
  );
  assert.equal(actionBlocks.length, 1, "expected exactly one GitHub Actions update owner");
  const [actionBlock] = actionBlocks;
  assert.match(actionBlock, /^    directory:\s*["']?\/["']?\s*$/m);
  assert.match(actionBlock, /^      interval:\s*["']?weekly["']?\s*$/m);
  assert.match(actionBlock, /^      prefix:\s*["']?chore\(ci\)["']?\s*$/m);

  const limit = actionBlock.match(/^    open-pull-requests-limit:\s*(\d+)\s*$/m);
  assert.ok(limit, ".github/dependabot.yml: missing open-pull-requests-limit");
  assert.ok(
    Number(limit[1]) >= 1 && Number(limit[1]) <= 5,
    ".github/dependabot.yml: GitHub Actions PR limit must remain between 1 and 5",
  );
});
