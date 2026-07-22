import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repository = fileURLToPath(new URL("../../../", import.meta.url));
const read = (path) => readFile(join(repository, path), "utf8");

const functionBlock = (source, name) => {
  const marker = new RegExp(
    `(?:pub(?:\\([^)]*\\))?\\s+)?(?:async\\s+)?fn\\s+${name}[^\\{]*\\{`,
  );
  const match = marker.exec(source);
  assert.ok(match, `missing function ${name}`);
  const opening = source.indexOf("{", match.index);
  let depth = 0;
  for (let offset = opening; offset < source.length; offset += 1) {
    if (source[offset] === "{") depth += 1;
    if (source[offset] === "}") depth -= 1;
    if (depth === 0) return source.slice(match.index, offset + 1);
  }
  assert.fail(`unterminated function ${name}`);
};

const ordered = (source, markers) => {
  let previous = -1;
  for (const marker of markers) {
    const index = source.indexOf(marker, previous + 1);
    assert.notEqual(index, -1, `missing ordered marker: ${marker}`);
    assert.ok(index > previous, `marker is out of order: ${marker}`);
    previous = index;
  }
};

test("State declares one managed-library owner with retained provenance", async () => {
  const [state, library] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/managed_library.rs"),
  ]);
  assert.match(state, /mod managed_library;/);
  assert.match(library, /struct ManagedLibraryOwnerInner\s*\{/);
  assert.match(library, /root_session:\s*Arc<AppRootSession>/);
  assert.match(library, /paths:\s*AppPaths/);
  assert.match(library, /rotation:\s*Arc<AsyncMutex<\(\)>>/);
  assert.match(library, /current:\s*Option<CurrentLibraryGeneration>/);
  assert.match(library, /retiring:\s*Option<RetiringLibraryGeneration>/);
  const startup = functionBlock(state, "new_with_telemetry_inner");
  ordered(startup, [
    "ManagedLibraryStartup::prepare",
    "ManagedArtifactMutationEpochCoordinator::default",
  ]);
  assert.match(library, /managed_startup_prepares_the_capability_relative_layout/);
});

test("candidate preparation owns provenance and stays off Tokio workers", async () => {
  const library = await read("apps/api/src/state/managed_library.rs");
  const prepare = functionBlock(library, "prepare_change");
  const signature = prepare.slice(0, prepare.indexOf("{"));
  assert.doesNotMatch(signature, /AdmittedAbsoluteDirectory/);
  assert.match(prepare, /spawn_blocking/);
  assert.match(prepare, /lock_owned\(\)\.await/);

  const worker = functionBlock(library, "prepare_configured_change");
  assert.match(worker, /prepare_managed_library_directory/);
  assert.match(worker, /admit_existing_library_directory/);
  assert.match(worker, /ExistingLibraryDirectoryAdmission::InsideRoot/);
  assert.match(worker, /prepare_admission_rebind/);
});

test("operation admission does not validate filesystem state under State lock", async () => {
  const library = await read("apps/api/src/state/managed_library.rs");
  const acquire = functionBlock(library, "try_acquire");
  ordered(acquire, [
    "current.root.witness()",
    "witness.try_acquire()?",
    "let state = self.lock_state()",
  ]);
  assert.doesNotMatch(
    acquire.slice(0, acquire.indexOf("witness.try_acquire()?")),
    /root\.try_acquire/,
  );
});

test("installed-version scans are generation-pinned without pinning warm cache entries", async () => {
  const [state, installed] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/installed_versions.rs"),
  ]);
  const refreshKey = installed.slice(
    installed.indexOf("struct RefreshKey"),
    installed.indexOf("struct CachedSnapshot"),
  );
  assert.match(refreshKey, /library_generation:\s*LibraryGenerationId/);
  assert.match(refreshKey, /index_generation:\s*u64/);
  assert.doesNotMatch(refreshKey, /Path|library_dir/);

  const lookup = installed.slice(
    installed.indexOf("pub(crate) struct InstalledVersionsLookup"),
    installed.indexOf("struct RefreshKey"),
  );
  assert.match(lookup, /operation:\s*LibraryOperation/);
  assert.match(lookup, /self\.operation\.configured_path\(\)/);

  const cached = installed.slice(
    installed.indexOf("struct CachedSnapshot"),
    installed.indexOf("enum RefreshCompletion"),
  );
  assert.doesNotMatch(cached, /LibraryOperation|PathBuf/);
  const scan = functionBlock(installed, "scan_with_validation");
  assert.match(scan, /scan_versions_snapshot\(operation\.core\(\)\)/);
  assert.match(installed, /RefreshClaim::Handoff/);
  assert.match(
    installed,
    /different_generation_waits_for_handoff_before_starting_one_scan/,
  );
  const lookupFlow = functionBlock(installed, "lookup");
  assert.match(
    lookupFlow,
    /RefreshCompletion::Ready[\s\S]*refresh_key_is_current\(&key,\s*&operation\)/,
  );
  const keyCurrency = functionBlock(installed, "refresh_key_is_current");
  assert.match(
    keyCurrency,
    /key\.library_generation\s*==\s*operation\.generation\(\)/,
  );
  assert.match(
    keyCurrency,
    /key\.index_generation\s*==\s*state\.generation/,
  );
  assert.match(
    installed,
    /invalidation_after_finish_before_waiter_wake_rejects_ready_snapshot/,
  );

  const stateLookup = functionBlock(
    state,
    "installed_versions_snapshot_with_foreground",
  );
  ordered(stateLookup, [
    "try_acquire_managed_library()",
    ".lookup(operation",
    "validate_managed_library_operation(lookup.operation())",
  ]);
  assert.match(stateLookup, /MAX_LIBRARY_GENERATIONS_PER_VERSION_LOOKUP/);
});

