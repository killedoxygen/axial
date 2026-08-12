import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) =>
  readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

const between = (source, start, end) => {
  const first = source.indexOf(start);
  const last = source.indexOf(end, first + start.length);
  assert.notEqual(first, -1, `missing section start: ${start}`);
  assert.notEqual(last, -1, `missing section end: ${end}`);
  return source.slice(first, last);
};

const occurrences = (source, needle) => {
  const positions = [];
  for (
    let offset = source.indexOf(needle);
    offset >= 0;
    offset = source.indexOf(needle, offset + 1)
  ) {
    positions.push(offset);
  }
  return positions;
};

test("P01-B01 has one typed portable path and identity owner", async () => {
  const [
    workspace,
    minecraftManifest,
    library,
    portable,
    managedFs,
    runtimeDownload,
    manifest,
    contentModel,
    install,
    managedTransaction,
    transaction,
    pack,
    applicationPack,
    resources,
    installFlight,
    screenshotActions,
    worldActions,
    performancePlan,
    performanceState,
    performanceMutation,
    fsPlatform,
    architecture,
    contentAdr,
  ] = await Promise.all([
    read("Cargo.toml"),
    read("core/minecraft/Cargo.toml"),
    read("core/minecraft/src/lib.rs"),
    read("core/minecraft/src/portable_path.rs"),
    read("core/minecraft/src/managed_fs.rs"),
    read("core/minecraft/src/runtime/file_download.rs"),
    read("core/content/src/manifest.rs"),
    read("core/content/src/model.rs"),
    read("core/content/src/install.rs"),
    read("core/content/src/managed_transaction.rs"),
    read("core/content/src/transaction.rs"),
    read("core/content/src/pack.rs"),
    read("apps/api/src/application/content/pack.rs"),
    read("apps/api/src/application/instances/resources.rs"),
    read("core/minecraft/src/loaders/install_flight.rs"),
    read("frontend/src/views/instance/screenshot-actions.ts"),
    read("frontend/src/views/instance/world-actions.ts"),
    read("core/performance/src/install/plan.rs"),
    read("core/performance/src/state/mod.rs"),
    read("core/performance/src/install/mutation.rs"),
    read("core/fs/src/platform.rs"),
    read("docs/ARCHITECTURE.md"),
    read("docs/adr/0002-content-discovery-and-provenance.md"),
  ]);

  await assert.rejects(
    access(
      new URL("../../../core/minecraft/src/artifact_path.rs", import.meta.url),
    ),
  );
  assert.match(workspace, /^unicode-casefold = "=0\.2\.0"$/m);
  assert.match(workspace, /^unicode-normalization = "=0\.1\.25"$/m);
  assert.match(workspace, /^dirs = "=6\.0\.0"$/m);
  assert.match(minecraftManifest, /^unicode-casefold\.workspace = true$/m);
  assert.match(minecraftManifest, /^unicode-normalization\.workspace = true$/m);
  assert.match(library, /^pub mod portable_path;$/m);

  for (const type of [
    "PortableFileName",
    "PortableRelativePath",
    "PortablePathKey",
  ]) {
    assert.match(portable, new RegExp(`pub struct ${type}\\b`));
  }
  assert.match(portable, /let spelling = nfc\(value\);/);
  assert.match(portable, /pub fn new_exact\(value: &str\)/);
  assert.match(portable, /value\.case_fold\(\)\.collect::<String>\(\)/);
  assert.match(portable, /folded\.as_str\(\)\.nfc\(\)\.collect\(\)/);
  assert.doesNotMatch(
    portable,
    /to_(?:ascii_)?lowercase|flat_map\(char::to_lowercase\)/,
  );
  assert.match(portable, /MAX_PORTABLE_FILE_NAME_BYTES: usize = 255/);
  assert.match(portable, /MAX_PORTABLE_RELATIVE_PATH_BYTES: usize = 512/);
  assert.match(portable, /'\\u\{00b9\}' \| '\\u\{00b2\}' \| '\\u\{00b3\}'/);
  assert.match(
    portable,
    /while let Some\(enabled\) = base\.strip_suffix\(DISABLED_SUFFIX\)/,
  );
  assert.match(portable, /pub fn managed_content_name_key/);

  assert.match(managedFs, /PortableFileName::new_exact\(name\)/);
  assert.match(managedFs, /PortableRelativePath::new_exact\(&authored\)/);
  assert.doesNotMatch(managedFs, /eq_ignore_ascii_case\(park_name\)/);
  assert.match(
    runtimeDownload,
    /PortableRelativePath::new_exact\(relative_path\)/,
  );

  for (const consumer of [install, pack, resources]) {
    assert.match(consumer, /Portable(?:FileName|RelativePath|PathKey)/);
  }
  assert.match(manifest, /ManagedContentFileName/);
  assert.doesNotMatch(manifest, /entry\.filename\.to_ascii_lowercase\(\)/);
  assert.match(
    contentModel,
    /pub struct ManagedContentFileName \{[\s\S]*?enabled: PortableFileName,[\s\S]*?disabled: PortableFileName,/,
  );
  assert.match(contentModel, /PortableFileName::new_exact\(value\)/);
  assert.match(
    contentModel,
    /let disabled = filename\.with_suffix\("\.disabled"\)\?;/,
  );
  assert.match(contentModel, /pub fn disabled\(&self\) -> &PortableFileName/);
  assert.match(
    contentModel,
    /impl<'de> Deserialize<'de> for ManagedContentFileName/,
  );
  assert.match(manifest, /filename: Option<ManagedContentFileName>/);
  assert.doesNotMatch(manifest, /pub filename: String/);
  assert.doesNotMatch(manifest, /manifest_filename/);
  assert.doesNotMatch(manifest, /pub fn filename\(&self\) -> &str/);
  assert.doesNotMatch(manifest, /managed_admitted/);
  assert.match(manifest, /struct ManifestEntryWire/);
  assert.match(manifest, /struct ContentManifestWire/);
  assert.match(manifest, /pub struct PendingManifestEntry/);
  assert.match(manifest, /pub fn validate_provider_pending_projection/);
  assert.match(manifest, /pub fn try_upsert_batch/);
  const manifestBatch = between(
    manifest,
    "pub fn try_upsert_batch",
    "pub fn try_set_enabled",
  );
  assert.ok(
    manifestBatch.indexOf("if additions.len() > MAX_MANIFEST_ENTRIES") <
      manifestBatch.indexOf("HashSet::with_capacity(additions.len())"),
    "manifest batch cardinality must be rejected before allocation and entry validation",
  );
  assert.match(manifest, /pub\(crate\) fn save_with_revalidation/);
  assert.match(manifest, /pub fn managed\([\s\S]*?\) -> ContentResult<Self>/);
  assert.match(manifest, /MANIFEST_SCHEMA_VERSION: u32 = 3/);
  assert.doesNotMatch(manifest, /MANIFEST_SCHEMA_VERSION: u32 = 2/);
  assert.match(install, /file: PlannedArtifact/);
  assert.doesNotMatch(install, /pub file: FileRef/);
  assert.doesNotMatch(install, /struct InstallDestination\b/);
  assert.match(
    managedTransaction,
    /pub struct ManagedContentOperationProjection \{[\s\S]*?effects: Vec<ManagedContentPathMutation>/,
  );
  assert.match(
    managedTransaction,
    /pub fn effect_paths\(&self\) -> Vec<PortableRelativePath> \{[\s\S]*?effect\.path\(\)\.clone\(\)/,
  );
  assert.match(
    managedTransaction,
    /struct ProjectedMutation \{[\s\S]*?results: HashMap<PortablePathKey, ManagedContentPathResult>/,
  );
  assert.match(
    managedTransaction,
    /let mut owners = HashMap::<PortablePathKey, &ManifestEntry>::new\(\)/,
  );
  assert.match(
    managedTransaction,
    /let mut future_owner = HashMap::<PortablePathKey, &PlannedFile>::new\(\)/,
  );
  assert.doesNotMatch(
    managedTransaction,
    /HashMap<String,\s*(?:ManagedContentPathResult|&ManifestEntry|&PlannedFile)>/,
  );
  assert.match(
    install,
    /pub struct ManagedRemoval \{[^}]*relative: PortableRelativePath/,
  );
  assert.match(
    install,
    /pub struct ProtectedManagedPaths \{[\s\S]*?keys: HashSet<PortablePathKey>/,
  );
  const removalPreflight = between(
    install,
    "pub fn verified_removable_variants",
    "fn managed_variant_paths",
  );
  assert.match(removalPreflight, /protected_paths\.contains\(&relative\)/);
  assert.doesNotMatch(removalPreflight, /protected_paths\s*\.iter\(\)/);
  assert.match(install, /present: bool/);
  assert.doesNotMatch(install, /fn managed_path_identity\([^)]*\) -> String/);
  assert.doesNotMatch(install, /fn managed_mod_candidates/);
  assert.doesNotMatch(install, /fn manifest_mod_candidates/);
  assert.doesNotMatch(install, /stage_managed_removals|FileTransaction/);
  assert.match(managedTransaction, /manifest\.try_upsert_batch\(entries\)/);
  assert.match(managedTransaction, /pub fn managed_mod_toggle_observation_paths/);
  assert.match(managedTransaction, /pub fn plan_managed_mod_toggle/);
  assert.match(managedTransaction, /pub fn managed_mod_delete_observation_paths/);
  assert.match(managedTransaction, /pub fn plan_managed_mod_delete/);
  assert.match(managedTransaction, /ProjectedPayload::Local/);
  assert.doesNotMatch(install, /pub fn (?:toggle_mod_file|delete_local_mod_file)/);
  assert.match(transaction, /struct ManagedContentInventory/);
  assert.match(transaction, /MAX_PORTABLE_INVENTORY_ENTRIES: usize = 100_000/);
  assert.match(transaction, /pub\(crate\) enum ManagedContentParent/);
  assert.match(transaction, /pub\(crate\) fn managed_content_parent/);
  assert.match(transaction, /parent\.as_str\(\) != candidate\.canonical\(\)/);
  assert.match(transaction, /fn require_exact_managed_file_variant_or_absent/);
  assert.match(transaction, /managed_content_name_key\(&name\)/);
  const installProduction = install.slice(0, install.indexOf("#[cfg(test)]"));
  const transactionProduction = transaction.slice(
    0,
    transaction.indexOf("#[cfg(test)]"),
  );
  assert.doesNotMatch(installProduction, /fs::read_dir/);
  assert.equal((transactionProduction.match(/fs::read_dir/g) ?? []).length, 1);
  assert.doesNotMatch(pack, /Component::CurDir => \{\}/);
  assert.match(pack, /managed_content_name_key\(&name\)/);
  assert.match(
    pack,
    /managed_content_parent\(portable_parent\(&path\)\.as_ref\(\)\)/,
  );
  assert.match(
    pack,
    /managed_parent\.is_some\(\)[\s\S]*?ManagedContentFileName::new_exact\(portable\.file_name\(\)\.as_str\(\)\)\.is_err\(\)/,
  );
  assert.doesNotMatch(pack, /fn managed_pack_parent/);
  assert.match(pack, /struct PackDestinationKey/);
  assert.match(pack, /additional_guarded_paths/);
  assert.match(pack, /pub struct ManagedPackAvailability/);
  assert.match(
    pack,
    /ManagedContentInventory::capture\(game_dir, &guarded_paths\)/,
  );
  assert.match(
    pack,
    /require_exact_managed_file_variant_or_absent\(&enabled, &disabled\)/,
  );
  assert.match(
    managedTransaction,
    /plan_managed_pack_transaction[\s\S]*?ManagedContentPathResult::Download[\s\S]*?ManagedContentMutationPlan::new_deferred/,
  );
  assert.match(
    applicationPack,
    /manifest\.materialize[\s\S]*?complete\.bind_manifest\(body\)/,
  );
  assert.doesNotMatch(pack, /pub fn publish_manifest|publication_verified|FileTransaction/);
  for (const transactionTest of [
    "managed_pack_inspection_binds_index_and_override_sources_without_writes",
    "managed_pack_inspection_rejects_override_index_collisions",
  ]) {
    assert.match(pack, new RegExp(`fn ${transactionTest}`));
  }
  assert.match(applicationPack, /Vec<PendingManifestEntry>/);
  assert.match(
    applicationPack,
    /ManagedPackAvailability::capture\(game_dir, &index\.files\)/,
  );
  assert.match(applicationPack, /availability\.contains\(file\)/);
  assert.match(
    applicationPack,
    /validate_provider_pending_projection\(&entries\)/,
  );
  assert.match(applicationPack, /try_upsert_batch\(materialized\)/);
  assert.match(applicationPack, /record_pack_root\.then_some\(pack_id\)/);
  assert.match(applicationPack, /reserved_pack_id == Some\(&canonical_id\)/);
  assert.match(
    applicationPack,
    /if !required_hashes\.contains\(hash\) \{\s*continue;/,
  );
  assert.match(
    applicationPack,
    /fn sha1_only_managed_path_remains_unmanaged_during_manifest_materialization/,
  );
  assert.match(
    applicationPack,
    /fn duplicate_unknown_sha512_files_remain_unmanaged_during_materialization/,
  );
  assert.match(applicationPack, /let mut stale_ids = HashSet::new\(\)/);
  assert.match(
    applicationPack,
    /let manifest_indexes = manifest[\s\S]*?collect::<HashMap<_, _>>\(\)/,
  );
  const packPreparation = between(
    applicationPack,
    "async fn prepare_pack_manifest",
    "fn group_pack_files_by_sha512",
  );
  assert.doesNotMatch(
    packPreparation,
    /manifest\.find\(|stale_entries\.contains\(/,
  );
  assert.doesNotMatch(
    applicationPack,
    /save_with_revalidation|verify_managed_inventory/,
  );
  assert.match(
    applicationPack,
    /ContentManifest::decode_managed\(planning\.manifest_bytes\(\)\)[\s\S]*?manifest\.materialize[\s\S]*?complete\.bind_manifest\(body\)/,
  );
  assert.doesNotMatch(applicationPack, /\.drain\(\.\.\)|u64::MAX/);
  assert.doesNotMatch(
    resources,
    /fn is_safe_resource_name[\s\S]*?name\.starts_with\('\.'\)/,
  );
  assert.match(installFlight, /version_id: &str/);
  assert.match(
    installFlight,
    /let version_id = PortableFileName::new_exact\(version_id\)[\s\S]*root\.install_flight\(version_id\.key\(\), MAX_LIVE_LOADER_INSTALL_FLIGHTS\)/,
  );
  assert.doesNotMatch(installFlight, /version_id: version_id\.to_string\(\)/);

  assert.doesNotMatch(
    screenshotActions,
    /value\.trim\(\)|name\.startsWith|\[\\\\\/\]/,
  );
  assert.match(screenshotActions, /screenshotKind\(value\)/);
  assert.doesNotMatch(
    worldActions,
    /next\?\.trim\(\)|value\.trim\(\)|name\.startsWith/,
  );
  assert.match(worldActions, /const nextName = next \?\? '';/);

  for (const performance of [
    performancePlan,
    performanceState,
    performanceMutation,
  ]) {
    assert.match(performance, /PortableFileName/);
    assert.match(performance, /PortablePathKey/);
    assert.doesNotMatch(performance, /filename\.to_ascii_lowercase\(\)/);
  }
  assert.match(performancePlan, /PortableFileName::new_exact\(filename\)/);
  assert.doesNotMatch(
    performancePlan,
    /MAX_FILENAME_BYTES|filename\.is_ascii\(\)/,
  );
  assert.match(performanceState, /PortableFileName::new_exact\(filename\)/);
  assert.doesNotMatch(performanceState, /STATE_FILENAME_MAX_BYTES/);
  assert.doesNotMatch(
    performanceMutation,
    /filename\.eq_ignore_ascii_case|cfg!\(windows\)/,
  );
  assert.doesNotMatch(
    performanceMutation,
    /map\(\|installed\| \(installed\.filename\.as_str\(\)/,
  );

  const managedTree = between(
    managedFs,
    "impl ManagedTreeDirectory",
    "pub(crate) struct ManagedTreeLimits",
  );
  assert.match(
    managedFs,
    /MAX_MANAGED_TREE_OPERATION_ENTRIES: usize = 100_000/,
  );
  assert.match(managedFs, /MAX_MANAGED_TREE_OPERATION_DEPTH: usize = 64/);
  assert.match(
    managedTree,
    /pub fn copy_tree_no_replace[\s\S]*?limits: ManagedTreeCopyLimits/,
  );
  assert.match(
    managedTree,
    /let mut budget = ManagedTreeBudget \{[\s\S]*?remaining_entries: limits\.max_entries,[\s\S]*?remaining_bytes: limits\.max_bytes,[\s\S]*?max_depth: limits\.max_depth/,
  );
  const boundedCopy = between(
    managedTree,
    "fn copy_tree_contents",
    "fn cleanup_tree_failure",
  );
  assert.match(
    boundedCopy,
    /struct Frame \{[\s\S]*?source: ManagedDir,[\s\S]*?target: ManagedDir,[\s\S]*?source_revision: DirectoryRevision,[\s\S]*?entries: Vec<DirectoryEntry>,[\s\S]*?next_entry: usize/,
  );
  assert.match(
    boundedCopy,
    /let frame_capacity = budget[\s\S]*?\.max_depth[\s\S]*?\.checked_add\(1\)[\s\S]*?let mut frames = Vec::with_capacity\(frame_capacity\)[\s\S]*?frames\.push\(Frame::enter\(source\.clone\(\), target\.clone\(\)\)\?\)/,
  );
  assert.match(
    boundedCopy,
    /PortableFileName::new_exact\(name\)[\s\S]*?budget\.reserve_bytes\(guard\.size\(\)\)[\s\S]*?read_guarded_file_bounded\(name, &guard, guard\.size\(\)\)[\s\S]*?frame\.target\.write_new_exact\(name, &bytes\)/,
  );
  assert.match(
    boundedCopy,
    /let frame = frames\.pop\(\)[\s\S]*?frame[\s\S]*?\.source[\s\S]*?\.validate_revision\(&frame\.source_revision\)[\s\S]*?if !frames\.is_empty\(\) \{[\s\S]*?frame\.target\.sync\(\)\?/,
  );
  const directoryCopy = between(
    boundedCopy,
    "EntryKind::Directory => {",
    "EntryKind::Link | EntryKind::Other",
  );
  const depthCheck = directoryCopy.indexOf("child_depth >= frame_capacity");
  const budgetDepthCheck = directoryCopy.indexOf("budget.enter(child_depth)?");
  const childSourceOpen = directoryCopy.indexOf(
    "frame.source.open_observed_child(&entry)?",
  );
  const childTargetCreate = directoryCopy.indexOf(
    "frame.target.create_child_new(name)?",
  );
  const childFramePush = directoryCopy.indexOf(
    "frames.push(Frame::enter(child_source, child_target)?)",
  );
  assert.ok(
    depthCheck >= 0 &&
      depthCheck < budgetDepthCheck &&
      budgetDepthCheck < childSourceOpen &&
      childSourceOpen < childTargetCreate &&
      childTargetCreate < childFramePush,
    "the bounded frame depth must be admitted before opening or creating a child",
  );
  assert.equal(
    occurrences(boundedCopy, "copy_tree_contents(").length,
    1,
    "tree copying must not recursively invoke copy_tree_contents",
  );
  assert.match(
    managedFs,
    /struct ManagedClearContentsFrame \{[\s\S]*?directory: ManagedDir,[\s\S]*?entries: Vec<DirectoryEntry>,[\s\S]*?next_entry: usize/,
  );
  const clearContents = between(
    managedFs,
    "fn clear_contents(&self)",
    "pub(crate) fn verify_authenticated",
  );
  for (const [childOpen, listing, revalidate, removeChild] of [
    [
      "frame.directory.open_observed_child(&entry)?",
      "child.listing(MAX_MANAGED_TREE_OPERATION_ENTRIES)?",
      "frame.directory.revalidate()?",
      "parent.directory.remove_empty_child(&frame.directory)?",
    ],
  ]) {
    assert.match(
      clearContents,
      /let frame_capacity = MAX_MANAGED_TREE_OPERATION_DEPTH \+ 1[\s\S]*?let mut frames = Vec::with_capacity\(frame_capacity\)[\s\S]*?frames\.push\(ManagedClearContentsFrame::new\(/,
    );
    const depthCheck = clearContents.indexOf("child_depth >= frame_capacity");
    const childOpenIndex = clearContents.indexOf(childOpen);
    assert.ok(
      depthCheck >= 0 && depthCheck < childOpenIndex,
      "cleanup depth must be refused before opening the child frame",
    );
    assert.ok(clearContents.includes(listing));
    assert.ok(clearContents.includes(revalidate));
    assert.ok(clearContents.includes(removeChild));
  }
  assert.equal(
    occurrences(clearContents, "clear_contents(").length,
    1,
    "unlocked cleanup must not recursively invoke clear_contents",
  );
  const retainedTreeCleanup = between(
    managedFs,
    "fn retain_tree_cleanup(",
    "fn retain_stage_discard_locked",
  );
  assert.match(
    retainedTreeCleanup,
    /retain_child_tree_removal_locked\(transition, &stage_directory\)/,
  );
  const retainedTreeRemoval = between(
    managedFs,
    "fn retain_child_tree_removal_locked(",
    "pub(crate) fn clear_owned_contents",
  );
  assert.match(
    retainedTreeRemoval,
    /DirectoryParkOutcome::Parked\(parked\)[\s\S]*?parked\.remove_tree\(\)[\s\S]*?EffectOwner::retain_parked_directory_tree_removal[\s\S]*?EffectOwner::retain_directory_tree_removal/,
  );
  assert.match(
    retainedTreeRemoval,
    /DirectoryParkOutcome::AppliedUnverified\(obligation\)[\s\S]*?EffectOwner::retain_directory_park_removal/,
  );
  assert.doesNotMatch(managedTree, /\.join\(|F_GETPATH/);
  const promotion = between(
    managedTree,
    "pub fn copy_tree_no_replace",
    "fn world_source_revision_drift",
  );
  assert.ok(
    promotion.indexOf(".validate_revision(&source_revision)") <
      promotion.indexOf("stage.sync()") &&
      promotion.indexOf("stage.sync()") <
        promotion.indexOf(".move_no_replace("),
    "the source revision and staged tree must be settled before no-replace publication",
  );
  const indeterminateBranches = occurrences(
    promotion,
    "ManagedTreeCopyOutcome::Indeterminate",
  );
  assert.ok(indeterminateBranches.length >= 2);
  for (const position of indeterminateBranches) {
    assert.doesNotMatch(
      promotion.slice(position, position + 180),
      /cleanup_tree_failure/,
    );
  }
  assert.doesNotMatch(managedFs, /F_GETPATH/);
  assert.match(
    managedFs,
    /enum ManagedEffectContinuation \{[\s\S]*?TreeDirectoryMove \{[\s\S]*?receipt: DirectoryMoveReceipt,[\s\S]*?TreeCleanup \{/,
  );
  assert.match(fsPlatform, /offset_of!\(FILE_ID_BOTH_DIR_INFO, FileName\)/);
  assert.match(
    fsPlatform,
    /!offset\.is_multiple_of\(std::mem::align_of::<FILE_ID_BOTH_DIR_INFO>\(\)\)[\s\S]*?offset[\s\S]*?\.checked_add\(record_size\)/,
  );
  assert.match(
    fsPlatform,
    /size_of::<FILE_RENAME_INFORMATION>\(\)[\s\S]*?checked_add\(filename_bytes as usize\)/,
  );
  assert.match(fsPlatform, /\(\*information\)\.ReplaceIfExists = 0/);
  assert.match(
    fsPlatform,
    /RootDirectory = if same_parent \{[\s\S]*?null_mut\(\)[\s\S]*?destination_parent\.as_raw_handle\(\)\.cast\(\)/,
  );
  assert.match(
    fsPlatform,
    /NtSetInformationFile\([\s\S]*?source\.as_raw_handle\(\)\.cast\(\),[\s\S]*?FileRenameInformation/,
  );
  assert.match(fsPlatform, /RtlNtStatusToDosError\(renamed\)/);
  assert.match(
    fsPlatform,
    /fn open_directory_cleanup_deleter[\s\S]*?FILE_TRAVERSE_ACCESS[\s\S]*?DELETE_ACCESS[\s\S]*?FILE_SHARE_READ/,
  );
  assert.match(
    fsPlatform,
    /fn open_file_cleanup_deleter[\s\S]*?FILE_READ_DATA_ACCESS[\s\S]*?DELETE_ACCESS[\s\S]*?FILE_SHARE_READ/,
  );

  assert.match(resources, /struct WorldBackupNamePlan/);
  assert.match(resources, /fn bounded_world_backup_name/);
  assert.match(
    resources,
    /Sha256::digest\(world_name\.as_str\(\)\.as_bytes\(\)\)/,
  );
  assert.match(
    resources,
    /ManagedTreeCopyLimits \{[\s\S]*?max_depth: WORLD_BACKUP_MAX_DEPTH,[\s\S]*?max_entries: WORLD_BACKUP_MAX_ENTRIES,[\s\S]*?max_bytes: WORLD_BACKUP_MAX_BYTES/,
  );
  const worldBackup = between(
    resources,
    "pub(crate) async fn handle_backup_instance_world",
    "pub(crate) async fn handle_instance_mods",
  );
  const sourceOpen = worldBackup.indexOf(".open_child(world_name.as_str())");
  const backupOpen = worldBackup.indexOf('.open_or_create_child("backups")');
  const copyStart = worldBackup.indexOf("copy_world_backup_staged(");
  const planStart = worldBackup.indexOf("WorldBackupNamePlan::new(");
  const filesystemAdmission = worldBackup.indexOf(
    "admit_exclusive_blocking_filesystem()",
  );
  const authorityAdmission = worldBackup.indexOf(
    "admit_instance_content_authority(lifecycle_guard)",
  );
  const workerStart = worldBackup.indexOf(".run(move ||");
  const rootActivation = worldBackup.indexOf(".activate()", workerStart);
  assert.ok(
    planStart >= 0 &&
      planStart < filesystemAdmission &&
      filesystemAdmission < authorityAdmission &&
      authorityAdmission < workerStart &&
      workerStart < rootActivation &&
      rootActivation < sourceOpen &&
      sourceOpen < backupOpen &&
      backupOpen < copyStart,
    "the complete backup plan must be admitted before any source or target capability opens",
  );
  assert.doesNotMatch(
    worldBackup,
    /ManagedTreeDirectory::(?:open|from_directory)/,
  );
  assert.doesNotMatch(
    resources,
    /available_world_backup_name|available_temp_world_backup_name|copy_world_dir_bounded|copy_regular_file_exact/,
  );
  assert.doesNotMatch(resources, /fs::rename\(&temp|remove_dir_all\(&temp/);

  assert.match(
    architecture,
    /strict v3 SHA-512-plus-size provenance manifests/,
  );
  assert.match(contentAdr, /strict v3 `axial\.content\.json` manifest/);
  assert.doesNotMatch(contentAdr, /strict v2 `axial\.content\.json` manifest/);
});
