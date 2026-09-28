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

function registryWireIds(source, marker) {
  return [...braceBlock(source, marker).matchAll(/=>\s*"([^"]+)"/g)].map(
    (match) => match[1],
  );
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
    invariant,
    decisionFixture,
    journals,
    fixtureNames,
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
    readJson(
      "apps/api/tests/fixtures/guardian/guardian-invariant-coverage-v5.json",
    ),
    readJson(
      "apps/api/tests/fixtures/guardian/guardian-decision-snapshot-v1.json",
    ),
    readJson("apps/api/tests/fixtures/guardian/operation-journals-v10.json"),
    readdir(new URL("apps/api/tests/fixtures/guardian/", repository)),
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
  assert.match(
    execution,
    /DownloadTempDiscarded => \("download_temp_discarded", NonFailure\)/,
  );
  assert.match(
    facts,
    /ExecutionFactKind::DownloadTempDiscarded\s*=>\s*\(\s*GuardianFactId::DownloadTempDiscarded/,
  );
  assert.match(model, /macro_rules! stable_phase_id_registry/);
  assert.match(
    model,
    /stable_phase_id_registry!\s*\{\s*"unknown Guardian fact id";\s*117;\s*pub enum GuardianFactId/,
  );
  assert.match(
    model,
    /stable_phase_id_registry!\s*\{\s*"unknown Guardian diagnosis id";\s*76;\s*pub enum DiagnosisId/,
  );
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

  const factIds = registryWireIds(model, "pub enum GuardianFactId");
  const diagnosisIds = registryWireIds(model, "pub enum DiagnosisId");
  assert.equal(factIds.length, 117);
  assert.equal(new Set(factIds).size, 117);
  assert.ok(factIds.includes("atomic_promotion_completed"));
  assert.ok(factIds.includes("download_temp_discarded"));
  assert.ok(factIds.includes("primitive_refused"));
  assert.ok(factIds.includes("temp_file_write_failed"));
  assert.equal(diagnosisIds.length, 76);
  assert.equal(new Set(diagnosisIds).size, 76);
  assert.equal(invariant.rules.length, 56);
  assert.equal(invariant.facts.length, 117);
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
  assert.ok(
    invariant.adapters.execution.some(
      (row) =>
        row.source === "download_temp_discarded" &&
        row.fact === "download_temp_discarded",
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
  assert.equal(journals.schema, "axial.state.operation_journals.v10");
  assert.equal(journals.next_sequence, 8);
  assert.deepEqual(
    journals.entries.map((entry) => entry.sequence),
    [1, 2, 4, 5, 7],
  );
  const tier2Step = journals.entries
    .flatMap((entry) => entry.completed_steps)
    .find((step) => step.step_id === "tier2_integrity_sweep");
  assert.equal(tier2Step?.metrics?.kind, "tier2_integrity");
  assert.deepEqual(tier2Step?.guardian_fact_ids, [
    "artifact_checksum_mismatch",
  ]);
  const install = journals.entries.find(
    (entry) => entry.command === "InstallVersion",
  );
  assert.equal(install?.completed_steps[0]?.metrics?.kind, "content_download");
  assert.deepEqual(install?.completed_steps[0]?.guardian_fact_ids, [
    "download_provider_unavailable",
  ]);
  assert.equal(
    install?.guardian_install_terminal?.diagnosis_id,
    "download_unavailable",
  );
  assert.equal(install?.guardian_install_terminal?.action, "Retry");
  assert.ok(install?.guardian_install_terminal?.memory);
  assert.deepEqual(
    journals.entries
      .filter((entry) => entry.intent.kind === "performance")
      .map((entry) => [entry.command, entry.intent.phase.phase]),
    [["ApplyPerformancePlan", "accepted"]],
  );
  assert.deepEqual(
    fixtureNames
      .filter((name) => name.startsWith("operation-journals-v"))
      .toSorted(),
    ["operation-journals-v10.json"],
  );
  assert.ok(!fixtureNames.includes("guardian-fact-ids.json"));
  for (const prefix of [
    "integrity_counter:",
    "execution_download_fact:",
    "guardian_fact:",
    "guardian_outcome_",
  ]) {
    assert.ok(!JSON.stringify(journals).includes(prefix));
  }
  assert.match(coverageDoc, /\| Diagnosis rules \| 56 \|/);
  assert.match(coverageDoc, /\| Registered facts \| 117 \|/);
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

test("Guardian evidence is operation-bound, typed, and string-free", async () => {
  const [sources, model, facts, contracts] = await Promise.all([
    readRustTree("apps", "core"),
    read("apps/api/src/guardian/model.rs"),
    read("apps/api/src/guardian/facts.rs"),
    read("apps/api/src/state/contracts.rs"),
  ]);

  assert.match(model, /pub enum EvidenceScope\s*\{/);
  assert.match(facts, /pub struct OperationEvidenceBatch\s*\{/);
  assert.match(model, /pub const MAX_OPERATION_EVIDENCE_FACTS: usize = 64;/);
  assert.match(
    model,
    /pub const MAX_OPERATION_EVIDENCE_SERIALIZED_BYTES: usize = 131_072;/,
  );
  assert.match(facts, /pub fn try_from_execution_operation\s*\(/);
  assert.match(facts, /pub fn try_from_guardian_operation\s*\(/);
  assert.match(facts, /OperationEvidenceBatchRejection::ForeignOperation/);
  assert.match(contracts, /guardian_fact_ids:\s*Vec<GuardianFactId>/);
  assert.match(contracts, /metrics:\s*Option<OperationStepMetrics>/);
  assert.match(
    contracts,
    /guardian_install_terminal:\s*Option<GuardianInstallTerminalEvidence>/,
  );

  const reservedJournalString =
    /"(?:integrity_counter:|execution_download_fact:|guardian_fact:|guardian_outcome_)/;
  for (const [path, source] of sources) {
    const production = source.split("#[cfg(test)]")[0];
    assert.doesNotMatch(
      production,
      reservedJournalString,
      `${path} retains a reserved journal string carrier`,
    );
  }
});

test("performance production persistence has one capability-owned journal", async () => {
  const [journals, state, stateEntries] = await Promise.all([
    read("apps/api/src/state/journals.rs"),
    read("apps/api/src/state/mod.rs"),
    readdir(new URL("apps/api/src/state/", repository)),
  ]);
  const persistence = braceBlock(
    journals,
    "struct OperationJournalPersistence",
  );
  const store = braceBlock(journals, "pub struct OperationJournalStore");

  assert.deepEqual(stateEntries.includes("performance_operations.rs"), false);
  assert.match(persistence, /owner: PersistenceOwnerLease/);
  assert.match(persistence, /writer: AtomicSnapshotWriter/);
  assert.match(store, /persistence: Option<OperationJournalPersistence>/);
  assert.match(
    state,
    /OperationJournalStore::try_load_from_directory_with_temporal\([\s\S]*operation_journal_parent\(\)/,
  );
  assert.doesNotMatch(
    state,
    /PerformanceOperationStore|performance_operation_directory/,
  );
  assert.match(
    journals,
    /pub const OPERATION_JOURNAL_SCHEMA: &str = "axial\.state\.operation_journals\.v10"/,
  );
  assert.match(
    journals,
    /previous_operation_journal_schema_is_strict_invalid_and_preserved_byte_exact[\s\S]*?"axial\.state\.operation_journals\.v10"[\s\S]*?"axial\.state\.operation_journals\.v9"/,
  );
});