test("instance readiness workers retain their indexed library generation", async () => {
  const instances = await read("apps/api/src/application/instances.rs");
  const indexed = instances.slice(
    instances.indexOf("struct IndexedCurrentVersions"),
    instances.indexOf("fn unconfigured_versions_scan"),
  );
  assert.match(indexed, /authority:\s*Option<InstalledVersionsLookup>/);
  assert.match(indexed, /authority:\s*Some\(lookup\)/);
  assert.doesNotMatch(indexed, /to_path_buf|Option<PathBuf>/);

  const listEnrichment = functionBlock(instances, "enrich_instances_for_state");
  ordered(listEnrichment, [
    "run_blocking_filesystem(move ||",
    "authority.as_ref().map(InstalledVersionsLookup::library_dir)",
  ]);
  const singleEnrichment = functionBlock(
    instances,
    "enrich_instance_for_indexed_scan",
  );
  ordered(singleEnrichment, [
    "run_blocking_filesystem(move ||",
    "authority.as_ref().map(InstalledVersionsLookup::library_dir)",
  ]);
  const foreground = functionBlock(
    instances,
    "enrich_instance_for_state_with_foreground",
  );
  assert.match(
    foreground,
    /enrich_instance_for_indexed_scan\(state, instance, scan, Some\(lookup\)\)/,
  );
});

test("same-binding publication blocks mixed State and Core generations", async () => {
  const library = await read("apps/api/src/state/managed_library.rs");
  const commit = functionBlock(library, "commit_with_publication_hook");
  ordered(commit, [
    "begin_publication",
    "publication_started()",
    "admission.commit()",
    "state.publishing_revision = None",
  ]);
  const acquire = functionBlock(library, "try_acquire");
  assert.match(acquire, /publishing_revision\.is_some\(\)/);
  assert.match(acquire, /io::ErrorKind::WouldBlock/);
});

test("retirement is retained before its cancellation point", async () => {
  const library = await read("apps/api/src/state/managed_library.rs");
  const close = functionBlock(library, "close");
  ordered(close, [
    "state.retiring = Some",
    "retirement.drain_and_settle().await?",
    "state.retiring = None",
  ]);
  assert.match(library, /cancelled_close_resumes_the_same_sole_retirement/);
});

test("status and debug surfaces contain no configured path", async () => {
  const library = await read("apps/api/src/state/managed_library.rs");
  const status = library.slice(
    library.indexOf("pub(crate) struct ManagedLibraryStatus"),
    library.indexOf("pub(crate) struct ManagedLibraryStartup"),
  );
  assert.doesNotMatch(status, /Path|directory|location/i);
  const fingerprintDebug = library.slice(
    library.indexOf("impl std::fmt::Debug for LibraryFingerprint"),
    library.indexOf("impl std::fmt::Debug for LibraryOperation"),
  );
  assert.doesNotMatch(fingerprintDebug, /configured_path/);
});

test("durable config commits runtime authority before visible config", async () => {
  const config = await read("apps/api/src/state/config.rs");
  const commit = functionBlock(config, "await_commit");
  ordered(commit, [
    "ConfigCommitAdmission::commit",
    "state.lock()",
    "state.visible = commit.candidate.clone()",
    "observer(previous, current.clone())",
    "drop(committed_admission)",
  ]);
});

