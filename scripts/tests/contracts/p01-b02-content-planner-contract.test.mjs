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

test("resolver consumes explicit full-entry liveness without ambient paths", async () => {
  const [resolver, planner] = await Promise.all([
    read("core/content/src/resolver.rs"),
    read("core/content/src/managed_transaction.rs"),
  ]);
  const target = braceBlock(resolver, "pub struct ResolutionTarget");
  assert.doesNotMatch(target, /game_dir|PathBuf|\bPath\b/);
  const resolve = braceBlock(resolver, "pub async fn resolve_content");
  assert.match(resolve, /live_content:\s*&LiveManagedContent/);
  assert.doesNotMatch(resolver, /entry_file_present|installed_entry_present/);
  assert.match(planner, /entries:\s*HashMap<CanonicalId, ManifestEntry>/);
  const contains = braceBlock(planner, "pub fn contains");
  assert.match(contains, /self\.entries\.get\(entry\.canonical_id\(\)\) == Some\(entry\)/);
  assert.match(planner, /pub fn from_entries<'a>/);
});

test("provider artifacts are admitted once into typed authenticated transfers", async () => {
  const [install, planner] = await Promise.all([
    read("core/content/src/install.rs"),
    read("core/content/src/managed_transaction.rs"),
  ]);
  const artifact = braceBlock(install, "struct PlannedArtifact");
  assert.match(artifact, /download_url:\s*Url/);
  assert.doesNotMatch(artifact, /download_url:\s*String/);
  const admit = braceBlock(install, "fn admit");
  assert.match(admit, /let \(size, download_url\) = validate_planned_artifact/);
  assert.doesNotMatch(admit, /Url::parse|expect\(/);
  const validate = braceBlock(install, "pub(crate) fn validate_planned_artifact");
  assert.match(validate, /ContentResult<\(u64, Url\)>/);
  assert.equal((validate.match(/Url::parse/g) ?? []).length, 1);
  const project = braceBlock(planner, "fn project_install");
  assert.match(project, /ExpectedTransferDigests::from_hex/);
  assert.match(project, /TransferContract::authenticated_exact/);
  assert.match(project, /planned\.download_url\(\)\.clone\(\)/);
  assert.doesNotMatch(planner, /derive\(Debug\)[\s\S]{0,80}ManagedContentPayloadSource/);
  assert.doesNotMatch(install, /derive\([^)]*Debug[^)]*\)[\s\S]{0,80}(?:PlannedFile|PlannedArtifact)/);
});

test("manifest projections retain an unforgeable Core planning binding", async () => {
  const [core, managedFs, minecraft, planner] = await Promise.all([
    read("core/minecraft/src/managed_fs/content_transaction.rs"),
    read("core/minecraft/src/managed_fs.rs"),
    read("core/minecraft/src/lib.rs"),
    read("core/content/src/managed_transaction.rs"),
  ]);
  const binding = braceBlock(core, "pub struct ManagedContentPlanningBinding");
  assert.match(binding, /session:\s*Arc<\(\)>/);
  assert.doesNotMatch(
    core,
    /(?:derive\([^)]*Clone[^)]*\)[\s\S]{0,80}ManagedContentPlanningBinding|impl\s+Clone\s+for\s+ManagedContentPlanningBinding)/,
  );
  assert.match(core, /pub fn planning_binding\(&self\)/);
  assert.match(core, /pub fn matches_planning_binding\(&self,/);
  assert.match(managedFs, /ManagedContentPlanningBinding/);
  assert.match(minecraft, /ManagedContentPlanningBinding/);
  assert.match(planner, /binding:\s*ManagedContentPlanningBinding/);
  assert.match(planner, /require_planning_binding\(session, &manifest\.binding\)/);
  assert.match(planner, /session\.matches_planning_binding\(&projection\.binding\)/);
  assert.match(planner, /require_manifest_snapshot/);
  assert.match(core, /fn planning_binding_matches_only_its_exact_planning_flow\s*\(/);
});

