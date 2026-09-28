import assert from "node:assert/strict";
import { createHash } from "node:crypto";
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

test("instance registry v3 owns strict identity-bound tombstones", async () => {
  const [manifest, instances, library] = await Promise.all([
    read("core/config/Cargo.toml"),
    read("core/config/src/instances/mod.rs"),
    read("core/config/src/lib.rs"),
  ]);

  assert.match(manifest, /^sha2\.workspace = true$/m);
  assert.match(
    instances,
    /pub const INSTANCE_REGISTRY_SCHEMA_VERSION: u32 = 3;/,
  );
  assert.doesNotMatch(
    instances,
    /pub const INSTANCE_REGISTRY_SCHEMA_VERSION: u32 = 2;/,
  );

  const pending = braceBlock(instances, "pub struct PendingInstanceDeletion");
  assert.match(
    instances.slice(
      Math.max(0, instances.indexOf("pub struct PendingInstanceDeletion") - 160),
      instances.indexOf("pub struct PendingInstanceDeletion"),
    ),
    /#\[serde\(deny_unknown_fields\)\]/,
  );
  assert.match(pending, /pub instance_id: String/);
  assert.match(pending, /pub created_at: String/);
  assert.match(pending, /pub tombstone_name: String/);
  assert.doesNotMatch(pending, /serde\((?:default|alias)/);

  const snapshot = braceBlock(instances, "pub struct InstanceRegistrySnapshot");
  assert.match(
    snapshot,
    /pub pending_deletions: Vec<PendingInstanceDeletion>/,
  );
  assert.doesNotMatch(snapshot, /pending_deletions: Vec<String>/);
  assert.match(library, /PendingInstanceDeletion/);
  assert.match(library, /derive_instance_tombstone_name/);
});

test("tombstone names use one exact lowercase domain-separated derivation", async () => {
  const instances = await read("core/config/src/instances/mod.rs");
  const derivation = braceBlock(
    instances,
    "pub fn derive_instance_tombstone_name",
  );

  assert.match(
    instances,
    /INSTANCE_TOMBSTONE_NAME_PREFIX: &str = "\.axial-instance-tombstone-v1-"/,
  );
  assert.match(
    instances,
    /INSTANCE_TOMBSTONE_HASH_DOMAIN: &\[u8\] = b"axial\.instance-tombstone\.v1"/,
  );
  assert.match(derivation, /is_canonical_instance_id\(instance_id\)/);
  assert.match(derivation, /is_valid_timestamp\(created_at, false\)/);
  assert.match(derivation, /hasher\.update\(INSTANCE_TOMBSTONE_HASH_DOMAIN\)/);
  assert.match(derivation, /hasher\.update\(instance_id\.as_bytes\(\)\)/);
  assert.match(derivation, /hasher\.update\(created_at\.as_bytes\(\)\)/);
  assert.match(derivation, /name\.push_str\(instance_id\)/);
  assert.match(derivation, /const LOWER_HEX: &\[u8; 16\] = b"0123456789abcdef"/);

  const digest = createHash("sha256")
    .update("axial.instance-tombstone.v1")
    .update(Buffer.from([0]))
    .update("0000000000000002")
    .update(Buffer.from([0]))
    .update("2026-01-01T00:00:00Z")
    .digest("hex");
  const exact = `.axial-instance-tombstone-v1-0000000000000002-${digest}`;
  assert.match(instances, new RegExp(exact));

  const validation = braceBlock(
    instances,
    "impl PendingInstanceDeletion",
  );
  assert.match(
    validation,
    /let expected = derive_instance_tombstone_name\(&self\.instance_id, &self\.created_at\)/,
  );
  assert.match(validation, /self\.tombstone_name != expected/);
});

test("registry rejects ambiguous ownership and bounds canonical persistence", async () => {
  const instances = await read("core/config/src/instances/mod.rs");
  const snapshotImpl = braceBlock(instances, "impl InstanceRegistrySnapshot");

  assert.match(
    snapshotImpl,
    /\.checked_add\(self\.pending_deletions\.len\(\)\)/,
  );
  assert.match(snapshotImpl, /total > INSTANCE_REGISTRY_MAX_ENTRIES/);
  assert.match(snapshotImpl, /self\.pending_deletions\.len\(\) > 1/);
  assert.match(snapshotImpl, /ids\.contains\(pending\.instance_id\.as_str\(\)\)/);
  assert.match(snapshotImpl, /pending_ids\.insert\(pending\.instance_id\.as_str\(\)\)/);
  assert.match(
    snapshotImpl,
    /tombstone_names\.insert\(pending\.tombstone_name\.as_str\(\)\)/,
  );
  assert.match(snapshotImpl, /serde_json::to_vec_pretty\(self\)/);
  assert.match(snapshotImpl, /encoded\.len\(\) as u64 > INSTANCE_REGISTRY_MAX_BYTES/);
  assert.match(instances, /fn schema_v2_is_rejected_without_migration_or_rewrite/);
  assert.doesNotMatch(snapshotImpl, /serde\((?:default|alias)/);
});
