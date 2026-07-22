import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const read = (path) => readFile(new URL(path, repository), "utf8");

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

test("instance deletion uses strict move-only transaction phases", async () => {
  const source = await read("apps/api/src/state/instance_registry.rs");

  for (const carrier of [
    "PreparedInstanceDeletion",
    "InstanceDeletionPreparationRetry",
    "InstanceDeletionPersistenceRetry",
    "CommittedInstanceDeletion",
    "InstanceDeletionSettlementRetry",
    "InstanceDeletionMarkerClearRetry",
  ]) {
    assert.match(source, new RegExp(`#\\[must_use[\\s\\S]*?struct ${carrier} \\{`));
  }
  assert.match(
    source,
    /impl Drop for InstanceDeletionDropGuard[\s\S]*?if self\.armed[\s\S]*?std::process::abort\(\)/,
  );
  assert.match(
    braceBlock(source, "pub(super) async fn persist"),
    /ticket\.persisted\(\)\.await[\s\S]*?\.visible = self\.candidate/,
  );
  assert.match(source, /InstanceDeletionPersistenceFailure::PreAcceptance/);
  assert.match(source, /InstanceDeletionPersistenceFailure::Retryable/);
  assert.match(source, /InstanceDeletionSettlementFailure::Marker/);
  assert.match(source, /InstanceDeletionMarkerWrite::Retry \{ revision \}/);
});

test("startup validates one complete portable topology before recovery", async () => {
  const source = await read("apps/api/src/state/instance_registry.rs");
  const proof = braceBlock(source, "fn prove_instance_directory_topology");
  const startup = braceBlock(
    source,
    "pub(super) async fn prepare_startup_deletion_recovery_with_gate",
  );

  assert.match(proof, /entries\(MAX_DIRECTORY_LIST_ENTRIES\)/);
  assert.match(proof, /DirectoryListingState::Complete/);
  assert.match(proof, /leaf_name_equivalence_keys/);
  assert.match(proof, /allowed_bindings/);
  assert.doesNotMatch(proof, /for allowed in &allowed_tombstones/);
  assert.match(proof, /EntryKind::Directory/);
  assert.match(proof, /non-Unicode sibling name/);
  assert.match(proof, /unrecognized tombstone sibling/);
  assert.match(proof, /more than one recognized tombstone/);
  assert.match(startup, /for instance in &snapshot_for_probe\.instances/);
  assert.match(startup, /for record in &snapshot_for_probe\.pending_deletions/);
  assert.match(startup, /live instance has both canonical and tombstone directories/);
  assert.match(startup, /pending instance deletion still has a canonical directory/);
  assert.match(startup, /InstanceDeletionStartupRecovery::RestoreLive/);
  assert.match(startup, /InstanceDeletionStartupRecovery::CompletePending/);
  assert.match(startup, /PreparedInstanceDeletionFiles::Removed\(record\)/);
});

test("delete-files owns exact park and marker settlement while keep-files is marker-free", async () => {
  const source = await read("apps/api/src/state/instance_registry.rs");
  const prepare = braceBlock(
    source,
    "pub(super) async fn prepare_delete_with_gate",
  );
  const settle = braceBlock(source, "pub(super) async fn settle_files");

  assert.match(prepare, /let pending = delete_files[\s\S]*?PendingInstanceDeletion::new/);
  assert.match(prepare, /if let Some\(pending\) = pending[\s\S]*?prepare_instance_deletion_files/);
  assert.match(prepare, /else \{\s*PreparedInstanceDeletionFiles::Kept\s*\}/);
  assert.match(prepare, /PreparedInstanceDeletionFiles::Removed\(pending\)/);
  assert.match(settle, /PreparedInstanceDeletionFiles::Removed\(record\)[\s\S]*?finish_removed_instance_deletion/);
  assert.match(source, /directory\.park_as\(tombstone_name\)/);
  assert.match(source, /directory\.remove_tree\(\)/);
  assert.doesNotMatch(source, /async fn remove_instance_directory/);
  assert.doesNotMatch(source, /fail_registry_delete|fail_delete_after_commit/);
});

test("generic registry reconciliation cannot consume deletion settlement", async () => {
  const source = await read("apps/api/src/state/instance_registry.rs");
  const reconcile = braceBlock(source, "async fn reconcile_obligations");

  assert.match(reconcile, /self\.reconcile_retry\(gate\)\.await/);
  assert.doesNotMatch(reconcile, /pending_deletions|remove_tree|park_as|tombstone/);
  assert.match(source, /async fn remove_uncommitted_instance_directory/);
});
