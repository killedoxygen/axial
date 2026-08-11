import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const read = (path) => readFile(new URL(path, repository), "utf8");

async function exists(path) {
  try {
    await access(new URL(path, repository));
    return true;
  } catch {
    return false;
  }
}

function braceBlock(source, marker) {
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing ${marker}`);
  const brace = source.indexOf("{", start);
  assert.notEqual(brace, -1, `missing body for ${marker}`);
  let depth = 0;
  for (let index = brace; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    if (source[index] === "}") depth -= 1;
    if (depth === 0) return source.slice(start, index + 1);
  }
  assert.fail(`unterminated ${marker}`);
}

function ordered(source, markers) {
  let previous = -1;
  for (const marker of markers) {
    const index = source.indexOf(marker, previous + 1);
    assert.notEqual(index, -1, `missing ordered marker: ${marker}`);
    assert.ok(index > previous, `marker is out of order: ${marker}`);
    previous = index;
  }
}

test("Performance transfers use the neutral retained-authority primitive", async () => {
  const [module, transfer, model] = await Promise.all([
    read("core/minecraft/src/download/mod.rs"),
    read("core/minecraft/src/download/transient_transfer.rs"),
    read("core/minecraft/src/download/model.rs"),
  ]);
  assert.equal(
    await exists("core/minecraft/src/download/content_transfer.rs"),
    false,
  );
  assert.doesNotMatch(module, /mod content_transfer/);
  assert.doesNotMatch(model, /VerifiedContentIntegrity|ExecutionDownloadReport/);
  const authority = braceBlock(transfer, "impl ManagedTransferAuthority");
  assert.match(authority, /retain_with_effect_settlement/);
  assert.match(authority, /ManagedTransferEffectAuthority/);
  const unsettled = braceBlock(transfer, "impl TransferUnsettledObligation");
  ordered(unsettled, [
    "require_transfer_effects_settled()",
    "ManagedTransferTerminalAuthority::new(self.authority)",
  ]);
});

test("Performance admits durable candidates before network and live effects", async () => {
  const [artifact, mutation] = await Promise.all([
    read("core/performance/src/install/artifact.rs"),
    read("core/performance/src/install/mutation.rs"),
  ]);
  const prepare = braceBlock(artifact, "fn prepare_transfer_targets");
  ordered(prepare, [
    "prepare_managed_artifact_candidate",
    ".admit_transient_destination",
    "ManagedTransferAuthority::retain_with_effect_settlement",
  ]);
  const stage = braceBlock(artifact, "pub(super) async fn stage_managed_graph");
  ordered(stage, [
    "prepare_transfer_targets",
    "resolver.client",
    "FuturesUnordered::new()",
    "start_next_transfer",
    "cancellation.cancel()",
    "publish_transfer_candidates",
  ]);
  const install = braceBlock(mutation, "pub(super) async fn ensure_installed");
  ordered(install, [
    "stage_managed_graph",
    "save_rollback_snapshot_async",
    "before_target_effect().await",
    "commit_staged_graph",
  ]);
  assert.ok(
    install.match(/settle_pre_target_candidates/g)?.length >= 3,
    "every post-transfer pre-target failure must settle durable candidates",
  );
  const cleanup = braceBlock(mutation, "async fn settle_pre_target_candidates");
  ordered(cleanup, ["drop(staged)", "reconcile_managed_storage"]);
});

test("Application pins exact public origins and owns bounded retries", async () => {
  const [network, workflow] = await Promise.all([
    read("apps/api/src/application/transfer.rs"),
    read("apps/api/src/application/performance/workflow/mutation.rs"),
  ]);
  const pin = braceBlock(network, "pub(crate) async fn pinned_public_transfer_client");
  ordered(pin, [
    "tokio::time::timeout",
    "tokio::net::lookup_host",
    "PinnedTransferOrigin::public",
    "bounded_unique_addresses",
    "TransferClientConfig::bounded_pinned_public",
  ]);
  const retry = braceBlock(network, "fn transfer_retryable");
  assert.match(retry, /Network/);
  assert.match(retry, /408 \| 429 \| 500\.\.=599/);
  const resolver = braceBlock(workflow, "fn performance_artifact_transfer_resolver");
  ordered(resolver, [
    "TransferOrigin::from_url",
    "clients.get(&origin)",
    "pinned_public_transfer_client",
    "clients.insert(origin",
    "managed_transfer_retry_policy()",
  ]);
});

test("Candidate recovery retains ready bytes and removes only pre-ready work", async () => {
  const [state, storage] = await Promise.all([
    read("core/performance/src/state/mod.rs"),
    read("core/performance/src/storage.rs"),
  ]);
  const candidate = braceBlock(state, "pub(crate) fn prepare_managed_artifact_candidate");
  ordered(candidate, [
    "create_file_create_new",
    "intents.sync()",
    "ManagedArtifactCandidateIntent",
  ]);
  const reconcile = braceBlock(state, "pub(crate) fn reconcile_managed_candidate_intents");
  ordered(reconcile, [
    "addition_marker_relative",
    "retained_candidates.insert",
    "quarantine_remove_exact",
  ]);
  assert.match(reconcile, /managed artifact candidate has no exact ready intent/);
  const continuation = braceBlock(storage, "enum ManagedEffectContinuation");
  assert.match(continuation, /ArtifactTransfer/);
  const settlement = braceBlock(storage, "impl ManagedTransferEffectAuthority");
  assert.match(settlement, /self\.require_settled\(\)/);
});
