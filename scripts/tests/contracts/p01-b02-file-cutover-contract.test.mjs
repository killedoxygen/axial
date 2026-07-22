import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const read = (path) => readFile(new URL(path, repository), "utf8");

async function readRustTree(...roots) {
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
  for (const root of roots) await visit(root);
  return sources;
}

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

test("file cutover deletes the raw mutation surface", async () => {
  const sources = await readRustTree("apps", "core");
  const removed = [
    "FileWriteRequest",
    "PromoteTempFileRequest",
    "DeleteFileRequest",
    "FileCapabilityReport",
    "FileCapabilityError",
    "FileCapabilityErrorKind",
    "write_file_atomically",
    "promote_temp_file",
    "delete_launcher_managed_file",
    "validate_managed_ownership",
    "io_error_fact",
    "atomic_temp_path_for",
    "replace_file_atomically",
    "MoveFileExW",
    "MOVEFILE_REPLACE_EXISTING",
    "MOVEFILE_WRITE_THROUGH",
  ];

  for (const [path, source] of sources) {
    for (const symbol of removed) {
      assert.doesNotMatch(
        source,
        new RegExp(`\\b${symbol}\\b`),
        `${path} retains raw file mutation symbol ${symbol}`,
      );
    }
  }
});

test("execution file module is fact-only and crate-private", async () => {
  const [moduleSource, fileSource] = await Promise.all([
    read("apps/api/src/execution/mod.rs"),
    read("apps/api/src/execution/file.rs"),
  ]);
  const production = fileSource.split("#[cfg(test)]")[0];

  assert.match(moduleSource, /^pub\(crate\) mod file;$/m);
  assert.match(production, /pub\(crate\) fn file_fact\s*\(/);
  assert.match(production, /TargetDescriptor::new\(/);
  assert.match(
    production,
    /EvidenceField::new\(\s*"target",[\s\S]*EvidenceSensitivity::Public/,
  );
  assert.deepEqual(
    [...production.matchAll(/(?:pub\(crate\)\s+)?fn\s+([a-z_]+)\s*\(/g)].map(
      (match) => match[1],
    ),
    ["file_fact", "safe_target_descriptor"],
  );
  assert.doesNotMatch(production, /\bpub\s+(?:struct|enum|fn)\b/);
  assert.doesNotMatch(
    production,
    /\b(?:std::|tokio::)?fs::|\basync_fs::|\bstd::(?:io|path)\b|\bPathBuf?\b|\bunsafe\b|windows_sys|MoveFileEx/,
  );
});

test("performance production persistence remains capability-owned", async () => {
  const source = await read("apps/api/src/state/performance_operations.rs");
  const persistence = braceBlock(
    source,
    "struct PerformanceOperationPersistence",
  );
  const progress = braceBlock(source, "fn accept_progress");
  const critical = braceBlock(source, "async fn commit_transition");
  const fixture = braceBlock(source, "fn write_operation_status_fixture");

  assert.match(persistence, /owner: PersistenceOwnerLease/);
  assert.match(persistence, /directory: AnchoredRecordDirectory/);
  assert.match(
    persistence,
    /writers: SyncMutex<HashMap<OperationId, AtomicSnapshotWriter>>/,
  );
  assert.match(
    source,
    /coordinator\s*\.claim_directory\(directory\.clone\(\)\)/,
  );
  assert.match(source, /self\s*\.directory\s*\.target\(/);
  assert.match(source, /self\s*\.owner\s*\.writer\(record\)/);
  assert.match(
    progress,
    /persistence\s*\.writer\(&status\.id\)\?[\s\S]*\.accept\(/,
  );
  assert.match(
    critical,
    /persistence\s*\.writer\(&status\.id\)\?[\s\S]*\.accept\(/,
  );

  const fixtureStart = source.indexOf("fn write_operation_status_fixture");
  assert.notEqual(fixtureStart, -1);
  assert.match(
    source.slice(Math.max(0, fixtureStart - 32), fixtureStart),
    /#\[cfg\(test\)\]\s*$/,
  );
  assert.match(fixture, /fs::create_dir_all\(storage_dir\)/);
  assert.match(fixture, /fs::write\(path, data\)/);
});

test("performance startup carries only capability authority", async () => {
  const [source, state] = await Promise.all([
    read("apps/api/src/state/performance_operations.rs"),
    read("apps/api/src/state/mod.rs"),
  ]);
  const retention = braceBlock(
    source,
    "pub enum PerformanceOperationRetentionIssueKind",
  );
  const startup = braceBlock(
    source,
    "pub(super) fn load_from_paths_for_startup",
  );
  const inner = braceBlock(
    source,
    "fn try_load_from_paths_with_coordinator_for_startup",
  );

  assert.doesNotMatch(retention, /\bBlockingTask\b/);
  assert.doesNotMatch(
    source,
    /PerformanceOperationRetentionIssueKind::BlockingTask/,
  );
  assert.doesNotMatch(startup, /\bAppPaths\b|\bpaths\b/);
  assert.doesNotMatch(inner, /\bAppPaths\b|\bpaths\b/);
  assert.match(startup, /directory: AnchoredRecordDirectory/);
  assert.match(inner, /directory: AnchoredRecordDirectory/);
  assert.match(
    state,
    /PerformanceOperationStore::load_from_paths_for_startup\(\s*performance_operation_directory,\s*\)/,
  );
});
