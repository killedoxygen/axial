import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const read = (path) => readFile(new URL(path, repository), "utf8");
const readJson = async (path) => JSON.parse(await read(path));

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

test("file cutover removes producerless Guardian vocabulary exactly", async () => {
  const [
    sources,
    execution,
    facts,
    model,
    rules,
    policy,
    guardianTests,
    copy,
    decisionSource,
    factIds,
    invariant,
    decisionFixture,
    journals,
    coverageDoc,
  ] = await Promise.all([
    readRustTree("apps", "core"),
    read("apps/api/src/execution/mod.rs"),
    read("apps/api/src/guardian/facts.rs"),
    read("apps/api/src/guardian/model.rs"),
    read("apps/api/src/guardian/rules.rs"),
    read("apps/api/src/guardian/policy.rs"),
    read("apps/api/src/guardian/tests.rs"),
    read("apps/api/src/guardian/copy.rs"),
    read("apps/api/src/guardian/decision_snapshot.rs"),
    readJson("apps/api/tests/fixtures/guardian/guardian-fact-ids.json"),
    readJson(
      "apps/api/tests/fixtures/guardian/guardian-invariant-coverage-v4.json",
    ),
    readJson(
      "apps/api/tests/fixtures/guardian/guardian-decision-snapshot-v1.json",
    ),
    readJson("apps/api/tests/fixtures/guardian/operation-journals-v6.json"),
    read("docs/GUARDIAN-INVARIANT-COVERAGE.md"),
  ]);
  const removedSymbols = [
    "FileLocked",
    "FileOwnershipUnknown",
    "FilePromoted",
    "FileTempLeftover",
    "FileWrittenToTemp",
    "FilesystemLocked",
    "OwnershipUnknown",
    "TempFileObserved",
  ];
  const removedWireIds = [
    "file_locked",
    "file_ownership_unknown",
    "file_promoted",
    "file_temp_leftover",
    "file_written_to_temp",
    "filesystem_locked",
    "ownership_unknown",
    "temp_file_observed",
  ];

  for (const [path, source] of sources) {
    for (const symbol of removedSymbols) {
      assert.doesNotMatch(
        source,
        new RegExp(`\\b${symbol}\\b`),
        `${path} retains producerless symbol ${symbol}`,
      );
    }
  }
  const fixtureText = JSON.stringify({
    factIds,
    invariant,
    decisionFixture,
    journals,
  });
  for (const wireId of removedWireIds) {
    assert.ok(!fixtureText.includes(wireId), `fixtures retain ${wireId}`);
  }

  assert.match(
    execution,
    /DownloadPromoted => \("download_promoted", NonFailure\)/,
  );
  assert.match(
    facts,
    /ExecutionFactKind::DownloadPromoted\s*=>\s*\(\s*GuardianFactId::AtomicPromotionCompleted/,
  );
  assert.match(
    facts,
    /ExecutionFactKind::DownloadTempWriteFailed\s*=>\s*\(\s*GuardianFactId::TempFileWriteFailed/,
  );
  assert.match(model, /pub const ALL: \[Self; 116\] = \[/);
  assert.match(model, /pub const ALL: \[Self; 76\] = \[/);
  assert.match(
    rules,
    /TempFileWriteFailed,[\s\S]*?evidence: \[TempFileWriteFailed\]/,
  );
  assert.match(rules, /ArtifactOwnershipUnsafe,\s*\[PrimitiveRefused\]/);
  assert.match(
    policy,
    /GuardianFactId::PrimitiveRefused,[\s\S]*?ArtifactOwnershipUnsafe/,
  );
  assert.match(
    guardianTests,
    /DiagnosisId::ArtifactOwnershipUnsafe,\s*&\[GuardianFactId::PrimitiveRefused\]/,
  );
  assert.match(guardianTests, /assert_eq!\(DIAGNOSIS_RULES\.len\(\), 56\)/);
  assert.match(copy, /assert_eq!\(GUARDIAN_COPY_RULES\.len\(\), 25\)/);
  assert.match(copy, /assert_eq!\(counts, \[3, 3, 13, 5, 1\]\)/);

  assert.match(decisionSource, /const FACT_SOURCE_COUNT: usize = 65;/);
  assert.match(decisionSource, /const DIAGNOSIS_COUNT: usize = 42;/);
  assert.match(decisionSource, /const FACT_SOURCE_PHASE_COUNT: usize = 252;/);
  assert.match(decisionSource, /RAW_DIAGNOSIS_CASE_COUNT, 1_272/);
  assert.match(decisionSource, /RAW_POLICY_EVALUATION_COUNT, 61_056/);
  assert.match(decisionSource, /COMPRESSED_POLICY_CELL_COUNT, 16_176/);

  assert.equal(factIds.length, 116);
  assert.equal(new Set(factIds).size, 116);
  assert.ok(factIds.includes("atomic_promotion_completed"));
  assert.ok(factIds.includes("primitive_refused"));
  assert.ok(factIds.includes("temp_file_write_failed"));
  assert.equal(invariant.rules.length, 56);
  assert.equal(invariant.facts.length, 116);
  assert.equal(invariant.adapters.execution.length, 47);
  assert.equal(
    Object.values(invariant.adapters).reduce(
      (count, rows) => count + rows.length,
      0,
    ),
    93,
  );
  assert.deepEqual(
    invariant.rules.find((row) => row.diagnosis === "artifact_ownership_unsafe")
      ?.triggers,
    ["primitive_refused"],
  );
  assert.deepEqual(
    invariant.rules.find((row) => row.diagnosis === "temp_file_write_failed")
      ?.evidence,
    ["temp_file_write_failed"],
  );
  assert.ok(
    invariant.adapters.execution.some(
      (row) =>
        row.source === "download_promoted" &&
        row.fact === "atomic_promotion_completed",
    ),
  );
  assert.equal(decisionFixture.contexts.length, 16);
  assert.equal(decisionFixture.source_cases.length, 77);
  assert.equal(decisionFixture.policy_profiles.length, 13);
  const sourceIds = decisionFixture.source_cases.map((row) => row.id);
  const factSources = decisionFixture.source_cases.filter(
    (row) => row.input.kind === "fact",
  );
  const unknownSources = decisionFixture.source_cases.filter(
    (row) => row.input.kind === "empty",
  );
  const referencedProfiles = new Set(
    decisionFixture.source_cases.flatMap((row) =>
      row.ownership_profiles.map((profile) => profile.policy_profile),
    ),
  );
  assert.deepEqual(sourceIds, sourceIds.toSorted());
  assert.equal(new Set(sourceIds).size, 77);
  assert.equal(factSources.length, 65);
  assert.equal(unknownSources.length, 12);
  assert.equal(
    factSources.reduce((count, row) => count + row.allowed_phases.length, 0),
    252,
  );
  assert.equal(new Set(factSources.map((row) => row.diagnosis.id)).size, 42);
  assert.equal(
    decisionFixture.source_cases.reduce(
      (count, row) =>
        count + row.allowed_phases.length * row.ownership_profiles.length,
      0,
    ),
    1_272,
  );
  assert.equal(1_272 * 16 * 3, 61_056);
  assert.equal((65 * 5 + 12) * 16 * 3, 16_176);
  assert.deepEqual(
    referencedProfiles,
    new Set(decisionFixture.policy_profiles.map((profile) => profile.id)),
  );
  const diagnosisIds = journals.entries
    .slice(0, 3)
    .flatMap((entry) => entry.guardian_diagnosis_ids);
  assert.equal(diagnosisIds.length, 76);
  assert.equal(new Set(diagnosisIds).size, 76);
  assert.match(coverageDoc, /\| Diagnosis rules \| 56 \|/);
  assert.match(coverageDoc, /\| Registered facts \| 116 \|/);
  assert.match(coverageDoc, /\| Adapter sources \| 93 \|/);
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
