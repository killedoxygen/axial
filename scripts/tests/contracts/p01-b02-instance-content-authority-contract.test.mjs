import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
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

async function readRustTree(root) {
  const sources = [];
  const visit = async (relative) => {
    for (const entry of await readdir(new URL(`${relative}/`, repository), {
      withFileTypes: true,
    })) {
      const child = `${relative}/${entry.name}`;
      if (entry.isDirectory()) await visit(child);
      else if (entry.isFile() && entry.name.endsWith(".rs")) {
        sources.push([child, await read(child)]);
      }
    }
  };
  await visit(root);
  return sources;
}

test("lifecycle acquisition rejects a queued retired-gate incarnation", async () => {
  const lifecycle = await read("apps/api/src/state/instance_lifecycle.rs");
  assert.match(lifecycle, /struct InstanceLifecycleGate\s*\{/);
  assert.match(lifecycle, /retired:\s*AtomicBool/);
  assert.match(
    lifecycle,
    /struct InstanceLifecycleIncarnation\s*\{\s*gate:\s*Arc<InstanceLifecycleGate>/,
  );
  const acquire = braceBlock(lifecycle, "async fn acquire_from");
  ordered(acquire, [
    "let guard = Arc::clone(&gate.lock).lock_owned().await",
    "let incarnation = InstanceLifecycleIncarnation { gate }",
    "if incarnation.is_retired()",
    "drop(guard)",
    "continue",
  ]);
  const aba = braceBlock(
    lifecycle,
    "async fn queued_waiter_rejects_the_retired_incarnation_after_locking",
  );
  ordered(aba, [
    'let retired = gates.acquire("instance").await',
    'let queued_candidate = gates.gate("instance").await',
    '.acquire_from("instance", Some(queued_candidate))',
    "tokio::time::timeout",
    "retired_incarnation.retire()",
    "drop(retired)",
    "waiter.await",
    "!retired_incarnation.same(replacement.incarnation())",
  ]);
});

test("State admission and directories retain one non-escaping App context", async () => {
  const state = await read("apps/api/src/state/mod.rs");
  const admission = braceBlock(
    state,
    "pub(crate) struct ManagedInstanceContentAdmission",
  );
  assert.match(admission, /lifecycle:\s*InstanceLifecycleLease/);
  assert.match(admission, /generation:\s*Instance/);
  assert.match(admission, /admission:\s*tokio::sync::OwnedRwLockReadGuard/);
  assert.doesNotMatch(admission, /ManagedTreeDirectory/);

  const context = braceBlock(state, "struct ManagedInstanceContentContext");
  assert.match(context, /lifecycle:\s*InstanceLifecycleLease/);
  assert.match(context, /generation:\s*Instance/);
  assert.match(context, /operation:\s*Option<ManagedTreeOperation>/);
  assert.match(context, /instances:\s*Arc<AppInstanceStore>/);

  const directory = braceBlock(
    state,
    "pub(crate) struct ManagedInstanceContentDirectory",
  );
  ordered(directory, [
    "directory: ManagedTreeDirectory",
    "context: Arc<ManagedInstanceContentContext>",
  ]);
  const contextDrop = braceBlock(
    state,
    "impl Drop for ManagedInstanceContentContext",
  );
  ordered(contextDrop, [
    "drop(self.operation.take())",
    "self.instances.release_managed_game_directory",
  ]);
  const wrapper = braceBlock(state, "impl ManagedInstanceContentDirectory");
  assert.match(wrapper, /open_child[\s\S]*Arc::clone\(&self\.context\)/);
  assert.match(
    wrapper,
    /open_or_create_child[\s\S]*Arc::clone\(&self\.context\)/,
  );
  assert.match(wrapper, /Arc::ptr_eq\(&self\.context,\s*&source\.context\)/);
  assert.match(
    state,
    /assert_not_impl_any!\(ManagedInstanceContentAuthority:\s*Clone\)/,
  );
  assert.match(
    state,
    /assert_not_impl_any!\(ManagedInstanceContentAdmission:\s*Clone\)/,
  );
  assert.match(
    state,
    /assert_not_impl_any!\(ManagedInstanceContentDirectory:\s*Clone\)/,
  );
  assert.match(
    state,
    /a child directory must retain the complete App authority context/,
  );
});

test("Store roots are operation-scoped and retain only unresolved retirement", async () => {
  const [registry, managedFs] = await Promise.all([
    read("apps/api/src/state/instance_registry.rs"),
    read("core/minecraft/src/managed_fs.rs"),
  ]);
  const rawDirectory = braceBlock(managedFs, "impl ManagedTreeDirectory");
  assert.doesNotMatch(rawDirectory, /from_directory|fn open\(/);
  const coreOperation = braceBlock(managedFs, "impl ManagedTreeOperation");
  assert.match(coreOperation, /pub fn directory\(&self\)/);
  assert.match(coreOperation, /with_operation_pin/);
  assert.doesNotMatch(coreOperation, /pub fn revalidate/);
  const coreRoot = braceBlock(managedFs, "impl ManagedTreeRoot");
  assert.doesNotMatch(coreRoot, /pub fn revalidate/);
  assert.match(
    registry,
    /instance_content_admission:\s*Arc<RwLock<\(\)>>/,
  );
  assert.match(
    registry,
    /instance_content_roots:\s*Mutex<HashMap<String, ManagedInstanceContentRoot>>/,
  );
  const slot = braceBlock(registry, "struct ManagedInstanceContentRoot");
  assert.match(slot, /incarnation:\s*InstanceLifecycleIncarnation/);
  assert.match(slot, /root:\s*Option<ManagedTreeRoot>/);
  assert.match(slot, /retirement:\s*Option<Arc<ManagedTreeRetirement>>/);
  assert.match(slot, /settlement:\s*Arc<AsyncMutex<\(\)>>/);
  assert.match(registry, /const INSTANCE_CONTENT_ROOT_LIMIT:\s*usize\s*=\s*64/);
  assert.match(
    registry,
    /instance_content_root_count:\s*AtomicUsize/,
  );

  const mint = braceBlock(registry, "pub(super) fn managed_game_directory");
  assert.match(mint, /expected:\s*&Instance/);
  assert.match(mint, /incarnation:\s*&InstanceLifecycleIncarnation/);
  assert.match(mint, /_admission:\s*&OwnedRwLockReadGuard/);
  ordered(mint, [
    "self.get(&expected.id).as_ref() != Some(expected)",
    "self.settle_retained_instance_content_root",
    "self.reserve_instance_content_root()?",
    "instance_directory.create_effect_owner()?",
    "ManagedTreeRoot::from_directory(instance_directory, effects)?",
    "root.try_acquire()?",
    "operation.directory()?",
    "ManagedInstanceContentRoot::active(incarnation.clone(), root)",
    "self.require_registered_instance_unchanged(&expected.id, expected)",
  ]);
  assert.doesNotMatch(mint, /ManagedTreeDirectory::from_directory/);

  const release = braceBlock(
    registry,
    "pub(super) fn release_managed_game_directory",
  );
  ordered(release, [
    "root.begin_retirement()",
    "tokio::runtime::Handle::try_current()",
    "runtime.spawn(async move",
    "settle_instance_content_retirement",
    "store.remove_settled_instance_content_root",
  ]);
  assert.match(
    registry,
    /managed_instance_content_root_capacity_is_bounded/,
  );
  assert.match(
    registry,
    /managed_game_directory_preserves_a_lexical_replacement_after_binding_loss/,
  );
  const retained = braceBlock(
    registry,
    "fn settle_retained_instance_content_root",
  );
  ordered(retained, [
    "root.retirement.as_ref().cloned()",
    "settlement.blocking_lock_owned()",
    "retirement.try_drain_and_settle()?",
    "Some(())",
    "self.remove_settled_instance_content_root",
  ]);
  const blockingRetirement = braceBlock(
    registry,
    "async fn settle_instance_content_retirement",
  );
  ordered(blockingRetirement, [
    "settlement.lock_owned().await",
    "retirement.wait_for_drain().await?",
    "instance_content_settlement_gate()",
    ".acquire_owned()",
    "tokio::task::spawn_blocking",
    "retirement.settle_drained()",
  ]);
  const coreRetirement = braceBlock(managedFs, "impl ManagedTreeRetirement");
  assert.match(coreRetirement, /pub async fn wait_for_drain/);
  assert.match(coreRetirement, /pub fn settle_drained/);
  assert.doesNotMatch(coreRetirement, /pub async fn drain_and_settle/);
});

test("delete and close drain roots before filesystem retirement", async () => {
  const [state, coordinator, registry] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/instance_deletions.rs"),
    read("apps/api/src/state/instance_registry.rs"),
  ]);
  const deletion = braceBlock(coordinator, "async fn prepare_auxiliaries");
  ordered(deletion, [
    "retire_managed_game_directory(&instance_id, lifecycle.incarnation())",
    "retire_existing_managed",
    "reserve_retirement",
  ]);
  const drive = braceBlock(coordinator, "async fn drive_deletion_once");
  ordered(drive, [
    "prepared.persist().await",
    "auxiliaries.commit(state).await",
    "committed.settle_files().await",
  ]);
  const admitted = braceBlock(state, "async fn delete_instance_admitted");
  ordered(admitted, [
    "acquire_integrity_instance_lifecycle",
    ".delete_admitted(",
  ]);
  const close = braceBlock(registry, "pub(super) async fn close_admitted");
  ordered(close, [
    "InstanceRegistryCloseTransition::begin",
    "close_instance_content_roots()",
    "reconcile_obligations(close).await?",
    ".owner",
    ".close()",
  ]);
  const prepare = braceBlock(registry, "pub(super) async fn prepare_delete_with_gate");
  ordered(prepare, [
    "require_no_instance_content_root(&instance_id)",
    "reconcile_obligations(gate).await",
    "prepare_instance_deletion_files",
  ]);
  assert.match(registry, /close_waits_for_an_escaped_content_directory_pin/);
  assert.match(
    registry,
    /deletion_retirement_waits_for_an_escaped_content_directory_pin/,
  );
  assert.match(
    registry,
    /canceled_close_waiting_for_content_context_reopens_admission/,
  );
});

test("filesystem activation occurs only inside the admitted blocking worker", async () => {
  const [state, resources] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/application/instances/resources.rs"),
  ]);
  const admit = braceBlock(
    state,
    "pub(crate) async fn admit_instance_content_authority",
  );
  assert.match(admit, /acquire_instance_content_admission\(\)\s*\.await/);
  ordered(admit, [
    "instance_lifecycle_gates.owns",
    ".has_active_instance(&lifecycle.instance_id)",
    "acquire_instance_content_admission()",
    ".has_active_instance(&generation.id)",
  ]);
  assert.doesNotMatch(
    admit,
    /managed_game_directory|create_effect_owner|ManagedTreeRoot|open_directory/,
  );
  const activate = braceBlock(state, "pub(crate) fn activate");
  assert.match(activate, /managed_game_directory/);
  assert.match(
    state,
    /instance_content_authority_rejects_an_active_session_after_lifecycle_acquisition/,
  );

  const backup = braceBlock(
    resources,
    "pub(crate) async fn handle_backup_instance_world",
  );
  ordered(backup, [
    "admit_instance_content_authority(lifecycle_guard)",
    ".await",
    ".run(move ||",
    "content_admission",
    ".activate()",
    "content_authority.directory()",
  ]);
});

