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

function ordered(source, markers) {
  let previous = -1;
  for (const marker of markers) {
    const index = source.indexOf(marker, previous + 1);
    assert.notEqual(index, -1, `missing ordered marker: ${marker}`);
    assert.ok(index > previous, `marker is out of order: ${marker}`);
    previous = index;
  }
}

test("State owns one bounded instance deletion admission", async () => {
  const [state, coordinator] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/instance_deletions.rs"),
  ]);

  assert.match(state, /mod instance_deletions;/);
  assert.match(
    state,
    /instance_deletions:\s*instance_deletions::InstanceDeletionCoordinator/,
  );
  assert.match(
    state,
    /instance_deletions:\s*instance_deletions::InstanceDeletionCoordinator::new\(\)/,
  );
  assert.match(coordinator, /gate:\s*Arc<AsyncMutex<\(\)>>/);
  assert.match(coordinator, /phase:\s*Arc<AtomicU8>/);
  assert.match(
    coordinator,
    /retained:\s*Arc<Mutex<Option<RetainedInstanceDeletion>>>/,
  );
  assert.match(
    coordinator,
    /pub\(super\) async fn admit\(\s*&self,\s*state: &AppState,\s*\) -> Result<InstanceDeletionAdmission, InstanceStoreError>/,
  );

  for (const marker of [
    "async fn delete_instance_admitted(",
    "async fn delete_pristine_setup_instance_admitted(",
  ]) {
    const deletion = braceBlock(state, marker);
    assert.match(deletion, /instance_deletions\.admit\(self\)\.await/);
  }
  assert.doesNotMatch(state, /\.delete_with_gate\(/);
});

test("State takes detached producer ownership before deletion can wait", async () => {
  const [state, application, applicationTests, install, setup] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/application/instances.rs"),
    read("apps/api/src/application/instances/tests.rs"),
    read("apps/api/src/application/install.rs"),
    read("apps/api/src/application/instances/setup.rs"),
  ]);
  const deletion = braceBlock(
    state,
    "pub(crate) async fn delete_instance_owned(",
  );
  ordered(deletion, [
    "let retry_owner = owner.claim_child()",
    ".spawn_joinable(async move",
    "foreground.wait_for_settlement().await",
    "delete_instance_admitted",
    "retry_owner",
  ]);
  const pristine = braceBlock(
    state,
    "pub(crate) async fn delete_pristine_setup_instance_with_owner(",
  );
  ordered(pristine, [
    "let retry_owner = owner.claim_child()",
    ".spawn_joinable(async move",
    "delete_pristine_setup_instance_admitted",
    "retry_owner",
  ]);
  assert.doesNotMatch(pristine, /register_integrity_foreground|try_claim_producer/);
  const pristineApplication = braceBlock(
    install,
    "pub(crate) async fn remove_pristine_setup_instance_admitted(",
  );
  ordered(pristineApplication, [
    "owner: ProducerLease",
    "foreground: IntegrityForegroundLease",
    ".delete_pristine_setup_instance_with_owner(",
  ]);
  assert.doesNotMatch(
    pristineApplication,
    /register_integrity_foreground|try_claim_producer/,
  );
  const setupTransaction = braceBlock(
    setup,
    "pub(super) async fn execute_setup_mutation_owned",
  );
  ordered(setupTransaction, [
    "register_integrity_foreground()",
    "producer.claim_child()",
    ".spawn_joinable(async move",
    "cleanup_foreground.wait_for_settlement().await",
    "mutation(",
  ]);
  assert.match(
    applicationTests,
    /failed_setup_queue_cleanup_survives_quiescence_after_admission/,
  );
  const route = braceBlock(application, "handle_delete_instance_owned(");
  ordered(route, [
    "handoff",
    ".try_claim()",
    "register_integrity_foreground()",
    ".delete_instance_owned(",
    "producer.claim_child()",
  ]);
  assert.doesNotMatch(route, /spawn_joinable/);
});

test("shutdown settles deletions before dependent owners close", async () => {
  const shutdown = await read("apps/api/src/state/shutdown.rs");
  const coordinate = braceBlock(shutdown, "async fn coordinate");
  ordered(coordinate, [
    "wait_for_quiesced",
    "close_instance_deletions",
    "close_managed_compositions",
    "close_known_good_inventories",
    "close_user_mod_witnesses",
    "close_instance_registry",
    "close_managed_library",
  ]);

  for (const marker of [
    "async fn close_managed_compositions",
    "async fn close_performance_rules",
    "async fn close_known_good_inventories",
    "async fn close_user_mod_witnesses",
    "async fn close_instance_registry",
  ]) {
    const close = braceBlock(shutdown, marker);
    assert.match(close, /AppShutdownStep::InstanceDeletions/);
  }
});

