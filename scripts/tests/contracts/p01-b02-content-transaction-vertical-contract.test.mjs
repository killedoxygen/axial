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

test("content transaction authority stays move-only and filesystem opaque", async () => {
  const [transaction, managedFs, minecraft] = await Promise.all([
    read("core/minecraft/src/managed_fs/content_transaction.rs"),
    read("core/minecraft/src/managed_fs.rs"),
    read("core/minecraft/src/lib.rs"),
  ]);
  const root = braceBlock(transaction, "pub struct ManagedContentTransactionRoot");
  ordered(root, [
    "directory: ManagedTreeDirectory",
    "authority: ManagedTransferAuthority",
  ]);
  for (const type of [
    "ManagedContentTransactionRoot",
    "ManagedContentPlanningSession",
    "ManagedContentManifestObservationFailure",
    "ManagedContentPlanningObservationFailure",
    "ManagedContentTransactionSession",
    "ManagedContentPreparedTransaction",
    "ManagedContentAwaitingTransaction",
    "ManagedContentTransferSlot",
    "ManagedContentSlotCancellation",
    "ManagedContentCancelledSlot",
    "ManagedContentReadyTransaction",
    "ManagedContentRecovery",
  ]) {
    assert.doesNotMatch(
      transaction,
      new RegExp(`(?:derive\\([^)]*Clone[^)]*\\)[\\s\\S]{0,80}${type}|impl\\s+Clone\\s+for\\s+${type})`),
      `${type} must remain move-only`,
    );
  }
  assert.doesNotMatch(transaction, /impl\s+Drop\s+for\s+ManagedContent/);
  assert.doesNotMatch(
    transaction,
    /pub\s+fn\s+\w+[^\{;]*(?:PathBuf|\bPath\b|\bDirectory\b|TransientStageSealed|FileCapability|EffectOwner)/s,
  );
  assert.doesNotMatch(
    transaction,
    /pub\s+(?:struct|enum)\s+\w+\s*\{[^}]*(?:PathBuf|\bPath\b|\bDirectory\b|TransientStageSealed|FileCapability|EffectOwner)/s,
  );
  assert.match(managedFs, /pub use content_transaction::\{/);
  assert.match(minecraft, /pub mod managed_path[\s\S]*ManagedContentTransactionOutcome/);
});

test("manifest-first planning is incremental bounded and cache-only at finish", async () => {
  const transaction = await read(
    "core/minecraft/src/managed_fs/content_transaction.rs",
  );
  assert.match(transaction, /const MAX_CONTENT_PLANNING_PATHS: usize = 8_704;/);
  const root = braceBlock(transaction, "impl ManagedContentTransactionRoot");
  assert.match(root, /pub fn observe_manifest\(\s*self,/);
  assert.doesNotMatch(root, /pub fn observe\s*\(/);
  const planning = braceBlock(transaction, "impl ManagedContentPlanningSession");
  assert.match(planning, /pub fn manifest_state\(&self\)/);
  assert.match(planning, /pub fn manifest_bytes\(&self\)/);
  assert.match(planning, /pub fn observe_more\(\s*self,/);
  assert.match(planning, /pub fn finish\(\s*self,/);
  const observeMore = braceBlock(transaction, "fn observe_more_transaction_paths");
  ordered(observeMore, [
    ".checked_add(paths.len())",
    "total > MAX_CONTENT_PLANNING_PATHS",
    "session.observed_paths.contains_key(&key)",
    "observe_file(",
    "Some(&mut session.remaining_bytes)",
    "session.observations.push",
    "session.observed_paths.insert(key, exact_path)",
  ]);
  assert.doesNotMatch(observeMore, /remaining_bytes\s*=\s*session\.remaining_bytes/);
  const finish = braceBlock(transaction, "fn finish_transaction_observation");
  ordered(finish, [
    "paths.len() > MAX_CONTENT_PATHS",
    "session.observed_paths.get(&key) != Some(path)",
    "let mut observations_by_key = observations",
    ".into_iter()",
    ".remove(&path.key())",
    "let read_preconditions = observations_by_key.into_values().collect()",
    "remaining_transaction_bytes: remaining_bytes",
  ]);
  assert.doesNotMatch(
    finish,
    /observe_file|sha512_guarded_file|read_guarded_file_bounded/,
  );
  const plan = braceBlock(transaction, "impl ManagedContentMutationPlan");
  ordered(plan, [
    ".insert(observation.path.key(), observation)",
    "observed.path != mutation.path",
    "observed.state != mutation.observed",
  ]);
  const binding = braceBlock(transaction, "fn plan_matches_session");
  assert.match(
    binding,
    /mutation\.path == observation\.public\.path[\s\S]*?mutation\.observed == observation\.public\.state/,
  );
  const transactionSession = braceBlock(
    transaction,
    "pub struct ManagedContentTransactionSession",
  );
  ordered(transactionSession, [
    "observations: Vec<PathObservationAuthority>",
    "read_preconditions: Vec<PathObservationAuthority>",
    "remaining_transaction_bytes: u64",
  ]);
  for (const testName of [
    "manifest_first_planning_is_incremental_and_selects_one_inspected_subset",
    "failed_manifest_observation_returns_the_no_effect_root",
    "plan_from_an_aliased_session_cannot_bind_a_later_exact_guard",
    "late_batch_failure_retains_successful_observations_and_budget",
  ]) {
    assert.match(transaction, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
});

test("plan is complete bounded portable and digest authenticated", async () => {
  const transaction = await read(
    "core/minecraft/src/managed_fs/content_transaction.rs",
  );
  const plan = braceBlock(transaction, "impl ManagedContentMutationPlan");
  ordered(plan, [
    "mutations.len() > MAX_CONTENT_PATHS",
    "validate_content_path(&observation.path)?",
    ".checked_add(*size)",
    "DuplicatePayloadId",
    "expected_sha1().is_none()",
    "transfer_contract_limit(&payload.contract)",
    "MAX_CONTENT_TRANSACTION_BYTES",
    "payload_bytes > manifest.remaining_transaction_bytes",
    "DuplicatePayloadUse",
    "used_payloads.len() != payload_ids.len()",
  ]);
  const planning = braceBlock(transaction, "impl ManagedContentPlanningSession");
  assert.match(planning, /pub fn manifest_bytes\(&self\) -> Option<&\[u8\]>/);
  const session = braceBlock(transaction, "impl ManagedContentTransactionSession");
  assert.match(session, /pub fn bind_encoded_manifest/);
  assert.match(transaction, /Arc::ptr_eq\(&session\.manifest_session, &plan\.manifest\.session\)/);
  assert.doesNotMatch(transaction, /serde_json/);
  const path = braceBlock(transaction, "fn validate_content_path");
  assert.match(path, /"mods" \| "resourcepacks" \| "shaderpacks"/);
  assert.match(path, /managed_content_name_is_reserved/);
  assert.match(transaction, /Exact\s*\{\s*size:\s*u64,\s*sha512:\s*Box<str>/);
  assert.match(transaction, /MissingObservation/);
  assert.match(transaction, /ObservationChanged/);
  assert.match(transaction, /ManagedContentPlanError::TransactionBudgetExceeded/);
  const observe = braceBlock(transaction, "fn observe_file");
  ordered(observe, [
    "guard.size() > max_bytes",
    "admit_observed_bytes(remaining, guard.size())",
    "sha512_guarded_file",
  ]);
  assert.match(transaction, /ManagedContentObservationError::TransactionBudgetExceeded/);
  assert.doesNotMatch(transaction, /ManagedContentPathResult::Preserve/);
  assert.doesNotMatch(plan, /mutations\.is_empty\(\)\s*\|\|\s*observations\.is_empty\(\)/);
});

test("preparation atomically reserves one private payload group", async () => {
  const transaction = await read(
    "core/minecraft/src/managed_fs/content_transaction.rs",
  );
  const prepare = braceBlock(transaction, "fn prepare_transaction");
  ordered(prepare, [
    "plan_matches_session",
    "MAX_CONTENT_PRIVATE_DIRECTORIES",
    'format!(\n        ".axial-content-{}"',
    "create_child_new(PRIVATE_STAGE_NAME)",
    "create_child_new(PRIVATE_BACKUP_NAME)",
    "ManagedContentTransferGroup",
    ".admit_transient_destinations(names)",
    "ManagedContentTransferSlotAuthority",
    "CreateOnlyTransferTarget::new(",
  ]);
  assert.doesNotMatch(prepare, /admit_transient_destination\s*\(/);
  assert.match(prepare, /PrivateNamespaceExhausted/);
  const group = braceBlock(transaction, "struct ManagedContentTransferGroup");
  assert.match(group, /_state_authority:\s*ManagedTransferAuthority/);
  assert.match(transaction, /MAX_CONTENT_FILE_BYTES/);
  assert.match(transaction, /MAX_CONTENT_TRANSACTION_BYTES/);
});

test("verified stages never expose their sealed carrier", async () => {
  const [transaction, transfer] = await Promise.all([
    read("core/minecraft/src/managed_fs/content_transaction.rs"),
    read("core/minecraft/src/download/transient_transfer.rs"),
  ]);
  assert.match(
    transfer,
    /pub\(crate\) fn retained\(&self\) -> Self/,
  );
  assert.match(
    transfer,
    /pub\(crate\) fn into_content_stage[\s\S]*TransientStageSealed/,
  );
  assert.match(
    transfer,
    /pub\(crate\) fn from_content_stage[\s\S]*TransientStageSealed/,
  );
  const accept = braceBlock(
    transaction,
    "fn accept_verified(\n    transaction:",
  );
  ordered(accept, [
    "shares_retained_authority",
    "report_matches_contract",
    "into_content_stage()",
    "TransientPublicationBatch::new(stages)",
    "publish_create_new()",
  ]);
  assert.match(transaction, /TransientPublicationBatchOutcome::Partial/);
  assert.match(transaction, /TransientPublicationBatchOutcome::Pending/);
  assert.match(transaction, /VerifiedTransferDiscardOutcome::Pending/);
  assert.match(transaction, /RecoveryState::StageFilePending/);
  assert.doesNotMatch(transaction, /Err\(\(_error, file\)\)\s*=>\s*\{\s*drop\(file\)/);
});

test("prepared cancellation and issued-slot settlement stay explicit", async () => {
  const transaction = await read(
    "core/minecraft/src/managed_fs/content_transaction.rs",
  );
  const prepared = braceBlock(transaction, "impl ManagedContentPreparedTransaction");
  assert.match(prepared, /pub fn into_transfer_slots/);
  assert.match(prepared, /pub fn cancel\(self\)/);
  const slot = braceBlock(transaction, "impl ManagedContentTransferSlot");
  assert.match(slot, /ManagedContentSlotCancellation/);
  const admission = braceBlock(transaction, "impl ManagedContentSlotCancellation");
  assert.match(admission, /authority:\s*ManagedTransferTerminalAuthority/);
  assert.match(admission, /shares_retained_authority/);
  assert.match(admission, /ManagedContentSlotCancellationOutcome::Refused/);
  const awaiting = braceBlock(transaction, "impl ManagedContentAwaitingTransaction");
  assert.match(awaiting, /pub fn cancel\([\s\S]*Vec<ManagedContentCancelledSlot>/);
  assert.match(awaiting, /MissingSlot/);
  assert.match(awaiting, /DuplicateSlot/);
  assert.match(awaiting, /ForeignAuthority/);
  assert.match(awaiting, /transaction: self,[\s\S]*receipts,/);
  assert.doesNotMatch(awaiting, /pub fn cancel\(self\)/);
  const cancel = braceBlock(transaction, "fn cancel_transfer_slots");
  assert.match(cancel, /slot\.target\.cancel\(\)/);
  assert.match(cancel, /TransferTargetCancelOutcome::Pending/);
  assert.match(transaction, /RecoveryState::TargetCancelPending/);
  assert.doesNotMatch(transaction, /impl\s+Drop\s+for\s+ManagedContent/);
});

test("commit publishes the manifest last and rollback reverses effects", async () => {
  const transaction = await read(
    "core/minecraft/src/managed_fs/content_transaction.rs",
  );
  const commit = braceBlock(transaction, "fn drive_commit");
  ordered(commit, [
    "revalidate_all(&state)",
    "mutation.backup_name.as_str()",
    "payload_name.as_str()",
    "let mut synced = HashSet::new()",
    "revalidate_read_preconditions(&state)",
    "revalidate_final_effects(&state)",
    '"manifest-old"',
    "manifest_publication_started = true",
    "write_new_exact_retained(MANIFEST_NAME",
    "state.root.sync()",
    "state.manifest_committed =",
    "cleanup_committed(state)",
  ]);
  assert.match(commit, /ManagedCreateOnlyWriteFailure::BeforePromotion/);
  assert.match(
    commit,
    /ManagedCreateOnlyWriteFailure::PromotionAttempted\s*\{\s*final_guard\s*\}/,
  );
  const initialValidation = braceBlock(transaction, "fn revalidate_all");
  assert.match(initialValidation, /revalidate_read_preconditions\(state\)/);
  const finalEffects = braceBlock(transaction, "fn revalidate_final_effects");
  assert.match(finalEffects, /ManagedContentPathResult::Absent/);
  assert.match(finalEffects, /ManagedContentPathResult::Download/);
  assert.match(finalEffects, /installed_guard/);
  const rollback = braceBlock(transaction, "fn drive_rollback");
  ordered(rollback, [
    "if state.manifest_committed",
    "remove_guarded_file(MANIFEST_NAME",
    'rename_guarded_file_no_replace("manifest-old"',
    "for index in (0..state.mutations.len()).rev()",
    ".remove_guarded_file(state.mutations[index].name.as_str()",
    "mutation.backup_name.as_str()",
    "cleanup_private(&mut state)",
  ]);
});

test("recovery reconstructs both bindings before choosing a direction", async () => {
  const transaction = await read(
    "core/minecraft/src/managed_fs/content_transaction.rs",
  );
  const reconcile = braceBlock(transaction, "impl ManagedContentRecovery");
  ordered(reconcile, [
    "state.root.inner.root.settle()",
    "classify_transaction(&mut state)",
    "state.manifest_committed || intent == TransactionIntent::Commit",
    "cleanup_committed(state)",
    "drive_rollback(state",
  ]);
  const classify = braceBlock(transaction, "fn classify_transaction");
  assert.doesNotMatch(classify, /read_preconditions|Preserve/);
  assert.match(classify, /let source =[\s\S]*let backup =/);
  assert.match(classify, /match \(source, backup\)/);
  assert.match(classify, /let staged =[\s\S]*let installed =/);
  assert.match(classify, /match \(staged, installed\)/);
  assert.match(classify, /payload_guard_matches_report/);
  assert.doesNotMatch(classify, /unwrap_or\(false\)|\.ok\(\)\s*\.flatten/);
  assert.match(transaction, /enum ExactBindingState[\s\S]*Unknown/);
  assert.match(transaction, /enum CleanupDirectoryState[\s\S]*Discover[\s\S]*Known[\s\S]*Done/);
  assert.match(transaction, /else \{\s*return false;\s*\}/);
  const cleanup = braceBlock(transaction, "fn cleanup_private");
  assert.match(cleanup, /guard\.take\(\)/);
  assert.match(cleanup, /guard = Some\(guard\)/);
  const authenticate = braceBlock(transaction, "fn payload_guard_matches_report");
  ordered(authenticate, [
    "guard.size() != report.bytes()",
    "sha1_guarded_file_bytes",
    "sha512_guarded_file",
    "digests.sha1().is_some() || digests.sha512().is_some()",
  ]);
  const outcome = braceBlock(
    transaction,
    "pub enum ManagedContentTransactionOutcome",
  );
  assert.match(outcome, /Committed\(ManagedContentCommitReceipt\)/);
  assert.match(outcome, /Cancelled\(ManagedContentCancelReceipt\)/);
  assert.match(outcome, /Failed\(ManagedContentTransactionFailure\)/);
  assert.match(outcome, /RecoveryRequired\(ManagedContentRecovery\)/);
  assert.equal((outcome.match(/^\s{4}\w+/gm) ?? []).length, 4);
  assert.doesNotMatch(reconcile, /Result\s*</);
  const retainRecovery = braceBlock(transaction, "fn recovery");
  ordered(retainRecovery, [
    "state.read_preconditions.clear()",
    "RecoveryState::Transaction",
  ]);
  for (const testName of [
    "manifest_only_transaction_publishes_without_pseudo_mutations",
    "more_than_effect_limit_read_preconditions_remain_non_effects",
    "read_precondition_drift_is_rejected_before_the_first_effect",
    "read_precondition_drift_after_an_effect_rolls_back_before_manifest",
    "final_effect_drift_blocks_manifest_and_recovery_ignores_read_preconditions",
  ]) {
    assert.match(transaction, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
});

test("guarded removal streams and revalidates the admitted revision", async () => {
  const managedFs = await read("core/minecraft/src/managed_fs.rs");
  const removal = braceBlock(managedFs, "fn remove_guarded_file_locked");
  ordered(removal, [
    "guard.size > MAX_MANAGED_GUARDED_REMOVAL_BYTES",
    "file.validate_revision(&guard.revision)",
    "file.reader(MAX_MANAGED_GUARDED_REMOVAL_BYTES)",
    "reader.finish()",
    "file.validate_revision(&guard.revision)",
    "guard.revision.retained()",
    "file.park_request(expected)",
  ]);
  assert.doesNotMatch(removal, /read_bounded|Vec\s*</);
  assert.match(managedFs, /settle_remove_exact_empty_child/);
  const exactDirectory = braceBlock(managedFs, "fn open_exact_directory_locked");
  assert.match(exactDirectory, /exact_portable_entry_kind/);
  assert.match(exactDirectory, /kind != Some\(EntryKind::Directory\)/);
});
