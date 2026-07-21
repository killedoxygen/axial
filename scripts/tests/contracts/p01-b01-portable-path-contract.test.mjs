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
  assert.match(
    install,
    /struct InstallDestination \{[^}]*relative: PortableRelativePath,[^}]*variants: Vec<PortableRelativePath>/,
  );
  assert.doesNotMatch(
    install,
    /struct InstallDestination \{[^}]*relative: String/,
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
  assert.match(install, /fn manifest_mod_candidates/);
  assert.match(install, /guard_managed_file_variants\(&variant_pairs\)/);
  assert.doesNotMatch(
    between(
      install,
      "pub(crate) fn stage_managed_removals",
      "pub fn uninstall",
    ),
    /filter\(\|\(_, _, present\)\| \*present\)/,
  );
  assert.match(install, /try_upsert_batch\(entries\)/);
  assert.match(
    install,
    /save_with_revalidation\(game_dir, \|\| transaction\.verify_managed_inventory\(\)\)/,
  );
  assert.match(transaction, /struct ManagedContentInventory/);
  assert.match(transaction, /MAX_PORTABLE_INVENTORY_ENTRIES: usize = 100_000/);
  assert.match(transaction, /pub\(crate\) enum ManagedContentParent/);
  assert.match(transaction, /pub\(crate\) fn managed_content_parent/);
  assert.match(transaction, /parent\.as_str\(\) != candidate\.canonical\(\)/);
  assert.match(transaction, /fn record_file/);
  assert.match(transaction, /fn record_absent/);
  assert.match(transaction, /fn require_exact_managed_file_variant_or_absent/);
  assert.match(transaction, /pub\(crate\) fn guard_managed_file_variants/);
  const additionalGuard = between(
    transaction,
    "pub(crate) fn guard_additional_paths",
    "pub(crate) fn guard_managed_file_variants",
  );
  assert.match(additionalGuard, /collect::<HashSet<_>>\(\)/);
  assert.doesNotMatch(additionalGuard, /expanded_paths\.contains/);
  assert.match(transaction, /managed_content_name_key\(&name\)/);
  assert.match(transaction, /\.expand\(&self\.root, &expanded_paths\)/);
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
    pack,
    /ContentResult<ContentManifest>[\s\S]*?save_with_revalidation\(game_dir,[\s\S]*?commit_after_verified_publication\(\)/,
  );
  assert.doesNotMatch(pack, /pub fn publish_manifest|publication_verified/);
  for (const transactionTest of [
    "finalize_failure_rolls_back_new_pack_files_without_network",
    "manifest_origin_conflict_rolls_back_new_pack_files_without_network",
    "successful_pack_transaction_publishes_files_and_strict_v3_manifest_without_network",
  ]) {
    assert.match(pack, new RegExp(`async fn ${transactionTest}`));
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
    /Ok\(std::mem::take\(&mut prepared_manifest\.manifest\)\)/,
  );
  assert.doesNotMatch(applicationPack, /\.drain\(\.\.\)|u64::MAX/);
  assert.doesNotMatch(
    resources,
    /fn is_safe_resource_name[\s\S]*?name\.starts_with\('\.'\)/,
  );
  assert.match(installFlight, /version_id: PortablePathKey/);
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
    "impl AnchoredDirectory",
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
  assert.match(managedTree, /ManagedTreeBudget::new\(limits\)/);
  assert.match(
    managedTree,
    /fn validate_copy_pair[\s\S]*?budget\.enter\(depth\)/,
  );
  const pairValidation = between(
    managedTree,
    "fn validate_copy_pair",
    "fn validate_owned_tree_metadata",
  );
  assert.match(pairValidation, /hash_tree_file\(&mut source_file, \*size\)/);
  assert.match(pairValidation, /hash_tree_file\(&mut target_file, \*size\)/);
  assert.match(
    managedTree,
    /fn cleanup_owned_tree[\s\S]*?budget\.enter\(depth\)/,
  );
  assert.match(
    managedTree,
    /fn promote_owned_tree[\s\S]*?validate_copy_pair\([\s\S]*?tree_rename_directory_no_replace/,
  );
  assert.doesNotMatch(managedTree, /\.join\(|F_GETPATH/);
  const promotion = between(
    managedTree,
    "fn promote_owned_tree",
    "fn publication_topology",
  );
  assert.ok(
    promotion.indexOf("validate_copy_pair(") <
      promotion.indexOf("tree_rename_directory_no_replace("),
    "the exact source/target pair must be validated before the no-replace promotion",
  );
  const indeterminateBranches = occurrences(
    promotion,
    "ManagedTreeCopyOutcome::Indeterminate",
  );
  assert.ok(indeterminateBranches.length >= 2);
  for (const position of indeterminateBranches) {
    assert.doesNotMatch(
      promotion.slice(position, position + 180),
      /cleanup_owned_stage/,
    );
  }
  assert.doesNotMatch(managedFs, /F_GETPATH/);
  assert.match(
    managedFs,
    /enum OwnedTreeReceiptEntry \{[\s\S]*?source_identity: platform::FileIdentity,[\s\S]*?source_stamp: platform::TreeFileStamp,[\s\S]*?identity: platform::FileIdentity,[\s\S]*?size: u64,[\s\S]*?stamp: platform::TreeFileStamp,[\s\S]*?sha256: Option<\[u8; 32\]>/,
  );
  assert.match(managedFs, /offset_of!\(FILE_ID_BOTH_DIR_INFO, FileName\)/);
  assert.match(
    managedFs,
    /record_extent[\s\S]*?next % size_of::<u64>\(\) != 0/,
  );
  assert.match(
    managedFs,
    /size_of::<FILE_RENAME_INFO>\(\)[\s\S]*?checked_add\(filename_bytes as usize\)/,
  );
  assert.match(managedFs, /Anonymous\.ReplaceIfExists = false/);
  assert.match(managedFs, /RootDirectory = to_parent\.as_raw_handle\(\)/);
  assert.match(
    managedFs,
    /SetFileInformationByHandle\([\s\S]*?source\.as_raw_handle\(\),[\s\S]*?FileRenameInfo/,
  );
  assert.match(
    managedFs,
    /fn tree_open_child_directory_for_cleanup[\s\S]*?FILE_TRAVERSE_ACCESS[\s\S]*?DELETE_ACCESS[\s\S]*?FILE_SHARE_READ/,
  );
  assert.match(
    managedFs,
    /fn tree_remove_file\([\s\S]*?expected_size: u64,[\s\S]*?expected_stamp: TreeFileStamp,[\s\S]*?expected_digest: Option<\[u8; 32\]>[\s\S]*?FILE_READ_DATA_ACCESS \| FILE_READ_ATTRIBUTES \| DELETE_ACCESS/,
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
  const rootOpen = worldBackup.indexOf("ManagedTreeDirectory::open(&game_dir)");
  assert.ok(
    planStart >= 0 &&
      planStart < filesystemAdmission &&
      filesystemAdmission < rootOpen &&
      rootOpen < sourceOpen &&
      sourceOpen < backupOpen &&
      backupOpen < copyStart,
    "the complete backup plan must be admitted before any source or target capability opens",
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