test("retained deletion carriers preserve commit order and retry ownership", async () => {
  const [coordinator, applicationTests, store] = await Promise.all([
    read("apps/api/src/state/instance_deletions.rs"),
    read("apps/api/src/application/instances/tests.rs"),
    read("apps/api/src/state/instance_registry.rs"),
  ]);
  for (const phase of [
    "Prepared",
    "PreparationRetry",
    "PersistenceRetry",
    "PreCommitRestoreRetry",
    "Committed",
    "SettlementRetry",
    "MarkerRetry",
  ]) {
    assert.match(coordinator, new RegExp(`\\b${phase}\\b`));
  }

  const drive = braceBlock(coordinator, "async fn drive_deletion_once");
  ordered(drive, [
    "prepared.persist().await",
    "auxiliaries.commit(state).await",
    "committed.settle_files().await",
  ]);

  const durability = braceBlock(
    coordinator,
    "fn registry_commit_is_durable",
  );
  assert.match(
    durability,
    /Committed\(_\)[\s\S]*MarkerRetry\(_\)[\s\S]*=> true/,
  );
  assert.match(
    durability,
    /SettlementRetry \{ expected, \.\. \}[\s\S]*FilesystemSettlementExpectation::Settled/,
  );
  ordered(durability, [
    "Prepared(_)",
    "PreparationRetry(_)",
    "PersistenceRetry(_)",
    "PreCommitRestoreRetry { .. }",
    "=> false",
  ]);

  const preparation = braceBlock(coordinator, "async fn prepare_deletion");
  assert.match(
    preparation,
    /Result<RetainedInstanceDeletion, InstanceStoreError>/,
  );
  assert.doesNotMatch(coordinator, /PreparedDeletionDrive|initial_error/);

  const request = braceBlock(coordinator, "async fn drive_request");
  ordered(request, [
    "INSTANCE_DELETION_RETRY_INITIAL_DELAY",
    "loop",
    "drive_deletion_once(state, deletion).await",
    "registry_commit_is_durable()",
    "self.retain(deletion)",
    "self.spawn_retained_driver",
    "return Ok(())",
    "tokio::select!",
    "tokio::time::sleep(retry_delay)",
    "retry_owner.wait_for_request_drain_start()",
    "self.retain(deletion)",
    "return Err(error)",
    ".saturating_mul(2)",
    ".min(INSTANCE_DELETION_RETRY_MAX_DELAY)",
  ]);
  assert.match(
    request,
    /InstanceDeletionAttempt::Settled => return Ok\(\(\)\)/,
  );
  assert.match(
    request,
    /InstanceDeletionAttempt::Aborted[\s\S]*return Err\(instance_deletion_aborted_error\(\)\)/,
  );
  assert.doesNotMatch(request, /subscribe_shutdown/);

  for (const behavior of [
    "cancelled_delete_caller_cannot_cancel_lifecycle_waiting_owner",
    "cancelled_delete_caller_cannot_cancel_registry_waiting_owner",
    "admitted_delete_claims_its_request_handoff_during_drain",
  ]) {
    assert.match(applicationTests, new RegExp(`async fn ${behavior}\\b`));
  }
  for (const behavior of [
    "accepted_keep_files_delete_retries_exact_revision_before_publication",
    "refused_precommit_delete_restores_the_exact_parked_tree",
    "delete_files_parks_then_removes_exact_tree_and_clears_marker",
  ]) {
    assert.match(store, new RegExp(`async fn ${behavior}\\b`));
  }

  const background = braceBlock(coordinator, "async fn drive_background");
  ordered(background, [
    "subscribe_shutdown()",
    "INSTANCE_DELETION_RETRY_INITIAL_DELAY",
    "drive_deletion_once",
    "tokio::time::sleep(retry_delay)",
    ".saturating_mul(2)",
    ".min(INSTANCE_DELETION_RETRY_MAX_DELAY)",
  ]);

  const close = braceBlock(coordinator, "async fn close_owned");
  ordered(close, [
    "self.take_retained()",
    "drive_deletion_once",
    "self.retain(deletion)",
  ]);

  const errorClass = braceBlock(
    coordinator,
    "fn instance_deletion_error_class",
  );
  for (const [variant, label] of [
    ["Root", "root"],
    ["Read", "read"],
    ["Parse", "parse"],
    ["Validation", "validation"],
    ["TooLarge", "too_large"],
    ["Persistence", "persistence"],
  ]) {
    assert.match(
      errorClass,
      new RegExp(`InstanceStoreError::${variant}[^=]*=> "${label}"`),
    );
  }
});

test("startup recovery transfers cancellation and shutdown ownership", async () => {
  const [state, coordinator] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/instance_deletions.rs"),
  ]);
  const load = braceBlock(state, "pub async fn load");
  ordered(load, [
    "InstanceDeletionStartupWaiter::pending()",
    "spawn_startup_recovery",
    "progress_startup()",
    "startup_waiter.mark_app_owned()",
    "Ok(state)",
  ]);

  const startup = braceBlock(coordinator, "pub(super) fn spawn_startup_recovery");
  ordered(startup, [
    "owner.claim_child()",
    "owner.spawn_joinable(async move",
    "prepare_startup_deletion_recovery_with_gate",
    "drive_deletion_once",
    "coordinator.retain(deletion)",
    "coordinator.spawn_retained_driver",
  ]);
  const waiterDrop = braceBlock(
    coordinator,
    "impl Drop for InstanceDeletionStartupWaiter",
  );
  assert.match(waiterDrop, /InstanceDeletionStartupOwnership::WaiterLost/);
  const orphan = braceBlock(
    coordinator,
    "fn spawn_instance_deletion_orphan_shutdown",
  );
  ordered(orphan, [
    "tokio::spawn(async move",
    "INSTANCE_DELETION_ORPHAN_SHUTDOWN_ATTEMPTS",
    "state.shutdown().await",
    "std::process::abort()",
  ]);
});
