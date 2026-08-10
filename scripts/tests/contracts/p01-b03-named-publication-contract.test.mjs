import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) =>
  readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

const block = (source, marker) => {
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing ${marker}`);
  const open = source.indexOf("{", start);
  assert.notEqual(open, -1, `missing body for ${marker}`);
  let depth = 0;
  for (let index = open; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    if (source[index] === "}") depth -= 1;
    if (depth === 0) return source.slice(start, index + 1);
  }
  assert.fail(`unterminated ${marker}`);
};

const ordered = (source, markers) => {
  let cursor = -1;
  for (const marker of markers) {
    const next = source.indexOf(marker, cursor + 1);
    assert.notEqual(next, -1, `missing ordered marker ${marker}`);
    assert.ok(next > cursor, `${marker} is out of order`);
    cursor = next;
  }
};

test("named stages bind a cooperatively owned sealed revision before publication", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const receipt = block(platform, "pub(crate) struct PublicationReceipt");
  assert.match(receipt, /state:\s*PublicationReceiptState/);
  assert.match(receipt, /binding:\s*PublicationBinding/);
  assert.doesNotMatch(receipt, /pub\(crate\)\s+state|Copy/);
  const receiptState = block(platform, "enum PublicationReceiptState");
  assert.match(receiptState, /\bAttempted\b/);
  assert.match(receiptState, /\bParentsPending\b/);
  assert.match(receiptState, /\bPoisoned\b/);
  assert.match(receiptState, /\bReported\b/);
  assert.match(receiptState, /\bCommitted\b/);
  assert.doesNotMatch(receiptState, /PublicationBinding/);
  const binding = block(platform, "struct PublicationBinding");
  for (const field of [
    "attempt_id",
    "staged_file",
    "sealed_size",
    "sealed_stamp",
    "source_parent",
    "source_name",
    "destination_parent",
    "destination_name",
  ]) {
    assert.match(binding, new RegExp(`${field}:`));
  }
  assert.doesNotMatch(binding, /pub\(crate\)/);
  const receiptImpl = block(platform, "impl PublicationReceipt");
  assert.match(receiptImpl, /pub\(crate\) fn is_attempted/);
  assert.match(receiptImpl, /pub\(crate\) fn matches_attempt/);
  assert.match(receiptImpl, /pub\(crate\) fn accepts_successor/);
  assert.match(receiptImpl, /fn validate_binding/);
  assert.match(receiptImpl, /fn validate_exact_revision/);
  assert.match(receiptImpl, /fn validate_content_observation/);
  assert.match(receiptImpl, /fn mark_poisoned/);
  assert.match(receiptImpl, /fn mark_committed/);
  assert.doesNotMatch(receiptImpl, /pub\(crate\) fn (?:mark_parents_pending|mark_reported|mark_poisoned|mark_committed)/);
  const preparation = block(platform, "pub(crate) fn prepare_publication");
  ordered(preparation, ["file_receipt_fields", "file_identity", "file_binding_state"]);
  assert.match(preparation, /observed_stamp\s*!=\s*sealed_stamp/);

  const seal = block(library, "pub fn seal(self)");
  ordered(seal, [
    "self.file.validate(&operation)",
    "platform::file_receipt_fields",
    "platform::sync_publication_file",
    "validate_revision_in",
    "StageRegistryPhase::Sealed",
  ]);
  assert.match(
    block(library, "pub struct SealedStagedFile"),
    /revision:\s*FileRevision/,
  );

  const promotion = block(library, "fn promote_no_replace_internal(");
  ordered(promotion, [
    "validate_revision_in",
    "allocate_publication_attempt",
    "platform::prepare_publication",
    "prepare_promotion",
    "platform::rename_no_replace",
    "record_publication",
    "validate_publication_attempt",
    "platform::settle_publication",
    "validate_content_revision_in",
    "token.disarm",
  ]);
  assert.match(
    block(library, "struct StagePromotionRecord"),
    /destination:\s*NamespaceLeaf[\s\S]*attempt_id:\s*u64[\s\S]*receipt:\s*platform::PublicationReceipt[\s\S]*displaced_park:\s*Option<u64>/,
  );
  assert.doesNotMatch(promotion, /PublicationReceipt::(?:attempted|committed|parents_pending)/);
  assert.match(promotion, /record_publication\(attempt_id,\s*receipt\.clone\(\)\)/);
  assert.match(promotion, /BindingState::Exact/);
  const liveAttempt = block(library, "fn validate_stage_publication(");
  assert.match(liveAttempt, /StageRegistryPhase::PromotionAttempted/);
  assert.match(liveAttempt, /promotion\.attempt_id\s*!=\s*attempt_id/);
  assert.match(liveAttempt, /promotion\.receipt\.accepts_successor\(receipt\)/);
});

test("Unix publication retains real file and parent barriers", async () => {
  const platform = await read("core/fs/src/platform.rs");
  const unix = platform.slice(
    platform.indexOf("#[cfg(unix)]\nmod native {"),
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const fileSync = block(unix, "pub(crate) fn sync_publication_file");
  assert.match(fileSync, /target_os = "macos"[\s\S]*full_fsync/);
  assert.match(fileSync, /not\(target_os = "macos"\)[\s\S]*rfs::fsync/);

  const rename = block(unix, "pub(crate) fn rename_no_replace");
  ordered(rename, [
    "file_binding_state",
    "RenameFlags::NOREPLACE",
    "attempt.mark_parents_pending()",
    "PublicationReceipt::validate_content",
  ]);

  const settle = block(unix, "pub(crate) fn settle_publication");
  ordered(settle, [
    "file_identity",
    "file_receipt_fields",
    "settle_publication_observation",
  ]);
  const settleObservation = block(unix, "fn settle_publication_observation");
  ordered(settleObservation, [
    "PublicationReceipt::validate_content_observation",
    "PublicationReceipt::validate_binding",
    "PublicationReceiptState::Poisoned",
    "sync_publication_directory(destination_parent)",
    "directory_identity(source_parent)",
    "sync_publication_directory(source_parent)",
    "receipt.mark_committed()",
  ]);
  assert.match(settle, /receipt:\s*&mut PublicationReceipt/);
  assert.match(settleObservation, /receipt\.mark_poisoned\(\)/);
  const parentSync = block(unix, "fn sync_publication_directory");
  assert.match(parentSync, /ErrorKind::Interrupted[\s\S]*continue/);
});

test("Windows publication requires a same-volume NTFS write-through receipt", async () => {
  const platform = await read("core/fs/src/platform.rs");
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const rename = block(windows, "pub(crate) fn rename_no_replace");
  ordered(rename, [
    "file_binding_state",
    "require_ntfs_publication_volume",
    "set_publication_write_through",
    "rename_handle_no_replace",
    "attempt.mark_reported()",
  ]);
  assert.doesNotMatch(rename, /sync_directory|PathBuf|canonicalize/);

  const volume = block(windows, "fn require_ntfs_publication_volume");
  assert.match(volume, /require_local_publication_handle\(source\)/);
  assert.match(volume, /require_local_publication_handle\(&destination_parent\.file\)/);
  assert.match(volume, /file_identity\(source\)\?\.volume/);
  assert.match(volume, /directory_identity\(destination_parent\)\?\.volume/);
  assert.match(volume, /require_local_ntfs\(source\)/);

  const localNtfs = block(windows, "fn require_local_ntfs");
  assert.match(localNtfs, /GetVolumeInformationByHandleW/);
  assert.match(localNtfs, /eq_ignore_ascii_case\("NTFS"\)/);
  const locality = block(windows, "fn require_local_publication_handle");
  assert.match(locality, /NtQueryInformationFile/);
  assert.match(locality, /FileIsRemoteDeviceInformation/);
  assert.match(locality, /information\.IsRemote\s*!=\s*0/);
  assert.doesNotMatch(locality, /FileRemoteProtocolInfo/);

  const writeThrough = block(windows, "fn set_publication_write_through");
  ordered(writeThrough, [
    "NtQueryInformationFile",
    "FILE_SYNCHRONOUS_IO_NONALERT",
    "mode.Mode |= FILE_WRITE_THROUGH",
    "NtSetInformationFile",
  ]);
  assert.match(writeThrough, /NtSetInformationFile[\s\S]*query_mode\(file\)/);

  const settle = block(windows, "pub(crate) fn settle_publication");
  ordered(settle, [
    "file_identity",
    "file_receipt_fields",
    "settle_publication_observation",
  ]);
  const settleObservation = block(windows, "fn settle_publication_observation");
  assert.match(settleObservation, /PublicationReceiptState::Reported/);
  assert.match(settleObservation, /PublicationReceiptState::Committed/);
  assert.match(settleObservation, /PublicationReceipt::validate_content_observation/);
  assert.match(settleObservation, /PublicationReceipt::validate_binding/);
  assert.match(settle, /receipt:\s*&mut PublicationReceipt/);
  assert.match(settleObservation, /receipt\.mark_committed\(\)/);
  assert.doesNotMatch(settleObservation, /parent_revisions|directory_revision/);
  assert.match(settleObservation, /no reported NTFS write-through receipt/);
  assert.doesNotMatch(settleObservation, /sync_directory/);
});

test("one operation-state predicate owns unsettled namespace leaves", async () => {
  const [library, transient, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/transient.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const operationState = block(library, "impl OperationState");
  const reservation = block(operationState, "fn namespace_footprint_is_reserved");
  for (const owner of [
    "moves",
    "transients",
    "directory_creations",
    "stage_creations",
    "file_parks",
    "directory_parks",
    "stages.values()",
  ]) {
    assert.ok(reservation.includes(owner), `missing namespace owner ${owner}`);
  }
  assert.match(reservation, /leaf_names_equivalent/);
  assert.match(reservation, /directory_has_physical_ancestor/);
  assert.match(reservation, /excluded_file_park_original/);
  const physicalAncestry = block(
    library,
    "fn directory_has_physical_ancestor",
  );
  assert.match(physicalAncestry, /absolute_directory_has_ancestor/);
  assert.equal(
    platform.match(/pub\(crate\) fn absolute_directory_has_ancestor/g)?.length,
    2,
  );
  assert.match(reservation, /excluded_stage_create/);
  assert.match(reservation, /candidate_file\s*==\s*Some\(record\.identity\)/);
  assert.match(reservation, /record\.identity\.is_none\(\)/);
  assert.doesNotMatch(operationState, /park_owners|parks_checked_out/);

  const directoryOpen = block(library, "pub fn open_directory");
  assert.equal(
    directoryOpen.match(/ensure_leaf_not_directory_create_reserved/g)?.length,
    2,
  );
  const observedDirectoryOpen = block(library, "pub fn open_observed_directory");
  assert.equal(
    observedDirectoryOpen.match(/ensure_leaf_not_directory_create_reserved/g)
      ?.length,
    2,
  );
  const directoryCreateReservation = block(
    library,
    "fn ensure_leaf_not_directory_create_reserved",
  );
  assert.match(directoryCreateReservation, /directory_creations\.values\(\)/);
  assert.match(directoryCreateReservation, /leaf_names_equivalent/);
  assert.doesNotMatch(
    directoryCreateReservation,
    /DirectoryCreateEffectPhase::/,
  );

  for (const cleanup of [
    ["fn cleanup_stage", "cleanup.take"],
    ["fn cleanup_abandoned_stage_create", "checked_out = true"],
    ["fn cleanup_abandoned_directory_create", "checked_out = true"],
  ]) {
    ordered(block(library, cleanup[0]), ["checked_add(1)", cleanup[1]]);
  }
  const stageCleanup = block(library, "fn cleanup_stage");
  assert.match(stageCleanup, /stages\s*\.\s*get_mut/);
  assert.match(stageCleanup, /header\.cleanup\.take/);
  assert.match(stageCleanup, /header\.cleanup\s*=\s*record\.cleanup/);
  const transientCleanup = block(transient, "fn cleanup_abandoned_transient");
  ordered(transientCleanup, [
    "checked_add(1)",
    "header.checked_out = true",
    "header.retained.take()",
  ]);
  assert.match(transientCleanup, /header\.retained\s*=\s*record\.retained/);
  assert.match(transientCleanup, /header\.checked_out\s*=\s*false/);
  const fileParkDrop = block(library, "impl Drop for FileParkRecordGuard");
  assert.match(fileParkDrop, /header\.size\s*=\s*record\.size/);
  assert.match(fileParkDrop, /header\.stamp\s*=\s*record\.stamp/);

  for (const registration of [
    "register_stage_record",
    "reserve_stage_create",
    "reserve_directory_create",
    "register_file_park",
    "register_directory_park",
    "prepare_stage_promotion",
  ]) {
    assert.match(
      block(library, `fn ${registration}`),
      /namespace_(?:leaf|footprint)_is_reserved/,
    );
  }
  const moveReservation = block(library, "impl MoveEffectToken");
  assert.match(moveReservation, /namespace_footprint_is_reserved/);
  assert.match(moveReservation, /record\.source/);
  assert.match(moveReservation, /record\.destination/);
  assert.match(moveReservation, /file_park_handoff_id/);
  assert.match(moveReservation, /FileParkLink::Move/);
  const moveRecord = block(library, "struct MoveEffectRecord");
  assert.match(moveRecord, /moved_file:\s*Option<platform::Identity>/);
  assert.match(moveRecord, /displaced_park:\s*Option<u64>/);
  const parkLink = block(library, "enum FileParkLink");
  assert.match(parkLink, /Stage\(u64\)/);
  assert.match(parkLink, /Move\(u64\)/);
  const handoff = block(library, "fn file_park_handoff_id");
  assert.match(handoff, /park\.phase\s*!=\s*FileParkRegistryPhase::Live/);
  assert.match(handoff, /park\.cleanup\.is_none\s*\(\s*\)/);
  assert.match(handoff, /park\.linked_effect\.is_some\s*\(\s*\)/);
  const promotion = block(library, "fn prepare_stage_promotion");
  assert.match(promotion, /file_park_handoff_id/);
  assert.match(promotion, /FileParkLink::Stage/);
  assert.match(
    block(transient, "fn reserve_batch"),
    /state\.namespace_leaf_is_reserved/,
  );
  assert.doesNotMatch(transient, /fn transient_destination_is_reserved/);
});

test("move-after-park handoff stays linear through managed settlement", async () => {
  const [library, storage, state] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/performance/src/storage.rs"),
    read("core/performance/src/state/mod.rs"),
  ]);

  const outcome = block(library, "pub enum FileMoveAfterParkOutcome");
  assert.match(outcome, /current:\s*FileCapability/);
  assert.match(outcome, /source:\s*FileCapability/);
  assert.match(outcome, /displaced:\s*ParkedFile/);
  assert.match(outcome, /AppliedUnverified\(FileMoveAfterParkObligation\)/);
  const movement = block(library, "pub fn move_no_replace_after_park");
  assert.match(movement, /displaced:\s*ParkedFile/);
  assert.doesNotMatch(movement, /displaced:\s*&ParkedFile/);
  for (const terminal of [
    "FileMoveAfterParkOutcome::Applied",
    "FileMoveAfterParkOutcome::NoEffect",
    "FileMoveAfterParkOutcome::AppliedUnverified",
  ]) {
    assert.match(movement, new RegExp(terminal.replaceAll("::", "\\s*::\\s*")));
  }
  const obligation = block(library, "pub struct FileMoveAfterParkObligation");
  assert.match(obligation, /movement:\s*FileMoveObligation/);
  assert.match(obligation, /displaced:\s*ParkedFile/);

  const continuation = block(storage, "enum ManagedEffectContinuation");
  assert.match(
    continuation,
    /FileMoveAfterPark\(FileMoveAfterParkReceipt\)/,
  );
  const claim = block(storage, "fn claim_continuation");
  const compositeClaim = claim.slice(
    claim.indexOf("ManagedEffectContinuation::FileMoveAfterPark"),
    claim.indexOf("ManagedEffectContinuation::DirectoryMove"),
  );
  assert.match(
    compositeClaim,
    /FileMoveAfterParkReceiptOutcome::Applied[\s\S]*retain_parked_file_removal/,
  );
  assert.match(
    compositeClaim,
    /FileMoveAfterParkReceiptOutcome::NoEffect[\s\S]*retain_parked_file_restore/,
  );

  for (const caller of [
    block(state, "fn publish_staged_state"),
    block(state, "pub(crate) fn reconcile_state_publication"),
  ]) {
    const pending = caller.indexOf(
      "ManagedFileMoveAfterParkOutcome::AppliedUnverified",
    );
    assert.notEqual(pending, -1, "missing indeterminate handoff branch");
    assert.doesNotMatch(
      caller.slice(pending, caller.indexOf("}", pending)),
      /settle_parked_file_restore/,
    );
  }
});

test("focused publication regressions remain registered", async () => {
  const [library, taskfile] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("Taskfile.yml"),
  ]);
  for (const name of [
    "sealed_stage_collision_is_no_effect",
    "sealed_stage_refuses_content_revision_drift",
    "sealed_stage_refuses_live_park_original_reservation",
    "replacement_handoff_publishes_while_unrelated_park_stays_reserved",
    "replacement_handoff_link_survives_indeterminate_cleanup_until_no_effect",
    "dropped_replacement_obligation_settles_stage_before_linked_park",
    "absolute_descendant_admission_cannot_bypass_directory_park_reservation",
    "directory_subtree_reservations_reject_nested_owners_in_both_orders",
    "directory_open_refuses_live_create_before_identity_attachment",
    "directory_open_rechecks_create_reservation_after_native_open",
    "identity_pending_directory_create_blocks_nested_reservations",
    "file_park_checkout_roundtrip_preserves_learned_revision",
    "hardlink_aliases_cannot_hold_independent_file_parks",
    "file_move_handoff_occupies_only_its_exact_park_original",
    "file_move_handoff_link_survives_indeterminate_reconciliation",
    "unsettled_file_move_blocks_hardlink_alias_park_ownership",
    "stable_headers_reserve_checked_out_creates_and_directory_parks",
    "unix_observed_publication_runs_real_parent_barriers",
    "unix_publication_destination_barrier_failure_is_permanently_poisoned",
    "unix_publication_source_barrier_failure_is_permanently_poisoned",
    "publication_receipt_rejects_reuse_for_another_mutation",
    "windows_publication_requires_reported_write_through_receipt",
    "windows_committed_publication_receipt_rejects_cross_mutation",
    "sealed_stage_promotes_across_directories",
  ]) {
    assert.match(library, new RegExp(`fn ${name}\\s*\\(`));
  }
  const windowsReceiptMisuse = block(
    library,
    "fn windows_committed_publication_receipt_rejects_cross_mutation",
  );
  ordered(windowsReceiptMisuse, [
    ".prepare_promotion(",
    "platform::rename_no_replace",
    ".record_publication(",
    "platform::settle_publication",
    "other.discard()",
    "platform::prepare_publication",
    "platform::rename_no_replace",
    ".update(StageRegistryPhase::Sealed)",
    "staged.discard()",
  ]);
  assert.doesNotMatch(windowsReceiptMisuse, /std::fs::rename/);
  assert.match(
    taskfile,
    /verify:native:macos:[\s\S]*cargo test --locked -p axial-fs unix_observed_publication_runs_real_parent_barriers/,
  );
  assert.match(
    taskfile,
    /verify:contracts:[\s\S]*scripts\/tests\/contracts\/p01-b03-named-publication-contract\.test\.mjs/,
  );
});