test("setup layout creation is capability-relative and epoch-owned", async () => {
  const [setup, state, library] = await Promise.all([
    read("apps/api/src/application/setup.rs"),
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/managed_library.rs"),
  ]);
  const setupFlow = functionBlock(setup, "setup_init_owned");
  assert.doesNotMatch(setupFlow, /create_minecraft_dir|create_dir_all/);
  ordered(setupFlow, [
    "let setup_result",
    "invalidate_installed_versions()",
    "invalidate_create_view_root",
    "setup_result.map_err",
  ]);
  const admission = functionBlock(state, "config_managed_library_admission");
  ordered(admission, [
    "managed_artifact_epoch.admit()",
    "managed_library",
    ".prepare_change(selection)",
  ]);
  const worker = functionBlock(library, "prepare_configured_change");
  assert.match(worker, /operation\.prepare_layout\(\)\?/);
  assert.match(worker, /root\.try_acquire\(\)\?\.prepare_layout\(\)\?/);
  const setupCommit = functionBlock(state, "commit_managed_library_setup");
  ordered(setupCommit, [
    "installed_versions.invalidate()",
    "invalidate_create_view_root",
    "drop(mutation)",
  ]);
});

test("shutdown closes config before the managed library generation", async () => {
  const shutdown = await read("apps/api/src/state/shutdown.rs");
  const coordinate = functionBlock(shutdown, "coordinate");
  ordered(coordinate, ["self.close_config(state)", "self.close_managed_library(state).await"]);
  const closeLibrary = functionBlock(shutdown, "close_managed_library");
  assert.match(closeLibrary, /AppShutdownStep::Config/);
  assert.match(closeLibrary, /state\s*\.close_managed_library\(\)\s*\.await/);
});