test("Application has no raw managed-tree or store-mint bypass", async () => {
  const sources = await readRustTree("apps/api/src/application");
  for (const [path, source] of sources) {
    assert.doesNotMatch(
      source,
      /ManagedTreeDirectory|managed_game_directory/,
      `${path} bypasses State-owned instance content authority`,
    );
  }
  const resources = await read(
    "apps/api/src/application/instances/resources.rs",
  );
  const copy = braceBlock(resources, "pub(super) fn copy_world_backup_staged");
  assert.match(copy, /source:\s*&ManagedInstanceContentDirectory/);
  assert.match(copy, /backup_root:\s*&ManagedInstanceContentDirectory/);
  assert.doesNotMatch(resources, /trait WorldBackupTree/);
});

test("architecture records capacity, settlement, and incarnation boundaries", async () => {
  const architecture = await read("docs/ARCHITECTURE.md");
  assert.match(architecture, /async phase performs no filesystem work/);
  assert.match(architecture, /operation-scoped `ManagedTreeRoot`/);
  assert.match(architecture, /never a raw path or `ManagedTreeDirectory`/);
  assert.match(architecture, /per-root single-flight cleanup/);
  assert.match(architecture, /small bounded blocking lane/);
  assert.match(architecture, /hard 64-root capacity/);
  assert.match(architecture, /centrally rejects active launch sessions/);
  assert.match(architecture, /waiter queued on the old gate must retry/);
});
