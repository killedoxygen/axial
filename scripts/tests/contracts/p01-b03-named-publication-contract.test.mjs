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
  assert.match(
    block(unix, "pub(crate) fn settle_recovery_publication"),
    /settle_publication_observation[\s\S]*true/,
  );
  assert.match(
    settleObservation,
    /PublicationReceiptState::Poisoned\)\s*&&\s*!retry_poisoned/,
  );
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
  assert.match(
    block(windows, "pub(crate) fn settle_recovery_publication"),
    /settle_publication\(/,
  );
});

test("root leases retain one fixed positional recovery control", async () => {
  const [library, platform, recovery, control] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
    read("core/fs/src/recovery.rs"),
    read("core/fs/src/control_frame.rs"),
  ]);
  const unix = platform.slice(
    platform.indexOf("#[cfg(unix)]\nmod native {"),
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const sharedExactRead = block(
    platform,
    "fn recovery_control_read_exact_with",
  );
  assert.match(sharedExactRead, /Ok\(0\)[\s\S]*UnexpectedEof/);
  assert.doesNotMatch(sharedExactRead, /bytes\.fill\(0\)/);
  assert.match(
    block(platform, "fn recovery_control_write_all_with"),
    /Ok\(0\)[\s\S]*WriteZero/,
  );
  assert.match(control, /FRAME_BYTES:\s*usize\s*=\s*16\s*\*\s*1024/);
  assert.match(
    recovery,
    /RECOVERY_FRAME_BYTES:\s*usize\s*=\s*control_frame::FRAME_BYTES[\s\S]*RECOVERY_REGION_BYTES:\s*u64[\s\S]*SUCCESSOR_AGGREGATE_SLOT_COUNT:\s*usize\s*=\s*64[\s\S]*RECOVERY_CONTROL_BYTES:\s*u64\s*=\s*RECOVERY_REGION_BYTES/,
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
      /if length == 0\s*\{[\s\S]*set_len\(RECOVERY_CONTROL_BYTES\)[\s\S]*(?:sync_recovery_control_file|sync_all)[\s\S]*\}\s*else if length != RECOVERY_CONTROL_BYTES/,
    );
    assert.match(initialize, /length == 0/);
    assert.match(initialize, /length != RECOVERY_CONTROL_BYTES/);
    assert.doesNotMatch(initialize, /length < RECOVERY_CONTROL_BYTES/);
    const exactRead = block(
      native,
      "pub(crate) fn recovery_control_read_exact_at",
    );
    assert.match(exactRead, /recovery_control_read_exact_with/);
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
    /recovery_control_write_all_with[\s\S]*\.write_at\(/,
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
    /recovery_control_write_all_with[\s\S]*\.seek_write\(/,
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
  assert.match(recovery, /fn select_recovery_frame/);
  assert.doesNotMatch(
    block(recovery, "struct SelectedRecoveryFrame"),
    /\bside\s*:/,
  );

  const acquire = block(library, "impl RootSessionAcquireObligation");
  const cleanup = block(acquire, "pub fn cleanup");
  assert.match(cleanup, /acquired\.is_some\(\)[\s\S]*return Err\(self\)/);
  const preserve = block(acquire, "pub fn acknowledge_preserved");
  const acquiredPreservation = preserve.slice(
    preserve.indexOf("if let Some(mut acquired) = self.acquired.take()"),
    preserve.indexOf(
      "if !platform::root_construction_has_unclassified(&construction)",
    ),
  );
  ordered(acquiredPreservation, [
    "root_construction_guard",
    "validate_lease",
    "validate_root",
    "finish_root_construction",
    "replay.acknowledge()",
    "process_image.take",
    "drop(acquired.lease)",
    "drop(root)",
  ]);
  assert.doesNotMatch(
    acquiredPreservation,
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
    "create_reserved",
    "advance",
    "clear",
    "has_live_or_uncertain",
    "has_live_successor",
    "is_uncertain",
    "record",
    "records",
  ]) {
    assert.match(journal, new RegExp(`fn ${method}\\s*\\(`));
  }
  ordered(block(journal, "fn write_with"), [
    "validate_candidate",
    "self.pending = Some(",
    "PendingWrite::Recovery",
    "encode_recovery_frame",
    "self.drive_pending",
  ]);
  const drive = block(journal, "fn drive_pending");
  ordered(drive, [
    "self.pending_io()",
    "write(offset, encoded)?",
    "sync()",
    "settle_pending_reload",
    "read(offset, &mut observed)",
    "observed == *encoded",
    "self.accept_pending()",
  ]);
  const load = block(journal, "fn load_initialized");
  assert.match(
    load,
    /initialize_if_absent\s*&&\s*control\.iter\(\)\.all[\s\S]*recovery_control_sync\(lease\)[\s\S]*recovery_control_read_exact_at\(lease, 0, &mut control\)/,
    "a pristine exact-length control still needs a confirmed barrier and readback before admission",
  );
  assert.equal(
    load.match(/recovery_control_read_exact_at/g)?.length,
    2,
    "recovery load performs one bulk read plus one pristine-control readback",
  );
  assert.match(load, /vec!\[0; control_len\]/);
  assert.match(load, /recovery_control_read_exact_at\(lease, 0, &mut control\)/);
  assert.match(load, /decode_control\(&control, None\)/);
  const journalOwner = block(recovery, "pub(crate) struct RecoveryJournal");
  assert.match(journalOwner, /physical:\s*\[\[Option<RecoveryFrameReceipt>/);
  assert.match(journalOwner, /successors:\s*\[Option<SelectedSuccessorFrame>/);
  assert.match(journalOwner, /pending:\s*Option<PendingWrite>/);
  const pending = block(recovery, "enum PendingWrite");
  assert.match(pending, /Recovery\s*\{[\s\S]*offset:\s*u64[\s\S]*encoded:\s*Box/);
  assert.match(pending, /Successor\s*\{[\s\S]*offset:\s*u64[\s\S]*encoded:\s*Box/);
  const reconcile = block(journal, "fn reconcile_uncertain");
  ordered(reconcile, [
    "recovery_control_sync",
    "read_control(lease)",
    "decode_control(&control, Some(self))",
    "accept_pending_control",
    "write_pending(lease)",
  ]);
  const decode = block(recovery, "fn decode_control");
  assert.match(decode, /Sha256::digest\(raw\[side\]\)/);
  assert.match(decode, /select_successor_frame/);
  assert.match(decode, /receipt\.digest\s*!=\s*acknowledgement\.frame_sha256/);
  assert.match(decode, /selected\.generation\s*==\s*acknowledgement\.recovery_generation \+ 1[\s\S]*selected\.record\.is_none/);
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
    /struct (?:UncertainRecoveryWrite|UncertainSuccessorWrite)|enum (?:UncertainRecoverySelection|PendingSelection)/,
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

test("one bounded successor engine shares framing, uncertainty, pins, and State replay", async () => {
  const [control, successor, recovery, replay] = await Promise.all([
    read("core/fs/src/control_frame.rs"),
    read("core/fs/src/successor.rs"),
    read("core/fs/src/recovery.rs"),
    read("core/fs/src/recovery_runtime.rs"),
  ]);
  const productionLines = (source, marker) => {
    const end = marker ? source.indexOf(marker) : source.length;
    assert.notEqual(end, -1, `missing production boundary ${marker}`);
    return source.slice(0, end).trimEnd().split("\n").length;
  };
  const ledger =
    productionLines(control) +
    productionLines(successor, "#[cfg(test)]\nmod tests") +
    (productionLines(recovery, "#[cfg(test)]\nmod tests") - 1440) +
    (productionLines(replay, "#[cfg(test)]\nmod admission_tests") - 2407);
  assert.ok(
    ledger <= 1050,
    `successor engine and State replay grew to ${ledger} production lines`,
  );

  const domain = block(control, "impl Domain");
  assert.match(domain, /AXRECV01[\s\S]*recovery-frame\.v1[\s\S]*AXSUCC01[\s\S]*successor-frame\.v1/);
  const probe = block(control, "pub(crate) fn probe");
  ordered(probe, [
    "bytes.iter().all",
    "checksum(domain, body, declared_side)",
    "body[..8] == domain.parts().0",
    "Address::new",
    "body[30..36].iter().all",
    "generation_side(generation)",
    "body[48..HEADER_BYTES].iter().all",
    "body[end..].iter().all",
  ]);

  const create = block(recovery, "fn create_successor");
  assert.match(create, /Result<SuccessorOwner,\s*\(io::Error, Option<SuccessorOwner>\)>/);
  ordered(create, [
    "self.successor_pending(slot, frame)?",
    "SuccessorOwner(Some((slot, generation, transfer)))",
    "self.pending = Some(pending)",
    "self.write_pending(lease)",
  ]);
  const tombstone = block(recovery, "fn tombstone_successor");
  ordered(tombstone, [
    "let completing = matches!",
    "self.reconcile_uncertain(lease)",
    "if completing",
    "ack.recovery_generation + 1",
    "self.successor_pending",
    "self.pending = Some",
    "self.write_pending(lease)",
    "owner.0 = None",
  ]);
  assert.match(block(recovery, "impl Drop for SuccessorOwner"), /process::abort/);
  const recoveryWrite = block(recovery, "fn validate_recovery_transition");
  ordered(recoveryWrite, [
    "recovery_pin_side",
    "frame.record.is_some()",
    "pin != selected_side",
    "pin == write_side",
  ]);

  const resume = block(replay, "pub(crate) fn resume(");
  ordered(resume, [
    "reconcile_uncertain(lease)",
    "has_live_successor()",
    "settle_replay_state_removals",
    "plan_replay",
  ]);
  for (const regression of [
    "combined_control_binds_successor_to_the_exact_recovery_predecessor",
    "first_successor_torn_write_retries_but_intact_invalid_frame_is_fatal",
    "successor_owner_retries_uncertainty_and_releases_the_exact_pin",
    "armed_successor_owner_drop_aborts",
  ]) {
    assert.match(recovery, new RegExp(`fn ${regression}\\s*\\(`));
  }
  assert.match(successor, /recovery_generation\s*=\s*u64::MAX - 1/);
});

test("replay revalidates the retained root-relative chain around effects", async () => {
  const replay = await read("core/fs/src/recovery_runtime.rs");
  const directory = block(replay, "struct RetainedDirectory");
  assert.match(directory, /handle:\s*platform::DirectoryHandle/);
  assert.match(directory, /identity:\s*platform::Identity/);
  assert.match(
    directory,
    /stamp:\s*RwLock<Option<platform::DirectoryStamp>>/,
  );
  assert.match(
    directory,
    /parent:\s*Option<\(Arc<RetainedDirectory>,\s*RecoveryName\)>/,
  );

  const validate = block(replay, "fn validate_retained_parent_chain");
  ordered(validate, [
    "platform::validate_root(root)",
    "platform::clone_root(root)",
    "platform::directory_revision(parent)",
    "platform::directory_binding_state",
    "platform::directory_identity(child)",
  ]);
  assert.doesNotMatch(validate, /platform::(?:entries|visit_entries)/);
  const scan = block(replay, "fn scan_parent");
  assert.equal((scan.match(/platform::visit_entries/g) ?? []).length, 1);
  ordered(scan, [
    "physical.insert(directory.identity)",
    "platform::directory_revision(&directory)",
    "platform::visit_entries",
    "platform::directory_revision(&directory)",
    "platform::open_directory",
    "platform::directory_binding_state",
  ]);
  const refresh = block(replay, "fn refresh_parent_after_effect");
  assert.equal((refresh.match(/platform::visit_entries/g) ?? []).length, 1);
  assert.match(refresh, /Arc::ptr_eq/);
  assert.match(refresh, /Expected::Child/);
  assert.match(refresh, /ObservedEntry::UnownedOccupied/);
  ordered(refresh, [
    "platform::visit_entries",
    "parent.update_stamp(revision)",
    "validate_parent_chain(root, plan)",
  ]);

  const rename = block(replay, "fn rename_replay_file");
  ordered(rename, [
    "platform::rename_recovery_file_no_replace",
    "std::mem::replace",
    "refresh_moved_file",
    "refresh_parent_after_effect",
    "platform::settle_renamed_recovery_file",
    "validate_plan_snapshot",
  ]);
  const removal = block(replay, "fn remove_replay_file");
  ordered(removal, [
    "platform::remove_recoverable_stage",
    "std::mem::replace",
    "pending_removals.push",
    "settle_pending_removals",
  ]);
  const pending = block(replay, "struct PendingRemoval");
  assert.match(pending, /registration:\s*RecoveryRegistration/);
  assert.match(pending, /coordinate:\s*ReplayCoordinate/);
  assert.match(pending, /file:\s*ObservedFile/);
  assert.doesNotMatch(pending, /parent:|name:/);
  ordered(block(replay, "fn settle_pending_removal"), [
    "exactly_one",
    "ObservedEntry::Absent",
    "refresh_parent_after_effect",
    "platform::settle_removed_recoverable_stage",
    "validate_plan_snapshot",
  ]);
  const publication = block(replay, "fn publish_replay_stage");
  ordered(publication, [
    "platform::prepare_publication",
    "platform::rename_recovery_publication_no_replace",
    "std::mem::replace",
    "refresh_moved_file",
    "refresh_parent_after_effect",
    "settle_pending_recovery_publication",
  ]);
  const advance = block(replay, "fn advance_replay_phase");
  ordered(advance, [
    "validate_plan_snapshot",
    "journal.advance",
    "plan.record = intended",
    "ObservedEntry::Unowned",
    "validate_plan_snapshot",
  ]);
  const replacement = block(replay, "fn replay_replacement");
  assert.match(replacement, /replacement_action\(&plans\[index\]\)/);
  for (const action of [
    "ParkTarget",
    "PublishStage",
    "RestorePark",
    "RemovePark",
    "RemoveStage",
    "NoEffect",
    "Applied",
  ]) {
    assert.match(replacement, new RegExp(`ReplacementAction::${action}`));
  }
  assert.doesNotMatch(replacement, /ReplacementCarrier::/);
  assert.match(replay, /run_replay_parent_validation_hook\(\)[\s\S]*replay\(root,/);
});

test("replay admission is single-scan, bounded, and linearly retained", async () => {
  const replay = await read("core/fs/src/recovery_runtime.rs");
  assert.match(
    replay,
    /must_use\s*=\s*"failed recovery admission retains every partially opened carrier"/,
  );

  const open = block(replay, "fn open_observed");
  ordered(open, [
    "platform::open_file(parent, name)",
    "platform::file_identity",
    "platform::file_binding_state",
    "platform::file_receipt_fields",
    "platform::open_recoverable_stage",
    "partial_carriers.push",
    "platform::file_identity(&retained_file.handle)",
    "platform::file_receipt_fields(&retained_file.handle)",
  ]);
  const exclusiveOpen = open.indexOf("platform::open_recoverable_stage");
  assert.match(
    open.slice(0, exclusiveOpen),
    /partial_carriers\.push\([\s\S]*exclusive:\s*false/,
  );
  assert.match(
    open.slice(exclusiveOpen),
    /partial_carriers\.push\([\s\S]*exclusive:\s*true[\s\S]*take_replay_exclusive_admission_failure[\s\S]*platform::file_identity/,
  );
  assert.match(replay, /enum ReplayState\s*\{[\s\S]*Admit[\s\S]*Replan/);
  const resume = block(replay, "pub(crate) fn resume(");
  ordered(resume, [
    "platform::validate_lease(lease)",
    "reconcile_uncertain(lease)",
    "has_live_successor()",
    "settle_replay_state_removals",
    "align_retained",
    "records().next().is_none()",
    "admit_replay_planning_attempt",
    "plan_replay",
    "transfer_retained_authority",
    "partial.absorb(retained)",
    "replay(root, lease",
    "into_orphans",
  ]);
  const transfer = block(replay, "fn transfer_retained_authority");
  const commit = transfer.indexOf("for transfer in transfers");
  assert.ok(commit > 0, "transfer commit pass must remain explicit");
  assert.doesNotMatch(transfer.slice(0, commit), /\.take\(\)/);
  ordered(transfer.slice(0, commit), [
    "source_file.receipt != destination.receipt",
    "platform::file_identity",
    "platform::file_receipt_fields",
    "validate_recovery_binding",
  ]);
  assert.match(transfer.slice(commit), /publication\.take\(\)/);
  const replayDrop = block(replay, "impl Drop for RecoveryReplay");
  assert.match(replayDrop, /inner\.is_some\(\)[\s\S]*process::abort/);
  const proof = block(replay, "fn prove_file_with_receipt");
  ordered(proof, [
    "platform::file_receipt_fields(file)",
    "expected_receipt != before",
    "platform::read_at",
  ]);

  assert.match(
    block(replay, "fn scan_parent"),
    /replacement recovery retains more than two physical carriers/,
  );
  const plan = block(replay, "fn plan_replay_records");
  ordered(plan, [
    "recovery records alias one physical file",
    "recovery proof lane exceeds its byte bound",
    "prove_file_with_receipt",
    "classify_replacement",
  ]);
  const projection = block(replay, "fn projected_names");
  assert.match(projection, /record\s*\.phase\s*\.owns_target\(\)/);
  assert.match(
    projection,
    /record\.old\.is_some\(\)\s*&&\s*record\.phase\.owns_park\(\)/,
  );
  const effect = block(replay, "fn replay(");
  ordered(effect, [
    "for index in 0..plans.len()",
    "plans[index].record.old.is_some()",
    "replay_replacement",
  ]);
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

test("State successors are domain-admitted before pre-session replay", async () => {
  const [library, recovery, runtime, config, journals, bootstrap] =
    await Promise.all([
      read("core/fs/src/lib.rs"),
      read("core/fs/src/recovery.rs"),
      read("core/fs/src/recovery_runtime.rs"),
      read("core/config/src/root.rs"),
      read("apps/api/src/state/journals.rs"),
      read("apps/api/src/bootstrap.rs"),
    ]);

  const descriptor = block(recovery, "pub(crate) struct StateSuccessorDescriptor");
  for (const field of [
    "owner_schema",
    "owner_id",
    "old_payload",
    "new_payload",
    "recoveries",
  ]) {
    assert.match(descriptor, new RegExp(`${field}:`));
  }
  const stateSuccessor = block(recovery, "pub(crate) fn state_successor");
  assert.match(stateSuccessor, /SuccessorOwnerClass::State/);
  assert.match(
    stateSuccessor,
    /RecoveryPhase::RemovePrepared\s*\|\s*RecoveryPhase::RemoveCommitted/,
  );
  ordered(stateSuccessor, [
    "self.physical",
    "acknowledgement.recovery_side",
    "receipt.frame.record",
    "receipt.frame.generation",
    "recovery.operation_id",
    "recoveries.push",
  ]);

  const replay = block(runtime, "pub(crate) fn resume_state_successor");
  ordered(replay, [
    "validate_lease",
    "reconcile_uncertain",
    "settle_replay_state_removals",
    "claim_state_successor",
    "plan_replay_records",
    "transfer_retained_authority",
    "ReplayMode::Successor",
    "tombstone_successor",
    "self.resume(root, lease)",
  ]);
  assert.match(replay, /ReplayState::Successor/);
  assert.match(replay, /effects_complete/);

  const token = block(library, "pub struct RootStateSuccessor");
  assert.match(token, /descriptor:\s*StateSuccessorDescriptor/);
  assert.doesNotMatch(token, /pub\s+descriptor|Clone|Copy/);
  const reconcile = block(library, "pub fn reconcile_state_successor");
  ordered(reconcile, [
    "replay.state_successor()",
    "Some(&successor.descriptor)",
    "root_construction_identity",
    "root_construction_guard",
    "replay.resume_state_successor",
    "finish_root_session_with_recovery",
  ]);

  const acquire = block(config, "fn acquire_root_session_with_state_successor");
  ordered(acquire, [
    "obligation.state_successor()",
    "admit(&successor)",
    "obligation.reconcile_state_successor(successor)",
  ]);
  assert.match(acquire, /acknowledge_preserved/);
  assert.doesNotMatch(acquire, /obligation\.cleanup\(\)/);

  const admission = block(journals, "pub(crate) fn admit_operation_journal_successor");
  for (const marker of [
    "OPERATION_JOURNAL_SUCCESSOR_SCHEMA",
    "OPERATION_JOURNAL_SUCCESSOR_OWNER",
    "OPERATION_JOURNAL_SNAPSHOT_NAME",
    "successor_payload_matches_proof",
  ]) {
    assert.match(admission, new RegExp(marker));
  }
  const payload = block(journals, "fn successor_payload_matches_proof");
  ordered(payload, ["size.to_le_bytes()", "copy_from_slice(&sha256)", "payload == expected"]);
  assert.match(
    block(bootstrap, "pub fn open_app_root_session"),
    /open_root_session_with_state_successor\(crate::state::admit_operation_journal_successor\)/,
  );
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
    "replacement_replay_completes_the_full_commit_sequence",
    "replacement_replay_cancels_without_touching_a_foreign_target",
    "replacement_prepared_stage_cleanup_ignores_the_user_target",
    "replacement_replay_restores_an_interrupted_park",
    "equal_proof_replacement_prefers_the_stage_carrier",
    "replacement_replay_retries_settlement_after_park_removal",
    "replacement_publication_retries_a_poisoned_recovery_receipt",
    "remove_committed_replay_finishes_the_durable_desired_state",
    "create_only_remove_committed_republishes_its_stage",
    "create_only_and_replacement_replay_share_one_parent_snapshot",
    "admitted_state_successor_replays_before_root_session_exposure",
    "state_successor_retains_completed_effects_until_tombstone_retry",
    "state_successor_admission_cannot_cross_root_lineage",
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
