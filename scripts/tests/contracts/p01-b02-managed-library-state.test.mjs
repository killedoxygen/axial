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

const structBlock = (source, name) => {
  const marker = new RegExp(
    `(?:pub(?:\\([^)]*\\))?\\s+)?struct\\s+${name}(?:<[^>{}]*>)?\\s*\\{`,
  );
  const match = marker.exec(source);
  assert.ok(match, `missing struct ${name}`);
  const opening = source.indexOf("{", match.index);
  let depth = 0;
  for (let offset = opening; offset < source.length; offset += 1) {
    if (source[offset] === "{") depth += 1;
    if (source[offset] === "}") depth -= 1;
    if (depth === 0) return source.slice(match.index, offset + 1);
  }
  assert.fail(`unterminated struct ${name}`);
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
  assert.match(
    instances,
    /struct ReadinessLibraryAuthority\s*\{[\s\S]*library_operation:\s*ManagedLibraryOperation/,
  );
  const readinessAuthority = functionBlock(instances, "from_lookup");
  assert.match(
    readinessAuthority,
    /library_operation:\s*lookup\.managed_library_operation\(\)\.clone\(\)/,
  );

  const listEnrichment = functionBlock(instances, "enrich_instances_for_state");
  ordered(listEnrichment, [
    "ReadinessLibraryAuthority::from_lookup",
    "run_blocking_filesystem(move ||",
    "readiness_authority.as_ref()",
  ]);
  const singleEnrichment = functionBlock(
    instances,
    "enrich_instance_for_indexed_scan",
  );
  ordered(singleEnrichment, [
    "ReadinessLibraryAuthority::from_lookup",
    "run_blocking_filesystem(move ||",
    "readiness_authority.as_ref()",
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
    "settle_managed_install_publication(",
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
    "drive_loader_install_publication(",
    "drop(authority)",
  ]);
  assert.match(vanilla, /DownloadError::FileOperation\(error\)/);
  assert.match(loaderStart, /LoaderError::Io\(error\)/);

  const publicationSettlement = functionBlock(
    install,
    "settle_managed_install_publication",
  );
  ordered(publicationSettlement, [
    "classify_managed_install_publication(",
    "evidence.verify_install_receipt(receipt)",
    "record_install_publication_checkpoint(",
    "accept_verified_known_good_install_receipt",
    "acknowledgement.acknowledge().await",
  ]);
  const loaderPublication = functionBlock(
    loader,
    "drive_loader_install_publication",
  );
  assert.match(
    loaderPublication.slice(0, loaderPublication.indexOf("{")),
    /library_operation:\s*&crate::state::LibraryOperation/,
  );
  assert.match(loaderPublication, /settle_managed_install_publication\(/);

  const acceptance = functionBlock(
    state,
    "accept_verified_known_good_install_receipt",
  );
  const acceptanceSignature = acceptance.slice(0, acceptance.indexOf("{"));
  assert.match(acceptanceSignature, /operation:\s*&LibraryOperation/);
  assert.doesNotMatch(acceptanceSignature, /Path/);
  ordered(acceptance, [
    ".activate_with(",
    "self.accept_known_good_source(",
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
    "let activation = KnownGoodActivationBatch",
    "reconcile_known_good_instance(",
    "before_final_validation().await",
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
  assert.match(
    activationBatch,
    /source:\s*Arc<[^>]*KnownGoodActivationSource>/,
  );
  assert.match(activationBatch, /deactivate_exact_source\s*\(/);

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
    /install_acceptance_rotation_cleans_only_its_exact_source_batch/,
  );
  assert.match(state, /std::fs::rename\(&hook_library_root/);
  const exactCleanup = functionBlock(knownGood, "deactivate_exact_source");
  assert.doesNotMatch(exactCleanup, /normalize_library_root|library_root/);
  assert.match(exactCleanup, /expected_source/);

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
  const retainedSettlement = functionBlock(
    install,
    "await_managed_install_settlement_retaining",
  );
  ordered(retainedSettlement, ["install.await", "(result, authority)"]);
});

test("known-good startup is contract-bound and rehydrate-only", async () => {
  const [state, knownGood, rebuilds, application, install] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/known_good.rs"),
    read("apps/api/src/state/known_good_rebuilds.rs"),
    read("apps/api/src/application/known_good.rs"),
    read("apps/api/src/application/install.rs"),
  ]);

  assert.match(
    knownGood,
    /KNOWN_GOOD_SCHEMA:\s*&str\s*=\s*"axial\.state\.known_good_inventory\.v5"/,
  );
  assert.match(
    knownGood,
    /struct KnownGoodSnapshot\s*\{[\s\S]*activation_contract_id:\s*ManagedInstallActivationContractId/,
  );
  assert.match(knownGood, /KnownGoodPersistencePolicy::Install/);
  assert.match(knownGood, /KnownGoodPersistencePolicy::RehydrateExact/);
  assert.match(knownGood, /KnownGoodPersistencePolicy::BootstrapAbsent/);

  const rehydrate = functionBlock(
    rebuilds,
    "rehydrate_known_good_for_registered_instance",
  );
  ordered(rehydrate, [
    "capture_known_good_rebuild_target",
    "live_authority || target.activation_contract_id.is_none()",
    "rebuild_known_good_for_registered_instance_with_expected_incarnation",
  ]);

  const startup = functionBlock(
    application,
    "spawn_startup_known_good_rebuilds_with",
  );
  assert.match(
    startup,
    /\.rehydrate_known_good_for_registered_instance\(/,
  );
  assert.doesNotMatch(
    startup,
    /\.rebuild_known_good_for_registered_instance\(/,
  );

  const explicit = functionBlock(
    application,
    "rebuild_registered_known_good_with",
  );
  ordered(explicit, [
    "checkpoint_recovery_blocks_explicit_known_good_rebuild",
    "KnownGoodRebuildError::InstallRecoveryActive",
    "rebuild_known_good_for_registered_instance",
  ]);
  const gate = functionBlock(
    install,
    "checkpoint_recovery_blocks_explicit_known_good_rebuild",
  );
  assert.match(gate, /recovering_install_journals/);
  assert.match(gate, /InstallPublicationCheckpointKind::Committed/);
  assert.match(gate, /InstallPublicationCheckpointKind::BaseCommitted/);
  assert.match(gate, /InstallPublicationCheckpointKind::ChildCommitted/);
  assert.doesNotMatch(gate, /InstallPublicationCheckpointKind::RolledBack/);

  const activation = functionBlock(
    state,
    "activate_known_good_source_before_final_validation",
  );
  assert.match(
    activation,
    /required registered known-good activation target changed/,
  );
  assert.match(
    state,
    /bootstrap_later_io_failure_rolls_back_the_entire_live_source_batch/,
  );
});

test("known-good activation sources remain move-only", async () => {
  const knownGood = await read("core/minecraft/src/known_good.rs");
  const declaration = "pub struct KnownGoodActivationSource";
  const declarationOffset = knownGood.indexOf(declaration);
  assert.notEqual(declarationOffset, -1, "missing known-good activation source");
  const attributes =
    knownGood
      .slice(0, declarationOffset)
      .match(/(?:#\[[^\n]+\]\s*)+$/)?.[0] ?? "";
  assert.doesNotMatch(
    attributes,
    /#\[derive\([^)]*\bClone\b[^)]*\)\]/,
    "KnownGoodActivationSource must remain move-only",
  );
});

test("known-good durable authority stays source-bound and verified-only", async () => {
  const [
    state,
    tier2,
    findings,
    reconciliation,
    contracts,
    failureMemory,
    versionBundle,
    minecraft,
  ] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/known_good_tier2.rs"),
    read("apps/api/src/state/registered_artifact_findings.rs"),
    read("apps/api/src/state/reconciliation.rs"),
    read("apps/api/src/state/contracts.rs"),
    read("apps/api/src/state/failure_memory.rs"),
    read("core/minecraft/src/version_bundle_publication.rs"),
    read("core/minecraft/src/lib.rs"),
  ]);

  const retiredCheckpointVocabulary =
    /RegisteredVersionBundlePublicationCheckpoint|VERSION_BUNDLE_PUBLICATION_CHECKPOINT_STEP|version_bundle_publication_checkpoint/;
  assert.doesNotMatch(
    [state, reconciliation, versionBundle, minecraft].join("\n"),
    retiredCheckpointVocabulary,
  );

  const sourceAuthorities = [
    [state, "KnownGoodVerificationLease"],
    [tier2, "KnownGoodTier2Ticket"],
    [tier2, "KnownGoodTier2CleanSeal"],
    [tier2, "KnownGoodTier2CleanReceipt"],
    [reconciliation, "RegisteredVersionBundleComponentRebuildEffect"],
    [reconciliation, "RecordedReconciliationFailure"],
    [reconciliation, "CurrentReconciliationIncarnation"],
    [findings, "RegisteredArtifactRepairAdmission"],
  ];
  for (const [source, name] of sourceAuthorities) {
    const authority = structBlock(source, name);
    assert.match(
      authority,
      /source:\s*(?:std::sync::)?(?:Arc|Weak)<[^>]*KnownGoodActivationSource>/,
      `${name} must retain its exact activation source`,
    );
    assert.doesNotMatch(
      authority,
      /(?:std::sync::)?(?:Arc|Weak)<[^>]*KnownGoodInventory>/,
      `${name} cannot retain a raw inventory as durable authority`,
    );
  }
  const findingsEvidence = structBlock(state, "KnownGoodVerificationLease");
  assert.doesNotMatch(
    findingsEvidence,
    /pub\(crate\)\s+fn\s+inventory\s*\(/,
  );

  assert.match(
    contracts,
    /pub enum ReconciliationScope\s*\{\s*RegisteredInstance\s*\{[\s\S]*?activation_contract_id:\s*ManagedInstallActivationContractId,/,
  );
  const memoryKey = functionBlock(failureMemory, "for_reconciliation_parts");
  ordered(memoryKey, [
    "let ReconciliationScope::RegisteredInstance",
    "activation_contract_id,",
    "inventory_fingerprint.as_str()",
    "activation_contract_id",
  ]);

  const publicAcceptanceNames = [
    ...state.matchAll(
      /pub\(crate\)\s+async fn\s+(accept_[a-z0-9_]*(?:known_good|loader_base)[a-z0-9_]*)\s*\(/g,
    ),
  ].map((match) => match[1]);
  assert.deepEqual(publicAcceptanceNames, [
    "accept_verified_known_good_install_receipt",
    "accept_verified_known_good_reconstruction_receipt",
    "accept_verified_known_good_checkpoint",
    "accept_verified_registered_known_good_checkpoint",
    "accept_verified_registered_known_good_bootstrap",
    "accept_verified_loader_base_commit",
    "accept_verified_loader_base_checkpoint",
  ]);
  assert.doesNotMatch(
    state,
    /pub\(crate\)\s+async fn\s+accept_known_good_source/,
  );
  assert.doesNotMatch(
    state,
    /\baccept_known_good_(?:install_receipt|reconstruction_receipt|activation_source)\b/,
  );
});

test("VersionBundle publication acknowledgement is exact and restart-durable", async () => {
  const [
    contracts,
    reconciliation,
    failureMemory,
    journals,
    guardian,
    reconstruction,
    publicationCore,
    managedPublication,
    managedFs,
    installCore,
  ] = await Promise.all([
    read("apps/api/src/state/contracts.rs"),
    read("apps/api/src/state/reconciliation.rs"),
    read("apps/api/src/state/failure_memory.rs"),
    read("apps/api/src/state/journals.rs"),
    read("apps/api/src/guardian/component_rebuild.rs"),
    read("core/minecraft/src/known_good_reconstruction.rs"),
    read("core/minecraft/src/version_bundle_publication.rs"),
    read("core/minecraft/src/managed_publication.rs"),
    read("core/minecraft/src/managed_fs.rs"),
    read("core/minecraft/src/download/install.rs"),
  ]);

  assert.match(
    publicationCore,
    /const INTENT_SCHEMA:\s*&str\s*=\s*"axial\.version_bundle_publication\.intent\.v4"/,
  );
  assert.match(
    publicationCore,
    /const OUTCOME_SCHEMA:\s*&str\s*=\s*"axial\.version_bundle_publication\.outcome\.v3"/,
  );
  assert.match(
    publicationCore,
    /const SETTLEMENT_SCHEMA:\s*&str\s*=\s*"axial\.version_bundle_publication\.settlement\.v5"/,
  );
  assert.match(
    publicationCore,
    /enum VersionBundlePublicationPurpose\s*\{[\s\S]*Install,[\s\S]*GuardianRebuild,/,
  );
  assert.match(
    structBlock(publicationCore, "PersistedIntent"),
    /purpose:\s*VersionBundlePublicationPurpose/,
  );
  assert.match(
    functionBlock(publicationCore, "classify_durable_version_bundle_owned"),
    /intent\.purpose\s*!=\s*purpose/,
  );
  assert.match(
    functionBlock(publicationCore, "durable_classification_from_settlement"),
    /settlement\.intent\.purpose\s*!=\s*purpose/,
  );
  assert.match(installCore, /VersionBundlePublicationPurpose::Install/);
  assert.match(
    reconstruction,
    /VersionBundlePublicationPurpose::GuardianRebuild/,
  );

  const terminal = structBlock(contracts, "ReconciliationTerminal");
  assert.match(
    terminal,
    /version_bundle_publication:\s*Option<ReconciliationVersionBundlePublication>/,
  );
  assert.doesNotMatch(terminal, /serde\s*\(\s*default/);
  const publication = structBlock(
    contracts,
    "ReconciliationVersionBundlePublication",
  );
  assert.match(
    publication,
    /evidence:\s*axial_minecraft::ManagedInstallPublicationEvidenceId/,
  );
  assert.match(
    publication,
    /outcome:\s*ReconciliationVersionBundleOutcome/,
  );
  assert.match(publication, /acknowledged:\s*bool/);
  assert.match(
    contracts,
    /pub enum ReconciliationVersionBundleOutcome\s*\{[\s\S]*Committed,[\s\S]*RolledBack,/,
  );
  assert.match(
    functionBlock(contracts, "is_pending"),
    /!self\.acknowledged/,
  );

  const publicationLease = functionBlock(
    reconciliation,
    "version_bundle_publication",
  );
  assert.match(publicationLease, /\.evidence_id\(\)/);
  assert.match(
    publicationLease,
    /ReconciliationVersionBundleOutcome::Committed/,
  );
  assert.match(
    publicationLease,
    /ReconciliationVersionBundleOutcome::RolledBack/,
  );
  const canonicalMemory = functionBlock(
    reconciliation,
    "reconciliation_memory_entry",
  );
  assert.match(
    canonicalMemory,
    /\.with_reconciliation_terminal\(terminal\)/,
  );

  const terminalize = functionBlock(
    guardian,
    "terminalize_version_bundle_component_rebuild",
  );
  ordered(terminalize, [
    "persist_managed_artifact_component_terminal(",
    ".await?",
    "settlement.version_bundle_publication_acknowledgement()",
    "settlement.into_version_bundle_publication()",
    "acknowledge_version_bundle_publication(publication, acknowledgement).await?",
  ]);
  const normalAcknowledgement = functionBlock(
    guardian,
    "acknowledge_version_bundle_publication",
  );
  ordered(normalAcknowledgement, [
    "receipt.acknowledge().await",
    "ManagedVersionBundleAcknowledgementOutcome::Acknowledged",
    "acknowledgement.record().await",
  ]);

  const productionReconciliation = reconciliation.slice(
    0,
    reconciliation.indexOf("#[cfg(test)]\nmod tests"),
  );
  const startupValidation = functionBlock(
    productionReconciliation,
    "validate_startup_version_bundle_publication",
  );
  ordered(startupValidation, [
    "current_reconciliation_incarnation(instance_id)",
    "fingerprint != &current.fingerprint",
    "inventory_fingerprint != &current.inventory_fingerprint",
    "activation_contract_id != current.source.activation_contract_id()",
    ".matches_version_id(current.source.version_id())",
    "let operation = self.try_acquire_managed_library()?",
    "operation.configured_path() != current.roots.library",
    "self.validate_managed_library_operation(&operation)?",
    "Ok((operation, current.source))",
  ]);
  assert.match(
    reconstruction,
    /Acquire\s*\{[\s\S]*managed_root:\s*ManagedLibraryOperation/,
  );
  assert.match(
    functionBlock(reconstruction, "acquire_version_bundle_publication_lease"),
    /ManagedRootPublicationLease::try_acquire\(guarded_root\)/,
  );
  const tryPublicationLease = functionBlock(
    managedPublication,
    "try_acquire",
  );
  ordered(tryPublicationLease, [
    "root_mutex.try_lock_owned()",
    "return Ok(None)",
    "Self::acquire_with_guard(root, in_process_guard, false).await",
  ]);
  const completionAuthority = structBlock(
    productionReconciliation,
    "ManagedArtifactCompletionAuthority",
  );
  assert.match(completionAuthority, /library_operation:\s*Option<LibraryOperation>/);
  for (const functionName of [
    "begin_version_bundle_commit",
    "settle_version_bundle_rollback",
  ]) {
    const settlement = functionBlock(productionReconciliation, functionName);
    assert.match(settlement, /library_operation_is_current\(\)/);
    assert.match(
      settlement,
      /receipt\.matches_managed_library\(operation\.core\(\)\)/,
    );
    assert.doesNotMatch(settlement, /matches_root/);
  }
  assert.match(
    functionBlock(productionReconciliation, "is_live_with"),
    /ManagedArtifactRebuildComponent::VersionBundle[\s\S]*library_operation_is_current\(\)/,
  );
  assert.doesNotMatch(reconstruction, /settled_version_bundle_matches_root/);
  const authorityMatch = functionBlock(
    managedFs,
    "shares_managed_library_operation",
  );
  assert.match(
    authorityMatch,
    /Arc::ptr_eq\(&self\.inner\.root,\s*&operation\.authority\.root\.inner\.root\)/,
  );
  assert.match(
    authorityMatch,
    /Arc::ptr_eq\(pin,\s*&operation\.pin\)/,
  );
  const settledAuthorityMatch = functionBlock(
    publicationCore,
    "settled_version_bundle_matches_managed_library",
  );
  ordered(settledAuthorityMatch, [
    "lease.revalidate().is_ok()",
    "lease.root().shares_managed_library_operation(expected)",
    "lease.revalidate().is_ok()",
  ]);
  const startupSettlementOffset = productionReconciliation.indexOf(
    "async fn settle_startup_version_bundle_publication(",
  );
  assert.notEqual(
    startupSettlementOffset,
    -1,
    "missing singular VersionBundle startup settlement",
  );
  const startupSettlement = functionBlock(
    productionReconciliation.slice(startupSettlementOffset),
    "settle_startup_version_bundle_publication",
  );
  assert.match(
    startupSettlement,
    /ManagedVersionBundleExpectedSettlement::Committed/,
  );
  assert.match(
    startupSettlement,
    /ManagedVersionBundleExpectedSettlement::RolledBack/,
  );
  ordered(startupSettlement, [
    "recover_managed_version_bundle_acknowledgement(",
    "ManagedVersionBundleAcknowledgementOutcome::NoSettlement",
    "ManagedVersionBundleAcknowledgementOutcome::Mismatch",
    "validate_startup_version_bundle_publication(",
    "acknowledge_reconciliation_version_bundle_publication(",
  ]);
  const acknowledgementConvergence = functionBlock(
    reconciliation,
    "converge_version_bundle_publication_acknowledgement",
  );
  assert.match(
    acknowledgementConvergence,
    /match\s*\(journal_pending,\s*memory_pending\)/,
  );
  assert.match(acknowledgementConvergence, /\(true,\s*false\)/);
  assert.match(
    acknowledgementConvergence,
    /\(false,\s*true\)[\s\S]*journal was acknowledged before its failure memory/,
  );
  const durableAcknowledgement = functionBlock(
    productionReconciliation,
    "acknowledge_reconciliation_version_bundle_publication",
  );
  ordered(durableAcknowledgement, [
    "self.failure_memory",
    ".acknowledge_reconciliation_version_bundle_publication(",
    ".await",
    "self.journals",
    ".acknowledge_reconciliation_version_bundle_publication(expected)",
  ]);
  const startup = functionBlock(
    reconciliation,
    "reconcile_reconciliation_startup",
  );
  ordered(startup, [
    "settle_reconciliation_pending()",
    "converge_existing_version_bundle_publication_acknowledgements()",
    "let referenced_predecessors",
    "reconciliation_memory_entry(terminal)",
    "commit_reconciliation_memory(",
    "converge_acknowledged_version_bundle_publications()",
  ]);
  assert.match(
    startup,
    /!referenced_predecessors\.contains\(terminal\.operation_id\(\)\)/,
  );

  const orphanRequirements = functionBlock(
    productionReconciliation,
    "startup_version_bundle_orphan_requirements",
  );
  ordered(orphanRequirements, [
    "ReconciliationLineage::Predecessor",
    "planned VersionBundle recovery predecessor is missing",
    "let predecessor_key = reconciliation_attempt_key(predecessor.attempt())",
    "let predecessor_memory = memories",
    "reconciliation_memory_entry(predecessor.clone())",
    "reconciliation_attempt_key(attempt)",
  ]);
  const startupPublications = functionBlock(
    productionReconciliation,
    "settle_startup_version_bundle_publications",
  );
  ordered(startupPublications, [
    "settle_startup_version_bundle_orphan().await?",
    "converge_acknowledged_version_bundle_publications()",
  ]);
  const orphanSettlement = functionBlock(
    productionReconciliation,
    "settle_startup_version_bundle_orphan",
  );
  ordered(orphanSettlement, [
    "recover_guardian_version_bundle_orphan(",
    "ManagedVersionBundleSettlementOutcome::Committed",
    "ReconciliationVersionBundleOutcome::Committed",
    "ReconciliationTerminalOutcome::Failed",
    ".with_version_bundle_publication(evidence, publication_outcome)",
    "record_reconciliation_journal_failure(",
    "commit_reconciliation_memory(",
    "settlement.acknowledge().await",
    "acknowledge_reconciliation_version_bundle_publication(&terminal)",
  ]);
  assert.match(
    orphanSettlement,
    /Receipt loss also loses the exact failed-artifact postcheck authority/,
  );

  const journalPruning = functionBlock(journals, "prune_records");
  ordered(journalPruning, [
    "let referenced_predecessors",
    "ReconciliationLineage::Predecessor",
    "!referenced_predecessors.contains(*key)",
  ]);

  assert.match(
    reconstruction,
    /expected\.matches_evidence\(&evidence\)[\s\S]*ManagedVersionBundleAcknowledgementOutcome::Mismatch/,
  );
  assert.match(
    reconstruction,
    /ManagedVersionBundleAcknowledgementOutcome::NoSettlement/,
  );

  for (const [source, functionName] of [
    [journals, "active_reconciliation_terminal"],
    [failureMemory, "active_durable_terminal"],
  ]) {
    const protection = functionBlock(source, functionName);
    ordered(protection, [
      "terminal",
      ".version_bundle_publication()",
      ".is_some_and(|publication| publication.is_pending())",
      "terminal.suppression_until()",
    ]);
  }
});

test("Unix exact-name bindings use bounded retained-parent enumeration", async () => {
  const platform = await read("core/fs/src/platform.rs");
  const unixMarker = "#[cfg(unix)]\nmod native {";
  const windowsMarker = "#[cfg(windows)]\nmod native {";
  const unixStart = platform.indexOf(unixMarker);
  const windowsStart = platform.indexOf(windowsMarker);
  assert.notEqual(unixStart, -1, "missing Unix platform module");
  assert.notEqual(windowsStart, -1, "missing Windows platform module");
  const unix = platform.slice(
    unixStart,
    windowsStart,
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
    "exact_directory_name_observation(parent, name, expected)?",
    "let final_state = directory_binding_state(parent, name, expected)?",
    "let final_revision = directory_revision(parent)?",
    "confirmed_state = directory_binding_state(parent, name, expected)?",
    "directory_revision(parent)? != final_revision",
  ]);
  const exactName = functionBlock(unix, "exact_directory_name_observation");
  ordered(exactName, [
    "Dir::read_from(parent)?",
    "observed == crate::MAX_DIRECTORY_LIST_ENTRIES",
    "entry_observation(parent, observed_name)?",
  ]);
  assert.match(exactName, /observed_name != name/);
  assert.match(exactName, /exact directory binding parent exceeds its entry bound/);
  assert.equal((exactName.match(/Dir::read_from\(parent\)/g) ?? []).length, 1);
  assert.doesNotMatch(`${exactBinding}\n${exactName}`, /read_dir|canonicalize|PathBuf/);
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