test("managed logical-name aliases are bounded and revalidated once per checkpoint", async () => {
  const core = await read("core/minecraft/src/managed_fs/content_transaction.rs");
  const validate = braceBlock(core, "fn validate_managed_logical_name_bindings");
  assert.match(validate, /entries_bounded\(MAX_MANAGED_DIRECTORY_ENTRIES\)/);
  assert.match(validate, /managed_content_name_key\(&name\)/);
  assert.match(validate, /name != spec\.enabled && name != spec\.disabled/);
  assert.doesNotMatch(validate, /for\s+\w+\s+in\s+0\.\./);
  const checkpoint = braceBlock(core, "fn revalidate_transaction_logical_names");
  assert.match(checkpoint, /mutations[\s\S]*read_preconditions/);
  const readPreconditions = braceBlock(core, "fn revalidate_read_preconditions");
  const effects = braceBlock(core, "fn revalidate_final_effects");
  assert.doesNotMatch(
    readPreconditions,
    /validate_managed_logical_name_bindings|entries_bounded/,
  );
  assert.doesNotMatch(effects, /validate_managed_logical_name_bindings|entries_bounded/);
  for (const name of [
    "managed_logical_name_observation_rejects_arbitrary_disabled_aliases",
    "exact_enabled_and_disabled_variants_are_allowed_but_late_aliases_are_not",
  ]) {
    assert.match(core, new RegExp(`fn\\s+${name}\\s*\\(`));
  }
});

test("uninstall dependency candidates use one shared indexed pass", async () => {
  const planner = await read("core/content/src/managed_transaction.rs");
  const scope = braceBlock(planner, "fn managed_uninstall_scope");
  assert.match(scope, /SelectedDependencyIndex/);
  assert.match(scope, /\.any\(\|dependency\| index\.matches\(dependency\)\)/);
  assert.equal(
    (planner.match(/managed_uninstall_scope(?:<'a>)?\(/g) ?? []).length,
    3,
  );
  assert.doesNotMatch(planner, /selected\.iter\(\)\.any\(\|selected\|/);
  assert.doesNotMatch(planner, /entries\.iter\(\)\.any\(\|removed\|/);
  assert.match(
    planner,
    /fn selected_dependency_index_preserves_project_and_version_only_semantics\s*\(/,
  );
});

test("content planning separates observations from exact filesystem effects", async () => {
  const planner = await read("core/content/src/managed_transaction.rs");
  assert.match(planner, /pub fn managed_content_liveness_paths/);
  assert.match(planner, /pub fn managed_install_observation_paths/);
  assert.match(planner, /pub fn managed_uninstall_observation_paths/);
  const projection = braceBlock(
    planner,
    "impl ManagedContentOperationProjection",
  );
  assert.match(projection, /pub fn effect_paths/);
  assert.match(projection, /pub fn seal/);
  assert.match(planner, /require_manifest_snapshot\(session\.manifest_bytes\(\)/);
  assert.match(planner, /observed_matches_entry\(observation\.state\(\), previous\)/);
  assert.match(planner, /matching_variants > 1/);
  assert.match(planner, /observation\.path\(\) != path/);
  assert.doesNotMatch(planner, /ManagedContentPathResult::Preserve/);
  assert.match(planner, /ManagedContentPathResult::Download/);
  assert.match(planner, /ManagedContentPathResult::Absent/);
});

test("ordinary legacy path install and uninstall entry points are deleted", async () => {
  const [library, install] = await Promise.all([
    read("core/content/src/lib.rs"),
    read("core/content/src/install.rs"),
  ]);
  assert.doesNotMatch(library, /\binstall_and_record\b|\buninstall_many\b/);
  assert.doesNotMatch(
    install,
    /pub async fn install_and_record|pub fn uninstall(?:_many)?\s*\(|download_verified_content_to_staging/,
  );
  assert.match(library, /plan_managed_content_install/);
  assert.match(library, /plan_managed_content_uninstall/);
  assert.match(library, /toggle_mod_file/);
  assert.match(library, /install_pack_files_with_finalize/);
});

test("legacy replacement transaction policy is deleted", async () => {
  const transaction = await read("core/content/src/transaction.rs");
  assert.doesNotMatch(
    transaction,
    /apply_preserving_absence|apply_with_policy|replace_existing|must_be_absent|allow_existing_destination/,
  );
  assert.match(transaction, /pub\(crate\) fn apply_new_with_inventory/);
  assert.match(transaction, /pub\(crate\) fn empty/);
  assert.match(transaction, /pub\(crate\) fn stage_removals_with_revalidation/);
});
