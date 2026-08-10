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

test("root leases retain one fixed positional recovery control", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const unix = platform.slice(
    platform.indexOf("#[cfg(unix)]\nmod native {"),
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  assert.match(
    await read("core/fs/src/recovery.rs"),
    /RECOVERY_FRAME_BYTES:\s*usize\s*=\s*16\s*\*\s*1024[\s\S]*RECOVERY_REGION_BYTES:\s*u64[\s\S]*SUCCESSOR_AGGREGATE_SLOT_COUNT:\s*usize\s*=\s*64[\s\S]*RECOVERY_CONTROL_BYTES:\s*u64\s*=\s*RECOVERY_REGION_BYTES/,
  );
  for (const native of [unix, windows]) {
    assert.match(
      block(native, "pub(crate) struct LeaseHandle"),
      /handle:\s*File[\s\S]*identity:\s*Identity[\s\S]*root:\s*RootGuard[\s\S]*name:\s*OsString[\s\S]*name_class_revision:\s*RwLock<Option<DirectoryStamp>>/,
    );
    const initialize = block(
      native,
      "pub(crate) fn recovery_control_initialize_len",
    );
    assert.match(
      initialize,
      /set_len\(RECOVERY_CONTROL_BYTES\)[\s\S]*(?:sync_recovery_control_file|sync_all)/,
    );
    assert.match(initialize, /length == 0/);
    assert.match(initialize, /length != RECOVERY_CONTROL_BYTES/);
    assert.doesNotMatch(initialize, /length < RECOVERY_CONTROL_BYTES/);
    const exactRead = block(
      native,
      "pub(crate) fn recovery_control_read_exact_at",
    );
    assert.match(exactRead, /Ok\(0\)[\s\S]*UnexpectedEof/);
    assert.doesNotMatch(exactRead, /bytes\.fill\(0\)/);
    assert.match(
      block(native, "fn validate_recovery_control("),
      /\.len\(\) != RECOVERY_CONTROL_BYTES/,
    );
    assert.match(
      block(native, "fn validate_recovery_control_range"),
      /checked_add[\s\S]*end > RECOVERY_CONTROL_BYTES/,
    );
    assert.match(
      block(native, "pub(crate) fn recovery_control_sync"),
      /(?:sync_recovery_control_file|sync_all)/,
    );
  }

  const unixAcquire = block(unix, "pub(crate) fn try_acquire_lease");
  for (const flag of ["CREATE", "EXCL", "NOFOLLOW", "NONBLOCK", "CLOEXEC"]) {
    assert.match(unixAcquire, new RegExp(`OFlags::${flag}`));
  }
  ordered(unixAcquire, [
    "lock_lease",
    "validate_lease_binding",
    "sync_publication_directory(&root.handle)",
    "validate_lease_binding",
  ]);
  assert.match(
    block(unix, "pub(crate) fn sync_publication_directory"),
    /target_os = "macos"[\s\S]*full_fsync/,
  );
  assert.match(unix, /AppliedUnverified\(LeaseAcquisitionObligation\)/);
  assert.match(
    block(unix, "pub(crate) struct LeaseHandle"),
    /root:\s*RootGuard[\s\S]*name:\s*OsString[\s\S]*name_class_revision/,
  );
  const unixLeaseValidation = block(unix, "pub(crate) fn validate_lease");
  assert.match(unixLeaseValidation, /validate_lease_binding/);
  ordered(block(unix, "fn validate_lease_binding"), [
    "validate_root(root)",
    "retained_file_identity(handle)",
    "file_binding_state",
    "validate_cached_lease_name_class",
  ]);
  const unixNameClass = block(unix, "fn validate_lease_name_class");
  assert.match(unixNameClass, /LEASE_NAME_CLASS_VALIDATION_ATTEMPTS/);
  assert.equal(unixNameClass.match(/directory_revision/g)?.length, 2);
  const unixNameClassScan = block(unix, "fn observe_lease_name_class");
  assert.match(unixNameClassScan, /visit_entries/);
  assert.match(unixNameClassScan, /MAX_DIRECTORY_LIST_ENTRIES/);
  assert.match(unixNameClassScan, /leaf_names_equal/);
  assert.match(unixNameClassScan, /candidate == name/);
  assert.doesNotMatch(unixNameClassScan, /\blisting\b|Vec|collect/);
  const unixNameClassCache = block(
    unix,
    "fn validate_cached_lease_name_class",
  );
  ordered(unixNameClassCache, [
    "directory_revision(&root.handle)",
    ".read()",
    ".write()",
    "file_binding_state",
    "let revision = directory_revision",
    "observe_lease_name_class",
    "file_binding_state",
    "directory_revision(&root.handle)",
    "*cached_revision = Some(revision)",
  ]);
  const unixControlSync = block(unix, "fn sync_recovery_control_file");
  assert.match(unixControlSync, /target_os = "macos"[\s\S]*full_fsync/);
  assert.match(unixControlSync, /not\(target_os = "macos"\)[\s\S]*rfs::fsync/);
  assert.match(
    block(unix, "pub(crate) fn clear_root_children"),
    /Some\(\(lease_name, lease\.identity\)\)/,
  );
  assert.match(
    block(unix, "pub(crate) fn recovery_control_read_exact_at"),
    /\.read_at\(/,
  );
  assert.match(
    block(unix, "pub(crate) fn recovery_control_write_all_at"),
    /\.write_at\(/,
  );

  const windowsAcquire = block(windows, "pub(crate) fn try_acquire_lease");
  assert.match(windowsAcquire, /FILE_OPEN_IF/);
  assert.match(windowsAcquire, /validate_windows_lease_name_class/);
  assert.match(windowsAcquire, /validate_windows_lease_binding/);
  assert.match(windowsAcquire, /retain_root_proof/);
  ordered(block(windows, "fn validate_windows_lease_binding"), [
    "validate_root(root)",
    "validate_windows_lease_direct_binding",
    "validate_cached_windows_lease_name_class",
  ]);
  const windowsDirectBinding = block(
    windows,
    "fn validate_windows_lease_direct_binding",
  );
  ordered(windowsDirectBinding, [
    "opened_file_path(&root.handle.file)",
    "expected_path.push(name)",
    "query_standard(handle)",
    "object_identity(handle)",
    "opened_file_path(handle)",
  ]);
  assert.match(windowsDirectBinding, /NumberOfLinks\s*!=\s*1/);
  assert.doesNotMatch(windowsDirectBinding, /opened_file_leaf_name/);
  const windowsNameClassScan = block(
    windows,
    "fn observe_windows_lease_name_class",
  );
  assert.match(windowsNameClassScan, /visit_entries/);
  assert.match(windowsNameClassScan, /MAX_DIRECTORY_LIST_ENTRIES/);
  assert.match(windowsNameClassScan, /leaf_names_equal/);
  assert.match(windowsNameClassScan, /candidate == name/);
  assert.doesNotMatch(windowsNameClassScan, /\blisting\b|Vec|collect/);
  const windowsNameClassCache = block(
    windows,
    "fn validate_cached_windows_lease_name_class",
  );
  ordered(windowsNameClassCache, [
    "directory_revision(&root.handle)",
    ".read()",
    ".write()",
    "validate_windows_lease_direct_binding",
    "let revision = directory_revision",
    "observe_windows_lease_name_class",
    "validate_windows_lease_direct_binding",
    "directory_revision(&root.handle)",
    "*cached_revision = Some(revision)",
  ]);
  assert.match(
    block(windows, "pub(crate) struct LeaseHandle"),
    /root:\s*RootGuard[\s\S]*name:\s*OsString[\s\S]*name_class_revision/,
  );
  assert.match(
    block(windows, "pub(crate) fn validate_lease"),
    /validate_windows_lease_binding[\s\S]*lease\.root[\s\S]*lease\.name/,
  );
  const windowsClear = block(windows, "pub(crate) fn clear_root_children");
  assert.match(windowsClear, /validate_windows_lease_binding/);
  assert.doesNotMatch(windowsClear, /file_binding_state|entry_observation/);
  assert.match(
    block(windows, "pub(crate) fn recovery_control_read_exact_at"),
    /\.seek_read\(/,
  );
  assert.match(
    block(windows, "pub(crate) fn recovery_control_write_all_at"),
    /\.seek_write\(/,
  );
  assert.doesNotMatch(
    `${block(unix, "pub(crate) fn recovery_control_read_exact_at")}\n${block(unix, "pub(crate) fn recovery_control_write_all_at")}\n${block(windows, "pub(crate) fn recovery_control_read_exact_at")}\n${block(windows, "pub(crate) fn recovery_control_write_all_at")}`,
    /seek\(|try_clone|PathBuf|OpenOptions/,
  );
  assert.match(
    block(library, "fn try_acquire_lease_and_finish_root"),
    /LeaseAcquisitionOutcome::AppliedUnverified/,
  );
});

test("startup recovery refusal has one preserve-only lease terminal", async () => {
  const [library, recovery, managed] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/recovery.rs"),
    read("core/minecraft/src/managed_fs.rs"),
  ]);

  assert.doesNotMatch(
    library,
    /#\[cfg_attr\(not\(test\), allow\(dead_code\)\)\]\s*mod recovery/,
  );
  assert.match(recovery, /#\[cfg\(test\)\]\s*fn select_recovery_frame/);
  assert.doesNotMatch(
    block(recovery, "struct SelectedRecoveryFrame"),
    /\bside\s*:/,
  );

  const acquire = block(library, "impl RootSessionAcquireObligation");
  const cleanup = block(acquire, "pub fn cleanup");
  assert.match(
    cleanup,
    /acquired_lease\.is_some\(\)[\s\S]*return Err\(self\)/,
  );
  const preserve = block(acquire, "pub fn acknowledge_preserved");
  const acquiredLeasePreservation = preserve.slice(
    preserve.indexOf("if let Some(lease) = self.acquired_lease.take()"),
    preserve.indexOf(
      "if !platform::root_construction_has_unclassified(&construction)",
    ),
  );
  ordered(acquiredLeasePreservation, [
    "root_construction_guard",
    "validate_lease",
    "validate_root",
    "finish_root_construction",
    "process_image.take",
    "drop(lease)",
    "drop(root)",
  ]);
  assert.doesNotMatch(
    acquiredLeasePreservation,
    /cleanup_root_construction|acknowledge_preserved_root_construction|recovery_control_(?:write|sync)|clear_root_children|set_len/,
  );

  const admitted = block(
    library,
    "impl AdmittedRootSessionAcquireObligation",
  );
  assert.match(
    block(admitted, "pub fn acknowledge_preserved"),
    /obligation\.acknowledge_preserved\(\)/,
  );
  for (const settlement of [
    block(managed, "fn settle_root_session_acquisition"),
    block(managed, "fn settle_admitted_root_session_acquisition"),
  ]) {
    ordered(settlement, ["obligation.cleanup()", "acknowledge_preserved()"]);
  }
});

test("one bounded recovery journal owns canonical restart records", async () => {
  const recovery = await read("core/fs/src/recovery.rs");
  const record = block(recovery, "pub(crate) struct RecoveryRecord");
  for (const field of [
    "operation_id",
    "phase",
    "destination_parent",
    "destination_leaf",
    "old",
    "new",
  ]) {
    assert.match(record, new RegExp(`${field}:`));
  }
  assert.doesNotMatch(record, /source_parent|source_leaf|PathBuf|Identity/);
  assert.match(recovery, /MAX_RECOVERABLE_FILE_BYTES:\s*u64\s*=\s*16\s*\*\s*1024\s*\*\s*1024/);
  assert.match(recovery, /MAX_LIVE_PROOF_BYTES:\s*u64\s*=\s*128\s*\*\s*1024\s*\*\s*1024/);
  assert.match(recovery, /MAX_RECORD_PAYLOAD_BYTES[\s\S]*assert!\(HEADER_BYTES \+ MAX_RECORD_PAYLOAD_BYTES <= FRAME_BODY_BYTES\)/);
  assert.match(recovery, /\.axial-rstage-/);
  assert.match(recovery, /\.axial-rpark-/);

  const journal = block(recovery, "impl RecoveryJournal");
  for (const method of [
    "load",
    "reconcile_uncertain",
    "reserve",
    "create_reserved",
    "advance",
    "clear",
    "has_live_or_uncertain",
    "is_uncertain",
    "record",
    "records",
  ]) {
    assert.match(journal, new RegExp(`fn ${method}\\s*\\(`));
  }
  ordered(block(journal, "fn write_with"), [
    "validate_candidate",
    "encode_recovery_frame",
    "write(offset, &encoded)",
    "barrier_confirmed",
    "reconcile_confirmed_write",
    "read(offset, &mut observed)",
    "decode_recovery_frame",
  ]);
  const load = block(journal, "fn load_initialized");
  assert.equal(
    load.match(/recovery_control_read_exact_at/g)?.length,
    1,
    "recovery load must perform one binding-validated bulk read",
  );
  assert.match(load, /vec!\[0; control_len\]/);
  assert.match(load, /recovery_control_read_exact_at\(lease, 0, &mut control\)/);
  assert.match(load, /recovery_frame_region\(&control/);
  assert.match(load, /RECOVERY_REGION_BYTES[\s\S]*control\[reserved_start\.\.\]/);
  const pending = block(recovery, "struct UncertainRecoveryWrite");
  assert.match(pending, /registration:\s*RecoveryRegistration/);
  assert.match(pending, /predecessor:\s*Option<RecoveryFrame>/);
  assert.match(pending, /intended:\s*RecoveryFrame/);
  const reconcile = block(journal, "fn reconcile_uncertain");
  ordered(reconcile, [
    "recovery_control_sync",
    "read_uncertain_selection",
    "UncertainRecoverySelection::Intended",
    "UncertainRecoverySelection::Predecessor",
    "recovery_control_write_all_at",
    "recovery_control_sync",
    "read_uncertain_selection",
    "accept_uncertain_intended",
  ]);
  assert.match(
    block(recovery, "fn validate_uncertain_control"),
    /pending\.predecessor[\s\S]*pending\.intended[\s\S]*validate_lane_slots/,
  );
  assert.match(
    recovery,
    /fn failed_write_and_unconfirmed_sync_never_reload_cached_bytes_as_durable/,
  );
  assert.match(
    recovery,
    /fn uncertain_readback_accepts_only_the_exact_predecessor_or_intended_lane/,
  );
  assert.match(
    recovery,
    /fn ordinary_post_sync_readback_failure_reloads_the_exact_durable_frame/,
  );
  assert.doesNotMatch(
    recovery,
    /pub\(crate\) (?:struct|fn) (?:RecoveryFrameAddress|SelectedRecoveryFrame|RecoveryLaneBootstrap|encode_recovery_frame|decode_recovery_frame|recovery_frame_offset)/,
  );
  assert.match(
    recovery,
    /RecoveryPhase::StagePrepared\s*=>\s*self\.new\.is_none\(\)/,
  );
  assert.match(
    recovery,
    /RecoveryPhase::StageSealed\s*=>\s*self\.new\.is_some\(\)/,
  );
  assert.match(
    recovery,
    /previous\.old\s*==\s*next\.old[\s\S]*previous\.new\.is_none_or/,
  );
});

test("replay revalidates the retained root-relative chain around effects", async () => {
  const replay = await read("core/fs/src/recovery_runtime.rs");
  const binding = block(replay, "struct RetainedParentBinding");
  assert.match(binding, /parent:\s*platform::DirectoryHandle/);
  assert.match(binding, /parent_identity:\s*platform::Identity/);
  assert.match(binding, /name:\s*RecoveryName/);
  assert.match(binding, /child_identity:\s*platform::Identity/);

  const validate = block(replay, "fn validate_parent_chain");
  ordered(validate, [
    "platform::validate_root(root)",
    "platform::clone_root(root)",
    "platform::entries",
    "leaf_names_equivalent",
    "platform::directory_binding_state",
    "platform::directory_identity(retained_child)",
    "platform::directory_identity(&plan.parent)",
  ]);
  const publication = block(replay, "fn replay_publication");
  ordered(publication, [
    "validate_parent_chain(root, plan)",
    "journal.advance",
    "platform::prepare_publication",
    "validate_parent_chain(root, plan)",
    "platform::rename_no_replace",
    "validate_parent_chain(root, plan)",
    "platform::settle_publication",
  ]);
  assert.match(
    publication,
    /platform::settle_publication[\s\S]*validate_parent_chain\(root, plan\)[\s\S]*journal\.(?:clear|advance)/,
  );
  const removal = block(replay, "fn remove_stage");
  ordered(removal, [
    "platform::clone_stage_cleanup",
    "validate_parent_chain(root, plan)",
    "platform::remove_parked_file",
  ]);
  const replayLoop = block(replay, "fn replay(");
  assert.match(
    replayLoop,
    /remove_stage\(root, plan\)[\s\S]*validate_parent_chain\(root, plan\)[\s\S]*sync_publication_directory[\s\S]*validate_parent_chain\(root, plan\)[\s\S]*journal\.clear/,
  );
  assert.match(replay, /run_replay_parent_validation_hook\(\)[\s\S]*replay\(root,/);
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
    "stages.iter()",
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
  assert.match(reservation, /excluded_recovery_target_stage/);
  assert.match(reservation, /recovery\.phase\.owns_target\(\)/);
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
    "lease_name_class_scan_cache_invalidates_on_root_revision_change",
    "root_lease_rejects_a_noncanonical_portable_alias_before_acquisition",
    "windows_root_lease_accepts_exact_and_rejects_noncanonical_spelling",
    "retained_root_lease_rejects_a_new_portable_alias_before_control_io",
    "windows_exclusive_recovery_control_prevents_root_displacement",
    "windows_exclusive_recovery_control_prevents_binding_substitution",
    "windows_recovery_control_reports_short_native_eof",
    "windows_exclusive_lease_prevents_substitution_before_root_clear",
    "replay_parent_swap_after_planning_retains_the_record_until_binding_restoration",
    "recovery_control_refuses_a_displaced_root_and_replacement_lease",
    "recovery_control_initializes_only_zero_length_and_rejects_corrupt_lengths",
    "recovery_failure_preserves_root_artifacts_and_releases_the_lease",
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