test("install effects retain mutation and library authority through activation", async () => {
  const [install, loader, state, knownGood] = await Promise.all([
    read("apps/api/src/application/install.rs"),
    read("apps/api/src/application/install/loader.rs"),
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/known_good.rs"),
  ]);
  const availability = functionBlock(install, "require_available_install_library");
  assert.match(availability, /managed_library_status\(\)\.availability/);
  assert.doesNotMatch(availability, /try_acquire|library_dir/);
  for (const state of ["Unconfigured", "Degraded", "Changing", "Closed"]) {
    assert.match(availability, new RegExp(`ManagedLibraryAvailability::${state}`));
  }
  assert.match(
    availability,
    /ManagedLibraryAvailability::Degraded\(_\)[\s\S]*StatusCode::PRECONDITION_FAILED/,
  );

  const queueSpec = functionBlock(install, "install_queue_spec_from_request");
  assert.doesNotMatch(queueSpec, /state\.library_dir\(\)/);
  assert.equal(
    queueSpec.match(/require_available_install_library\(state\)\?/g)?.length,
    2,
  );

  const vanilla = functionBlock(install, "start_install_version_with_foreground");
  ordered(vanilla, [
    "require_available_install_library(state)?",
    "register_install_foreground(state)?",
    "admit_managed_artifact_mutation()",
    "try_acquire_managed_library()",
    "(mutation, library_operation)",
    "validate_managed_library_operation(library_operation)",
    "accept_known_good_install_receipt",
    "drop(authority)",
  ]);
  assert.doesNotMatch(
    vanilla,
    /validate_managed_library_operation\(library_operation\)[\s\S]{0,100}library_operation\.revalidate/,
  );

  const loaderStart = functionBlock(loader, "start_loader_install_with_foreground");
  ordered(loaderStart, [
    "require_available_install_library(state)?",
    "register_install_foreground(state)?",
    "admit_managed_artifact_mutation()",
    "try_acquire_managed_library()",
    "(mutation, library_operation)",
    "validate_managed_library_operation(library_operation)?",
    "accept_known_good_install_receipt",
    "drop(authority)",
  ]);
  assert.doesNotMatch(
    loaderStart,
    /validate_managed_library_operation\(library_operation\)\?;[\s\S]{0,100}library_operation\.revalidate/,
  );
  assert.match(vanilla, /DownloadError::FileOperation\(error\)/);
  assert.match(loaderStart, /LoaderError::Io\(error\)/);

  const acceptance = functionBlock(state, "accept_known_good_install_receipt");
  const acceptanceSignature = acceptance.slice(0, acceptance.indexOf("{"));
  assert.match(acceptanceSignature, /operation:\s*&LibraryOperation/);
  assert.doesNotMatch(acceptanceSignature, /Path/);
  ordered(acceptance, [
    "validate_managed_library_operation(operation)?",
    ".activate_known_good_source(",
    ".await",
  ]);
  assert.doesNotMatch(
    acceptance,
    /\.await;[\s\S]*validate_managed_library_operation/,
    "install acceptance cannot validate after dropping exact activation cleanup authority",
  );

  const activation = functionBlock(
    state,
    "activate_known_good_source_before_final_validation",
  );
  ordered(activation, [
    "complete_independent_known_good_fanout(",
    ".await;",
    "before_final_validation().await;",
    "validate_managed_library_operation(operation)",
    "activation.deactivate(self)",
  ]);
  assert.doesNotMatch(
    activation,
    /if candidates\.is_empty\(\)\s*\{\s*return Ok\(\(\)\)/,
    "empty activation batches must still cross final generation validation",
  );
  const activationBatch = state.slice(
    state.indexOf("struct KnownGoodActivationBatch"),
    state.indexOf("pub(crate) struct InstanceLifecycleLease"),
  );
  assert.match(activationBatch, /candidates:\s*Vec<\(String, String\)>/);
  assert.match(activationBatch, /inventory:\s*Arc<[^>]*KnownGoodInventory>/);
  assert.match(activationBatch, /deactivate_exact_inventory\s*\(/);

  const candidate = state.slice(
    state.indexOf("struct KnownGoodCandidateAdmission"),
    state.indexOf("pub(crate) struct InstanceLifecycleLease"),
  );
  assert.match(candidate, /library_operation:\s*Option<LibraryOperation>/);
  const candidateRevalidation = functionBlock(state, "revalidate");
  assert.match(
    candidateRevalidation,
    /validate_managed_library_operation\(operation\)\?/,
  );
  assert.match(
    state,
    /install_candidate_generation_rotation_deactivates_exact_inventory/,
  );
  assert.match(
    state,
    /install_acceptance_rotation_cleans_only_its_exact_inventory_batch/,
  );
  assert.match(state, /std::fs::rename\(&hook_library_root/);
  const exactCleanup = functionBlock(knownGood, "deactivate_exact_inventory");
  assert.doesNotMatch(exactCleanup, /normalize_library_root|library_root/);
  assert.match(exactCleanup, /expected_inventory/);

  const observer = functionBlock(state, "config_commit_observer");
  ordered(observer, [
    "let managed_identity_changed",
    "if managed_identity_changed",
    "known_good.clear_active()",
  ]);

  assert.doesNotMatch(
    install,
    /async fn await_managed_install_settlement<Mutation/,
  );
  const settlement = functionBlock(
    install,
    "await_managed_install_settlement_retaining",
  );
  ordered(settlement, ["install.await", "drop(authority)", "None"]);
});

test("Unix exact-name bindings use bounded retained-parent enumeration", async () => {
  const platform = await read("core/fs/src/platform.rs");
  const unix = platform.slice(
    platform.indexOf("#[cfg(unix)]"),
    platform.indexOf("#[cfg(windows)]"),
  );
  const absoluteValidation = functionBlock(
    unix,
    "validate_absolute_directory_guard",
  );
  const rootValidation = functionBlock(unix, "validate_root");
  for (const validation of [absoluteValidation, rootValidation]) {
    assert.match(validation, /if binding\.exact_name/);
    assert.match(validation, /exact_directory_binding_state/);
  }

  const exactBinding = functionBlock(unix, "exact_directory_binding_state");
  ordered(exactBinding, [
    "directory_binding_state(parent, name, expected)?",
    ".read()",
    "Some(&observed_revision)",
    ".write()",
    "let revision = directory_revision(parent)?",
    "*cached_revision = None",
    "Dir::read_from(parent)?",
    "observed == crate::MAX_DIRECTORY_LIST_ENTRIES",
    "entry_observation(parent, observed_name)?",
    "directory_revision(parent)? != revision",
  ]);
  assert.match(exactBinding, /observed_name != name/);
  assert.match(exactBinding, /exact directory binding parent exceeds its entry bound/);
  assert.equal((exactBinding.match(/Dir::read_from\(parent\)/g) ?? []).length, 1);
  assert.doesNotMatch(exactBinding, /read_dir|canonicalize|PathBuf/);
});

test("degraded version scans cache only retained passive facts", async () => {
  const versions = await read("core/minecraft/src/version/mod.rs");
  const scan = functionBlock(versions, "scan_versions_snapshot");
  const noProofFailures = [
    scan.slice(
      scan.indexOf("versions_root.open_observed_child"),
      scan.indexOf("let revision = version_dir.passive_revision"),
    ),
    scan.slice(
      scan.indexOf("guarded.observe_file(&json_name)"),
      scan.indexOf("let data = match guarded.directory.read_guarded_file_bounded"),
    ),
    scan.slice(
      scan.indexOf("guarded.directory.read_guarded_file_bounded"),
      scan.indexOf("let stub = match serde_json::from_slice"),
    ),
    scan.slice(
      scan.indexOf("guarded.observe_file(&jar_name)"),
      scan.indexOf("let mut versions = Vec::new()"),
    ),
  ];
  for (const failurePath of noProofFailures) {
    assert.match(failurePath, /Err\(_\)[\s\S]*dependencies_revalidatable = false/);
  }
  assert.match(
    scan,
    /VersionDirectoryEntryValidation::Unrevalidatable[\s\S]*dependencies_revalidatable = false/,
  );
  assert.match(
    scan,
    /let facts = if dependencies_revalidatable\s*\{\s*VersionScanDependencyFacts::Present/,
  );
});
