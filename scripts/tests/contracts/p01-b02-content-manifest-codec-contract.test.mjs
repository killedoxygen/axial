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

test("managed content manifest codec is strict bounded and path-free", async () => {
  const source = await read("core/content/src/manifest.rs");
  const decode = braceBlock(source, "pub fn decode_managed");
  assert.doesNotMatch(decode, /\bPath(?:Buf)?\b|\bfs::|manifest_path|read_manifest/);
  ordered(decode, [
    "let Some(bytes) = bytes else",
    "ManifestOrigin::Missing",
    "bytes.len() > MAX_MANIFEST_BYTES",
    "serde_json::from_slice(bytes)",
    "manifest.validate()?",
    "ManifestOrigin::Present(manifest_digest(bytes))",
  ]);

  const encode = braceBlock(source, "pub fn encode_managed");
  assert.doesNotMatch(encode, /\bPath(?:Buf)?\b|\bfs::|manifest_path|write_all/);
  ordered(encode, [
    "self.validate()?",
    "serde_json::to_vec_pretty(self)?",
    "body.len() > MAX_MANIFEST_BYTES",
    "Ok(body)",
  ]);

  assert.match(source, /const MANIFEST_SCHEMA_VERSION:\s*u32\s*=\s*3;/);
  assert.match(source, /const MAX_MANIFEST_BYTES:\s*usize\s*=\s*4 \* 1024 \* 1024;/);
  assert.match(
    source,
    /#\[serde\(deny_unknown_fields\)\]\s*struct ContentManifestWire/,
  );
  assert.match(
    source,
    /#\[serde\(deny_unknown_fields\)\]\s*struct ManifestEntryWire/,
  );
  assert.doesNotMatch(source, /parse_and_validate|decode_(?:legacy|compat)|migrate_manifest/);
});

test("path manifest workflows delegate to the managed codec", async () => {
  const source = await read("core/content/src/manifest.rs");
  const load = braceBlock(source, "pub fn load");
  ordered(load, [
    "read_manifest_bytes(&path)?",
    "Self::decode_managed(bytes.as_deref())",
  ]);
  assert.doesNotMatch(load, /serde_json::|ContentManifestWire|ManifestOrigin::/);

  const save = braceBlock(source, "pub(crate) fn save_with_revalidation");
  ordered(save, [
    "let body = self.encode_managed()?",
    "read_valid_manifest_snapshot(&path)?",
    "self.validate_origin(current.as_deref())?",
    "create_manifest_temp(game_dir)?",
    "file.write_all(&body)?",
    "revalidate()?",
    "read_valid_manifest_snapshot(&path)? != current",
    "promote_replacement(&temp, &path)",
    "ManifestOrigin::Present(manifest_digest(&body))",
  ]);
  assert.doesNotMatch(save, /serde_json::/);

  const snapshot = braceBlock(source, "fn read_valid_manifest_snapshot");
  ordered(snapshot, [
    "read_manifest_bytes(path)?",
    "ContentManifest::decode_managed(Some(&bytes))?",
    "Ok(Some(bytes))",
  ]);
});

test("managed codec has focused behavioral coverage", async () => {
  const source = await read("core/content/src/manifest.rs");
  for (const name of [
    "managed_codec_roundtrips_path_free_and_retains_exact_origin",
    "managed_decoder_is_strict_and_bounded_without_a_path",
    "managed_encoder_enforces_the_aggregate_serialized_bound",
    "save_then_load_roundtrips",
    "save_detects_a_manifest_change_before_final_validation",
    "save_rejects_a_manifest_changed_since_load",
    "manifest_load_requires_the_exact_v3_schema_without_rewriting_rejections",
  ]) {
    assert.match(source, new RegExp(`fn\\s+${name}\\s*\\(`));
  }
});
