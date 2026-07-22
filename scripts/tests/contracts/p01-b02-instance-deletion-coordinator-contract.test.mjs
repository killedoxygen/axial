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

test("State owns one bounded instance deletion admission", async () => {
  const [state, coordinator] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/instance_deletions.rs"),
  ]);

  assert.match(state, /mod instance_deletions;/);
  assert.match(
    state,
    /instance_deletions:\s*instance_deletions::InstanceDeletionCoordinator/,
  );
  assert.match(
    state,
    /instance_deletions:\s*instance_deletions::InstanceDeletionCoordinator::new\(\)/,
  );
  assert.match(coordinator, /gate:\s*Arc<AsyncMutex<\(\)>>/);
  assert.match(coordinator, /phase:\s*Arc<AtomicU8>/);
  assert.match(
    coordinator,
    /pub\(super\) async fn admit\(&self\) -> io::Result<InstanceDeletionAdmission>/,
  );

  for (const marker of [
    "pub(crate) async fn delete_instance(",
    "pub(crate) async fn delete_pristine_setup_instance(",
  ]) {
    const deletion = braceBlock(state, marker);
    assert.match(deletion, /instance_deletions\s*\.admit\(\)\s*\.await/);
  }
});

test("shutdown settles deletions before dependent owners close", async () => {
  const shutdown = await read("apps/api/src/state/shutdown.rs");
  const coordinate = braceBlock(shutdown, "async fn coordinate");
  ordered(coordinate, [
    "wait_for_quiesced",
    "close_instance_deletions",
    "close_managed_compositions",
    "close_known_good_inventories",
    "close_user_mod_witnesses",
    "close_instance_registry",
    "close_managed_library",
  ]);

  for (const marker of [
    "async fn close_managed_compositions",
    "async fn close_performance_rules",
    "async fn close_known_good_inventories",
    "async fn close_user_mod_witnesses",
    "async fn close_instance_registry",
  ]) {
    const close = braceBlock(shutdown, marker);
    assert.match(close, /AppShutdownStep::InstanceDeletions/);
  }
});

test("failed deletion close reopens the exact admission for retry", async () => {
  const coordinator = await read(
    "apps/api/src/state/instance_deletions.rs",
  );
  const closeDrop = braceBlock(
    coordinator,
    "impl Drop for InstanceDeletionCloseAdmission",
  );
  assert.match(closeDrop, /InstanceDeletionPhase::Running/);
  assert.match(
    coordinator,
    /async fn failed_close_reopens_admission_for_shutdown_retry/,
  );
  assert.match(
    coordinator,
    /async fn close_waits_for_the_exact_in_flight_deletion/,
  );
});
