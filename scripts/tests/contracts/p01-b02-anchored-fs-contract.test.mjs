import assert from "node:assert/strict";
import { access, readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repository = fileURLToPath(new URL("../../../", import.meta.url));
const contractPhase = process.env.P01_B02_CONTRACT_PHASE ?? "terminal";
const terminalTest = contractPhase === "terminal" ? test : test.skip;

const read = (path) => readFile(join(repository, path), "utf8");

const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

const exists = async (path) => {
  try {
    await access(join(repository, path));
    return true;
  } catch {
    return false;
  }
};

const readRustTree = async (...roots) => {
  const sources = [];
  const visit = async (relative) => {
    for (const entry of await readdir(join(repository, relative), {
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
};

const between = (source, start, end) => {
  const first = source.indexOf(start);
  const last = source.indexOf(end, first + start.length);
  assert.notEqual(first, -1, `missing section start: ${start}`);
  assert.notEqual(last, -1, `missing section end: ${end}`);
  return source.slice(first, last);
};

const functionBlock = (source, name) => {
  const marker = new RegExp(
    `(?:pub(?:\\([^)]*\\))?\\s+)?(?:async\\s+)?(?:unsafe\\s+)?fn\\s+${escapeRegExp(name)}(?:<[^>{}]+>)?\\s*\\(`,
  );
  const match = marker.exec(source);
  assert.ok(match, `missing function ${name}`);
  const openingBrace = source.indexOf("{", match.index + match[0].length);
  assert.notEqual(openingBrace, -1, `missing body for ${name}`);
  let depth = 0;
  for (let offset = openingBrace; offset < source.length; offset += 1) {
    if (source[offset] === "{") depth += 1;
    if (source[offset] === "}") depth -= 1;
    if (depth === 0) return source.slice(match.index, offset + 1);
  }
  assert.fail(`unterminated body for ${name}`);
};

const functionBlocks = (source) => {
  const blocks = [];
  const marker =
    /(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([a-zA-Z0-9_]+)(?:<[^>{}]+>)?\s*\(/g;
  for (let match = marker.exec(source); match; match = marker.exec(source)) {
    const openingBrace = source.indexOf("{", match.index + match[0].length);
    if (openingBrace === -1) continue;
    let depth = 0;
    for (let offset = openingBrace; offset < source.length; offset += 1) {
      if (source[offset] === "{") depth += 1;
      if (source[offset] === "}") depth -= 1;
      if (depth === 0) {
        blocks.push({
          name: match[1],
          source: source.slice(match.index, offset + 1),
        });
        marker.lastIndex = offset + 1;
        break;
      }
    }
  }
  return blocks;
};

const uniqueReachableFunctions = (source, seed) => {
  const blocks = functionBlocks(source);
  const byName = new Map();
  for (const block of blocks) {
    const matches = byName.get(block.name) ?? [];
    matches.push(block.source);
    byName.set(block.name, matches);
  }

  const reached = [seed];
  const visited = new Set();
  for (let index = 0; index < reached.length; index += 1) {
    const body = reached[index];
    for (const [name, matches] of byName) {
      if (
        matches.length !== 1 ||
        visited.has(name) ||
        !new RegExp(`\\b${escapeRegExp(name)}\\s*\\(`).test(body)
      ) {
        continue;
      }
      visited.add(name);
      reached.push(matches[0]);
    }
  }
  return reached.join("\n");
};

const itemBlock = (source, kind, name) => {
  const marker = new RegExp(
    `(?:pub\\s+)?${kind}\\s+${name}(?:<[^>{}]+>)?\\s*\\{`,
  );
  const match = marker.exec(source);
  assert.ok(match, `missing ${kind} ${name}`);
  const openingBrace = source.indexOf("{", match.index);
  let depth = 0;
  for (let offset = openingBrace; offset < source.length; offset += 1) {
    if (source[offset] === "{") depth += 1;
    if (source[offset] === "}") depth -= 1;
    if (depth === 0) return source.slice(match.index, offset + 1);
  }
  assert.fail(`unterminated ${kind} ${name}`);
};

const implementationBlock = (source, name) => {
  const marker = new RegExp(`impl\\s+${name}(?:<[^>{}]+>)?\\s*\\{`);
  const match = marker.exec(source);
  assert.ok(match, `missing impl ${name}`);
  const openingBrace = source.indexOf("{", match.index);
  let depth = 0;
  for (let offset = openingBrace; offset < source.length; offset += 1) {
    if (source[offset] === "{") depth += 1;
    if (source[offset] === "}") depth -= 1;
    if (depth === 0) return source.slice(match.index, offset + 1);
  }
  assert.fail(`unterminated impl ${name}`);
};

const assertOrdered = (source, before, after, label) => {
  const beforeIndex = source.indexOf(before);
  const afterIndex = source.indexOf(after);
  assert.notEqual(beforeIndex, -1, `missing ${label} start: ${before}`);
  assert.notEqual(afterIndex, -1, `missing ${label} end: ${after}`);
  assert.ok(beforeIndex < afterIndex, `${label} is out of order`);
};

const assertCountAtLeast = (source, expression, count, label) => {
  const matches = source.match(
    new RegExp(expression.source, expression.flags + "g"),
  );
  assert.ok((matches?.length ?? 0) >= count, label);
};

const assertLinear = (source, type) => {
  const declaration = new RegExp(
    `((?:#\\[[^\\]]*\\]\\s*)*)(?:pub(?:\\([^)]*\\))?\\s+)?struct\\s+${escapeRegExp(type)}\\b`,
  ).exec(source);
  assert.ok(declaration, `missing linear type ${type}`);
  assert.doesNotMatch(
    declaration[1],
    /#\[derive\([^\]]*\b(?:Clone|Copy|Serialize|Deserialize)\b[^\]]*\)\]/,
    `${type} must remain linear and process-local`,
  );
  assert.doesNotMatch(source, new RegExp(`impl Clone for ${type}\\b`));
  assert.doesNotMatch(
    source,
    new RegExp(`impl (?:Serialize|Deserialize) for ${type}\\b`),
  );
};

const traitImplementationBlock = (source, trait, name) => {
  const marker = new RegExp(
    `impl\\s+${escapeRegExp(trait)}\\s+for\\s+${escapeRegExp(name)}\\s*\\{`,
  );
  const match = marker.exec(source);
  assert.ok(match, `missing impl ${trait} for ${name}`);
  const openingBrace = source.indexOf("{", match.index);
  let depth = 0;
  for (let offset = openingBrace; offset < source.length; offset += 1) {
    if (source[offset] === "{") depth += 1;
    if (source[offset] === "}") depth -= 1;
    if (depth === 0) return source.slice(match.index, offset + 1);
  }
  assert.fail(`unterminated impl ${trait} for ${name}`);
};

const terminalDrainContract = (
  library,
  start,
  terminalExpression,
  label,
  successExpression = /\bReady\s*(?:\(|\{)/,
) => {
  const header = start.slice(0, start.indexOf("{"));
  const outcomeName = header.match(
    /->\s*(?:(?:io::)?Result\s*<\s*)?([A-Za-z0-9_]+(?:Outcome|Start|Drain))\b/,
  )?.[1];
  assert.ok(outcomeName, `${label} must return an explicit drain outcome`);
  const outcome = itemBlock(library, "enum", outcomeName);
  assert.equal(
    outcome.match(new RegExp(successExpression.source, "g"))?.length ?? 0,
    1,
    `${label} needs exactly one typed terminal-success variant`,
  );
  assert.equal(
    outcome.match(/\bPending\s*(?:\(|\{)/g)?.length ?? 0,
    1,
    `${label} needs exactly one pending-drain variant`,
  );
  const pendingName = outcome.match(
    /\bPending\s*(?:\(\s*|\{[\s\S]{0,200}?\b(?:drain|pending|authority):\s*)([A-Za-z0-9_]+)\b/,
  )?.[1];
  assert.ok(pendingName, `${label} needs a retained Pending drain authority`);
  assertLinear(library, pendingName);
  const pending = itemBlock(library, "struct", pendingName);
  assert.match(
    pending,
    /(?:RootSession|Arc<CapabilityAuthority>)/,
    `${label} pending authority must retain the root session`,
  );

  const startFlow = uniqueReachableFunctions(library, start);
  assert.doesNotMatch(startFlow, /\.wait(?:_while)?\(/);
  const drainingMarker = startFlow.match(
    /AUTHORITY_DRAINING|\bDraining\b/,
  )?.[0];
  assert.ok(drainingMarker, `${label} must enter DRAINING`);
  assertOrdered(
    startFlow,
    ".lock()",
    drainingMarker,
    `${label} gate lock before DRAINING`,
  );
  assert.match(
    startFlow,
    /\bPending\s*(?:\(|\{)|\.try_settle\s*\(/,
    `${label} must return or immediately probe a pending drain`,
  );

  const settlementDirect = functionBlock(
    implementationBlock(library, pendingName),
    "try_settle",
  );
  assert.match(
    settlementDirect.slice(0, settlementDirect.indexOf("{")),
    /try_settle\((?:mut )?self\)/,
    `${label} settlement must consume its linear authority`,
  );
  const settlement = uniqueReachableFunctions(library, settlementDirect);
  assert.doesNotMatch(settlement, /\.wait(?:_while)?\(/);
  const settleActiveNonzero = settlement.match(
    /\b(?:active|in_flight|operations)\b[\s\S]{0,160}?(?:!=|>)\s*0/,
  )?.[0];
  assert.ok(
    settleActiveNonzero,
    `${label} settlement must retry while active permits remain`,
  );
  assert.match(settlement, /\bPending\s*(?:\(|\{)/);
  assert.match(settlement, successExpression);
  assert.match(settlementDirect, terminalExpression);
  const successMarker = settlementDirect.match(successExpression)?.[0];
  assert.ok(successMarker, `${label} settlement must return terminal success`);
  const delegatedSettlement = settlementDirect.match(
    /\b[a-z_]*(?:finish|settle)[a-z_]*terminal[a-z_]*drain[a-z_]*\s*\(/,
  )?.[0];
  if (delegatedSettlement) {
    assertOrdered(
      settlementDirect,
      delegatedSettlement,
      successMarker,
      `${label} drain settlement before terminal success`,
    );
  }
  const terminalMarker = settlement.match(
    /\b(?:[a-z_]+\.)?(?:phase|state)\s*=\s*(?:terminal_phase|AUTHORITY_(?:RESETTING|REVOKED)|Resetting|Revoked)\b/,
  )?.[0];
  assert.ok(
    terminalMarker,
    `${label} settlement needs its terminal transition`,
  );
  const unresolvedStages = settlement.match(
    /\b(?:stages|staged_files)\b[\s\S]{0,80}?\.is_empty\(\)/,
  )?.[0];
  assert.ok(
    unresolvedStages,
    `${label} terminal success must prove the stage registry empty`,
  );
  const unresolvedStageCreates = settlement.match(
    /\b(?:stage_creations|stage_create_records|pending_stage_creates)\b[\s\S]{0,80}?\.is_empty\(\)/,
  )?.[0];
  assert.ok(
    unresolvedStageCreates,
    `${label} terminal success must prove the pre-effect stage-create registry empty`,
  );
  const unresolvedDirectoryCreates = settlement.match(
    /\b(?:directory_creations|directory_create_records|pending_directory_creates)\b[\s\S]{0,80}?\.is_empty\(\)/,
  )?.[0];
  assert.ok(
    unresolvedDirectoryCreates,
    `${label} terminal success must prove the pre-effect directory-create registry empty`,
  );
  assertOrdered(
    settlement,
    settleActiveNonzero,
    unresolvedStages,
    `${label} active-permit drain before stage settlement`,
  );
  assertOrdered(
    settlement,
    settleActiveNonzero,
    unresolvedStageCreates,
    `${label} active-permit drain before stage-create settlement`,
  );
  assertOrdered(
    settlement,
    settleActiveNonzero,
    unresolvedDirectoryCreates,
    `${label} active-permit drain before directory-create settlement`,
  );
  assertOrdered(
    settlement,
    unresolvedStages,
    terminalMarker,
    `${label} stage settlement before terminal transition`,
  );
  assertOrdered(
    settlement,
    unresolvedStageCreates,
    terminalMarker,
    `${label} stage-create settlement before terminal transition`,
  );
  assertOrdered(
    settlement,
    unresolvedDirectoryCreates,
    terminalMarker,
    `${label} directory-create settlement before terminal transition`,
  );

  const refusalName = outcome.match(
    /\bRefused\s*(?:\(\s*|\{[\s\S]{0,200}?\b(?:failure|refusal):\s*)([A-Za-z0-9_]+)\b/,
  )?.[1];
  assert.ok(refusalName, `${label} needs a typed retained refusal`);
  assertLinear(library, refusalName);
  const refusal = itemBlock(library, "struct", refusalName);
  assert.match(
    refusal,
    /(?:RootSession|Arc<CapabilityAuthority>)/,
    `${label} refusal must retain the sole session and cleanup registries`,
  );
  assert.match(
    implementationBlock(library, refusalName),
    /pub fn (?:retry|try_settle)\((?:mut )?self\)/,
    `${label} refusal must expose consuming retry`,
  );

  return {
    outcomeName,
    pendingName,
    startFlow,
    settlementDirect,
    settlement,
    drainingMarker,
    settleActiveNonzero,
    terminalMarker,
  };
};

const assertAbsent = (sources, expressions) => {
  for (const [path, source] of sources) {
    for (const expression of expressions) {
      assert.doesNotMatch(source, expression, `${path} retains ${expression}`);
    }
  }
};

test("P01-B02 contract mode is explicit and terminal by default", () => {
  assert.ok(
    contractPhase === "migration" || contractPhase === "terminal",
    "P01_B02_CONTRACT_PHASE must be migration or terminal",
  );
});

test("P01-B02 has one dependency-bottom physical capability owner", async () => {
  const [workspace, manifest, library] = await Promise.all([
    read("Cargo.toml"),
    read("core/fs/Cargo.toml"),
    read("core/fs/src/lib.rs"),
  ]);

  assert.match(workspace, /^\s*"core\/fs",$/m);
  assert.match(workspace, /^axial-fs = \{ path = "core\/fs" \}$/m);
  assert.match(manifest, /^name = "axial-fs"$/m);
  const dependencies = manifest.slice(manifest.indexOf("[dependencies]"));
  assert.doesNotMatch(dependencies, /^axial-[a-z0-9-]+\s*=/m);
  assert.doesNotMatch(dependencies, /path\s*=\s*"\.\.\//);

  for (const type of [
    "LeafName",
    "DirectoryIdentity",
    "DirectoryEntry",
    "Directory",
    "FileCapability",
    "StagedFile",
    "RootSession",
    "DirectoryCreateOutcome",
    "FileCreateOutcome",
    "FilePromotionOutcome",
    "FileRemovalOutcome",
    "DirectoryRemovalOutcome",
  ]) {
    assert.match(library, new RegExp(`pub (?:struct|enum) ${type}\\b`));
  }
  const identityStart = library.indexOf("pub struct DirectoryIdentity");
  const identityEnd = library.indexOf("\n}", identityStart);
  assert.notEqual(identityStart, -1);
  assert.notEqual(identityEnd, -1);
  const identity = library.slice(identityStart, identityEnd + 2);
  assert.match(
    identity,
    /(?:session|generation|nonce):\s*(?:\[u8;\s*\d+\]|[A-Za-z0-9_]*Session[A-Za-z0-9_]*)/,
    "native identity equality must remain scoped to one live root session",
  );
  assert.match(identity, /platform::Identity/);
  assert.doesNotMatch(identity, /\n\s*pub\s+[a-z_]+:/);
  assert.doesNotMatch(
    library.slice(Math.max(0, identityStart - 180), identityStart),
    /Serialize|Deserialize/,
  );
  assert.doesNotMatch(
    library,
    /impl (?:Serialize|Deserialize) for DirectoryIdentity/,
  );
  assert.match(library, /const MAX_LEAF_UNITS: usize = 255;/);
  assert.match(library, /\.is_empty\(\)/);
  assert.match(library, /value == OsStr::new\("\."\)/);
  assert.match(library, /value == OsStr::new\("\.\."\)/);
  assert.match(
    library,
    /bytes\.iter\(\)\.any\(\|byte\| matches!\(byte, 0 \| b'\/'\)\)/,
  );
  assert.match(library, /matches!\(\*unit, 0 \| 0x2f \| 0x3a \| 0x5c\)/);
  assert.match(library, /pub fn entries\(&self, limit: usize\)/);
  assert.match(library, /parent: DirectoryIdentity/);
  assert.doesNotMatch(library, /pub enum MutationOutcome/);
  assert.doesNotMatch(library, /pub fn (?:path|into_path|as_path)\s*\(/);
  assert.doesNotMatch(library, /PathBuf/);
});

test("P01-B02 mutation outcomes retain distinct gated obligations", async () => {
  const library = await read("core/fs/src/lib.rs");
  const outcomes = [
    ["DirectoryCreateOutcome", "Created"],
    ["FileCreateOutcome", "Created"],
    ["FilePromotionOutcome", "Applied"],
    ["FileParkOutcome", "Parked"],
    ["FileRemovalOutcome", "Removed"],
    ["FileRestoreOutcome", "Restored"],
    ["DirectoryParkOutcome", "Parked"],
    ["DirectoryRemovalOutcome", "Removed"],
    ["DirectoryRestoreOutcome", "Restored"],
    ["FileReplaceOutcome", "Replaced"],
  ];
  const obligations = new Map();

  for (const [outcomeName, appliedVariant] of outcomes) {
    const outcome = itemBlock(library, "enum", outcomeName);
    assert.match(outcome, new RegExp(`\\b${appliedVariant}\\b`));
    assert.match(outcome, /\bNoEffect\b/);
    const obligation = outcome.match(
      /AppliedUnverified(?:\(\s*|\s*\{[\s\S]{0,320}?obligation:\s*)([A-Za-z0-9_]+Obligation)\b/,
    )?.[1];
    assert.ok(
      obligation,
      `${outcomeName} must retain its applied-unverified obligation`,
    );
    assert.equal(
      obligations.has(obligation),
      false,
      `${outcomeName} reuses ${obligation} instead of an operation-specific obligation`,
    );
    obligations.set(obligation, outcomeName);
  }

  for (const [obligation, outcome] of obligations) {
    assertLinear(library, obligation);
    const implementation = implementationBlock(library, obligation);
    assert.match(
      implementation,
      /pub fn (?:reconcile|settle)\((?:mut )?self\)/,
      `${outcome} settlement must consume ${obligation}`,
    );
    const settlement = uniqueReachableFunctions(library, implementation);
    const admissionName = settlement.match(/\.([a-z_]*enter[a-z_]*)\s*\(/)?.[1];
    assert.ok(
      admissionName,
      `${obligation} settlement must enter an operation gate`,
    );
    if (admissionName !== "enter") {
      const admission = functionBlock(library, admissionName);
      assert.match(admission, /AUTHORITY_LIVE|\bLive\b/);
      assert.doesNotMatch(
        admission,
        /AUTHORITY_DRAINING|\bDraining\b/,
        `${obligation} normal settlement cannot borrow drain-recovery authority`,
      );
    }
  }
});

test("P01-B02 reserves create effects before native namespace mutation", async () => {
  const library = await read("core/fs/src/lib.rs");
  const authority = implementationBlock(library, "CapabilityAuthority");

  for (const {
    label,
    outcome,
    obligation,
    nativeEffect,
    recordPattern,
    tokenPattern,
    retainedAuthority,
  } of [
    {
      label: "staged-file create",
      outcome: "FileCreateOutcome",
      obligation: "FileCreateObligation",
      nativeEffect: "create_file",
      recordPattern:
        /struct ([A-Za-z0-9_]*StageCreate[A-Za-z0-9_]*(?:Record|Reservation)[A-Za-z0-9_]*)\s*\{/,
      tokenPattern:
        /struct ([A-Za-z0-9_]*StageCreate[A-Za-z0-9_]*(?:Token|Reservation)[A-Za-z0-9_]*)\s*\{/,
      retainedAuthority:
        /(?:created|retained|cleanup|handle):\s*Option<(?:File|platform::FileCleanupHandle)>/,
    },
    {
      label: "directory create",
      outcome: "DirectoryCreateOutcome",
      obligation: "DirectoryCreateObligation",
      nativeEffect: "create_directory",
      recordPattern:
        /struct ([A-Za-z0-9_]*DirectoryCreate[A-Za-z0-9_]*(?:Record|Reservation)[A-Za-z0-9_]*)\s*\{/,
      tokenPattern:
        /struct ([A-Za-z0-9_]*DirectoryCreate[A-Za-z0-9_]*(?:Token|Reservation)[A-Za-z0-9_]*)\s*\{/,
      retainedAuthority:
        /(?:created|retained|cleanup|handle):\s*Option<platform::(?:DirectoryCleanupHandle|DirectoryHandle)>/,
    },
  ]) {
    const recordName = library.match(recordPattern)?.[1];
    const tokenName = library.match(tokenPattern)?.[1];
    assert.ok(
      recordName && tokenName,
      `${label} needs typed reservation state`,
    );
    const record = itemBlock(library, "struct", recordName);
    assert.match(record, /parent:\s*Directory/);
    assert.match(record, /name:\s*LeafName/);
    assert.match(record, retainedAuthority);
    assert.match(record, /(?:phase|state):/);
    const token = itemBlock(library, "struct", tokenName);
    assert.match(token, /Weak<CapabilityAuthority>/);
    assert.match(
      itemBlock(library, "struct", obligation),
      new RegExp(`\\b${escapeRegExp(tokenName)}\\b`),
      `${label} ambiguity must retain its pre-effect reservation`,
    );

    const operation = functionBlocks(library).find(
      ({ source }) =>
        new RegExp(`->\\s*${outcome}\\b`).test(
          source.slice(0, source.indexOf("{")),
        ) && new RegExp(`platform::${nativeEffect}\\s*\\(`).test(source),
    );
    assert.ok(operation, `missing ${label} operation`);
    const permit = operation.source.match(/\.enter\s*\(\)/)?.[0];
    const reservationCall = operation.source.match(
      /\.([a-z_]*(?:reserve|register)[a-z_]*(?:create|stage|directory)[a-z_]*)\s*\(/,
    );
    const effect = operation.source.match(
      new RegExp(`platform::${nativeEffect}\\s*\\(`),
    )?.[0];
    const attach = operation.source.match(
      /\.(?:attach|finalize|record|commit|mark_applied)[a-z_]*(?:create)?[a-z_]*\s*\(/,
    )?.[0];
    assert.ok(
      permit && reservationCall && effect && attach,
      `${label} needs permit, reservation, native effect, and retained-effect attachment`,
    );
    assertOrdered(
      operation.source,
      permit,
      reservationCall[0],
      `${label} permit before reservation`,
    );
    assertOrdered(
      operation.source,
      reservationCall[0],
      effect,
      `${label} reservation before namespace effect`,
    );
    assertOrdered(
      operation.source,
      effect,
      attach,
      `${label} namespace effect before exact authority attachment`,
    );
    const reserve = functionBlock(authority, reservationCall[1]);
    assert.match(
      reserve,
      new RegExp(`\\b${escapeRegExp(recordName)}\\b`),
      `${label} reservation call must install its typed record`,
    );
  }

  const reconcileDirectory = functionBlock(
    implementationBlock(library, "DirectoryCreateObligation"),
    "reconcile",
  );
  const finishDirectoryCreate = reconcileDirectory.match(
    /\b([a-z_]*(?:finish|settle)[a-z_]*directory_create[a-z_]*)\s*\(/,
  )?.[1];
  assert.ok(
    finishDirectoryCreate,
    "directory-create reconciliation needs a typed settlement helper",
  );
  const directorySettlement = functionBlock(library, finishDirectoryCreate);
  assertOrdered(
    directorySettlement,
    "platform::open_directory",
    "Ok(directory)",
    "directory-create reconciliation must downgrade cleanup authority before returning",
  );
});

test("P01-B02 remains session-local and does not absorb B03 durability", async () => {
  const [manifest, library, platform] = await Promise.all([
    read("core/fs/Cargo.toml"),
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  assert.doesNotMatch(manifest, /^serde(?:_json)?\s*=/m);
  assert.doesNotMatch(
    `${library}\n${platform}`,
    /\bSerialize\b|\bDeserialize\b|persistent_(?:binding|identity)|\b(?:StageJournal|PersistedStage|DurableReceipt|StartupStageRecovery|PidStage|StagePid)\b|std::process::id\(|process::id\(/,
    "axial-fs must not persist native identity, stage state, PID sweep authority, or restart truth",
  );
  assert.doesNotMatch(
    `${library}\n${platform}`,
    /fn [a-z_]*(?:startup|restart)[a-z_]*(?:sweep|recover|cleanup)[a-z_]*\s*\(|fn [a-z_]*(?:sweep|recover)[a-z_]*(?:stage|temp|pid)[a-z_]*\s*\(/,
    "B03 owns startup recovery and durable staged-object cleanup",
  );
});

test("P01-B02 root acquisition retains exact partial-effect obligations", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);

  assert.equal(
    /\b(?:created_any|did_create|partial_effect|effect_applied)\s*:\s*bool\b/.test(
      platform,
    ),
    false,
    "a boolean cannot retain the exact root bindings created before failure",
  );

  const outcomeName = library.match(
    /pub enum (Root[A-Za-z0-9_]*(?:Acquire|Acquisition|Open|Session)[A-Za-z0-9_]*Outcome)\s*\{/,
  )?.[1];
  assert.ok(outcomeName, "root admission needs an operation-specific outcome");
  const outcome = itemBlock(library, "enum", outcomeName);
  assert.match(outcome, /\bAcquired\b/);
  assert.match(outcome, /\bNoEffect\b/);
  const obligationName = outcome.match(
    /AppliedUnverified(?:\(\s*|\s*\{[\s\S]{0,240}?obligation:\s*)(Root[A-Za-z0-9_]*Obligation)\b/,
  )?.[1];
  assert.ok(
    obligationName,
    "partial root admission must return a root-specific AppliedUnverified obligation",
  );

  const obligation = itemBlock(library, "struct", obligationName);
  assert.match(
    obligation,
    /error:\s*(?:(?:io::)?Error|Root[A-Za-z0-9_]*Error)/,
  );
  const constructionName = obligation.match(
    /(?:root|construction|creation|bindings)[a-z_]*:\s*(?:Option<)?platform::(Root[A-Za-z0-9_]*(?:Guard|Construction|Creation|Obligation))>?/,
  )?.[1];
  assert.ok(
    constructionName,
    "the obligation must retain the walked root and its exact created bindings",
  );
  assert.match(
    obligation,
    /lease[a-z_]*:\s*(?:Option<)?platform::[A-Za-z0-9_]*Lease[A-Za-z0-9_]*(?:Handle|Binding|Acquisition|Obligation)/,
    "the obligation must retain an exact lease handle or lease acquisition obligation",
  );

  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const construction = itemBlock(source, "struct", constructionName);
    const createdBindingName = construction.match(
      /Vec<(Root[A-Za-z0-9_]*(?:Created|Creation)[A-Za-z0-9_]*Binding|CreatedRoot[A-Za-z0-9_]*Binding)>/,
    )?.[1];
    assert.ok(
      createdBindingName,
      `${platformName} root construction must directly own its created-binding chain`,
    );
    const createdBinding = itemBlock(source, "struct", createdBindingName);
    assert.match(createdBinding, /parent:\s*(?:Option<)?DirectoryHandle/);
    assert.match(createdBinding, /name:\s*(?:OsString|LeafName)/);
    assert.match(createdBinding, /identity:\s*Identity/);
    assert.match(
      createdBinding,
      /(?:child|handle):\s*(?:Option<)?DirectoryHandle/,
      `${platformName} created-root cleanup needs the retained child handle`,
    );
  }

  const implementation = implementationBlock(library, obligationName);
  for (const operation of ["reconcile", "cleanup"]) {
    assert.match(
      implementation,
      new RegExp(`pub fn ${operation}\\((?:mut )?self(?:[,)]|\\s*\\))`),
      `${obligationName}::${operation} must consume the obligation`,
    );
  }
  assert.doesNotMatch(
    implementation,
    /pub fn (?:release|abandon)\((?:mut )?self/,
    "root partial effects cannot be silently released without returning ownership",
  );
  const acquire = functionBlock(
    implementationBlock(library, "RootSession"),
    "acquire",
  );
  assert.match(acquire.split("{")[0], new RegExp(`\\b${outcomeName}\\b`));
  assertOrdered(
    acquire,
    "open_or_create_root",
    "try_acquire_lease",
    "root creation before lease acquisition",
  );
  const leaseFailure = acquire.slice(acquire.indexOf("try_acquire_lease"));
  assert.match(
    leaseFailure,
    new RegExp(`${outcomeName}::AppliedUnverified|${obligationName}`),
    "lease failure after root creation must preserve the root acquisition obligation",
  );
});

test("P01-B02 retains physical startup process-image ancestry for reset", async () => {
  const [library, platform, bootstrap] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
    read("apps/api/src/bootstrap.rs"),
  ]);
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );

  const authority = itemBlock(library, "struct", "CapabilityAuthority");
  assert.match(
    authority,
    /(?:executable|process_image|image_ancestry)[a-z_]*:\s*platform::[A-Za-z0-9_]*(?:Executable|ProcessImage|ImageAncestry)[A-Za-z0-9_]*/i,
    "the root authority must retain startup process-image ancestry",
  );
  const rootSession = implementationBlock(library, "RootSession");
  const acquire = functionBlock(rootSession, "acquire");
  const startupCapture = `${bootstrap}\n${uniqueReachableFunctions(library, acquire)}`;
  assert.match(startupCapture, /current_exe\(/);
  assert.match(
    startupCapture,
    /(?:capture|open|admit|retain)[a-z_]*(?:executable|process_image|image_ancestry)|(?:executable|process_image|image_ancestry)[a-z_]*(?:capture|open|admit|retain)/i,
  );
  const captureCall = acquire.match(
    /\b([a-z_]*(?:capture|open|admit|retain)[a-z_]*(?:executable|process_image|image_ancestry)[a-z_]*|[a-z_]*(?:executable|process_image|image_ancestry)[a-z_]*(?:capture|open|admit|retain)[a-z_]*)\s*\(/i,
  )?.[1];
  assert.ok(captureCall, "root acquisition must invoke physical image capture");
  assertOrdered(
    acquire,
    captureCall,
    "open_or_create_root",
    "process-image capture before root mutation",
  );

  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const ancestryName = source.match(
      /(?:pub\(crate\)\s+)?struct ([A-Za-z0-9_]*(?:Executable|ProcessImage|ImageAncestry)[A-Za-z0-9_]*)\s*\{/i,
    )?.[1];
    assert.ok(
      ancestryName,
      `${platformName} needs a retained process-image guard`,
    );
    const ancestry = itemBlock(source, "struct", ancestryName);
    const bindingName = ancestry.match(
      /(?:bindings|ancestors):\s*Vec<([A-Za-z0-9_]+)>/,
    )?.[1];
    assert.ok(
      bindingName,
      `${platformName} image ancestry needs exact bindings`,
    );
    assert.match(ancestry, /(?:file|image)[a-z_]*:\s*(?:File|OwnedFd)/);
    assert.match(ancestry, /identity:\s*Identity/);
    const binding = itemBlock(source, "struct", bindingName);
    assert.match(binding, /parent:\s*(?:File|OwnedFd|DirectoryHandle)/);
    assert.match(binding, /name:\s*OsString/);
    assert.match(binding, /identity:\s*Identity/);
  }

  const unixProcessImage = functionBlocks(unix)
    .filter(({ name }) => /executable|process_image|image_ancestry/i.test(name))
    .map(({ source }) => source)
    .join("\n");
  assert.match(unixProcessImage, /openat\(/);
  assert.match(unixProcessImage, /OFlags::NOFOLLOW|directory_flags\(\)/);
  assert.match(unixProcessImage, /fstat\(|(?:file|directory)_identity\(/);
  assert.doesNotMatch(
    unixProcessImage,
    /st_nlink\s*(?:==|!=|<=|>=|<|>)|(?:single|one)[a-z_]*link/,
    "Unix process-image ancestry is binding-specific and must allow hard-linked executables",
  );

  const windowsProcessImage = functionBlocks(windows)
    .filter(({ name }) => /executable|process_image|image_ancestry/i.test(name))
    .map(({ source }) => source)
    .join("\n");
  assert.match(windowsProcessImage, /FILE_OPEN_REPARSE_POINT/);
  assert.match(
    windowsProcessImage,
    /OBJ_CASE_INSENSITIVE/,
    "the Windows process-image path walker must follow AppPaths case semantics",
  );
  assert.match(windowsProcessImage, /object_identity\(|directory_identity\(/);

  const beginReset = functionBlock(rootSession, "begin_reset");
  const resetDrain = terminalDrainContract(
    library,
    beginReset,
    /AUTHORITY_RESETTING|\bResetting\b/,
    "reset",
  );
  const preDrainImageValidations = [
    ...resetDrain.startFlow.matchAll(
      /\b[a-z_]*(?:executable|process_image|image_ancestry|ancestry|reset_scope|containment)[a-z_]*\s*\(/gi,
    ),
  ].map((match) => ({ marker: match[0], index: match.index }));
  const postDrainImageValidations = [
    ...resetDrain.settlement.matchAll(
      /\b[a-z_]*(?:executable|process_image|image_ancestry|ancestry|reset_scope|containment)[a-z_]*\s*\(/gi,
    ),
  ].map((match) => ({ marker: match[0], index: match.index }));
  assert.ok(
    preDrainImageValidations.length >= 1,
    "reset needs physical image classification before terminal drain",
  );
  assert.ok(
    postDrainImageValidations.length >= 1,
    "reset needs pre-drain and post-drain physical image classification",
  );
  assert.ok(
    preDrainImageValidations.some(
      ({ index }) =>
        index < resetDrain.startFlow.indexOf(resetDrain.drainingMarker),
    ),
    "physical process-image refusal must run before terminal drain",
  );
  assert.ok(
    postDrainImageValidations.some(
      ({ index }) =>
        index > resetDrain.settlement.indexOf(resetDrain.settleActiveNonzero) &&
        index < resetDrain.settlement.indexOf(resetDrain.terminalMarker),
    ),
    "physical process-image ancestry must be revalidated after active operations drain",
  );
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const containment = uniqueReachableFunctions(
      `${library}\n${source}`,
      `${resetDrain.startFlow}\n${resetDrain.settlement}`,
    );
    assert.match(
      containment,
      /(?:ancestors|bindings)[\s\S]{0,400}?\.iter\(\)|\.iter\(\)[\s\S]{0,400}?(?:ancestors|bindings)/,
      `${platformName} reset must compare every launched image ancestor`,
    );
    assert.match(
      containment,
      /root[a-z_]*\.identity|root_identity|directory_identity\([^)]*root/,
      `${platformName} reset containment must use physical root identity`,
    );
    assert.match(
      containment,
      /InvalidData|PermissionDenied|Refused|Inside|Ambiguous/,
      `${platformName} reset containment must fail closed`,
    );
  }
});

test("P01-B02 native operations stay relative to retained handles", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );

  assert.doesNotMatch(`${library}\n${platform}`, /create_dir_all/);
  assert.doesNotMatch(platform, /F_GETPATH|\/proc\/self\/fd|\/dev\/fd/);
  assert.match(unix, /struct RootGuard/);
  assert.match(unix, /bindings: Vec<RootBinding>/);
  assert.match(unix, /openat\(/);
  assert.match(unix, /mkdirat\(/);
  assert.match(unix, /renameat(?:_with)?\(/);
  assert.match(unix, /unlinkat\(/);
  assert.match(unix, /OFlags::NOFOLLOW/);
  assert.match(unix, /fstat\(/);
  assert.doesNotMatch(
    unix,
    /rustix::io::dup/,
    "root capabilities and the flock lease need fresh open-file descriptions",
  );
  const freshRootOpen = functionBlocks(unix).find(({ source }) =>
    /openat\([\s\S]*?(?:OsStr::new\("\."\)|"\.")/.test(source),
  );
  assert.ok(freshRootOpen, "Unix needs one held-root reopen primitive");
  for (const operation of ["clone_root", "try_acquire_lease"]) {
    const block = functionBlock(unix, operation);
    assert.match(
      block,
      new RegExp(`openat\\(|\\b${freshRootOpen.name}\\(`),
      `${operation} must open a fresh root description`,
    );
  }
  for (const [operation, cleanupType] of [
    ["park_file_no_replace", "FileCleanupHandle"],
    ["park_directory_no_replace", "DirectoryCleanupHandle"],
  ]) {
    const park = functionBlock(unix, operation);
    assert.match(
      park.split("{")[0],
      new RegExp(
        `Result<${cleanupType}>|cleanup:\\s*&(?:platform::)?${cleanupType}`,
      ),
      `${operation} must receive or return its exact retained cleanup authority`,
    );
    assert.match(park, /RenameFlags::NOREPLACE/);
    const renameIndex = park.indexOf("RenameFlags::NOREPLACE");
    const postEffect = park.slice(renameIndex);
    assert.match(postEffect, /BindingState::Absent/);
    assert.match(postEffect, /BindingState::Exact/);
    assertCountAtLeast(
      postEffect,
      /(?:file|directory)_binding_state\(/,
      2,
      `${operation} must reobserve original and parked bindings after rename`,
    );
  }
  const unixEntries = uniqueReachableFunctions(
    unix,
    functionBlock(unix, "entries"),
  );
  assert.match(
    unixEntries,
    /FileType::Unknown|DT_UNKNOWN/,
    "Unix enumeration must classify unknown d_type through a fallback",
  );
  assert.match(
    unixEntries,
    /statat\(|openat\(|entry_kind\(/,
    "Unix unknown d_type must use a handle-relative observation",
  );

  assert.match(windows, /struct RootGuard/);
  assert.match(windows, /fn open_or_create_root\(/);
  assert.match(windows, /NtCreateFile\(/);
  assert.match(windows, /InitializeObjectAttributes\(/);
  assert.match(windows, /OBJ_CASE_INSENSITIVE/);
  assert.match(
    windows,
    /InitializeObjectAttributes\([\s\S]*?object_flags[\s\S]*?parent\.as_raw_handle\(\)\.cast\(\)/,
  );
  assert.match(windows, /parent\.as_raw_handle\(\)\.cast\(\)/);
  const rootChainOpen = functionBlock(windows, "open_root_chain_directory");
  assert.match(rootChainOpen, /OBJ_CASE_INSENSITIVE/);
  const exactRelativeOpen = functionBlock(windows, "nt_open_relative");
  assert.match(exactRelativeOpen, /nt_open_relative_with_attributes\(/);
  assert.match(
    exactRelativeOpen,
    /share,\s*0,\s*\)/,
    "ordinary managed-child opens must retain exact leaf semantics",
  );
  for (const operation of ["open_directory", "open_file"]) {
    const childOpen = functionBlock(windows, operation);
    assert.match(childOpen, /nt_open_relative\(/);
    assert.doesNotMatch(childOpen, /OBJ_CASE_INSENSITIVE/);
    assert.doesNotMatch(childOpen, /nt_open_relative_with_attributes\(/);
    assert.doesNotMatch(
      childOpen,
      /DELETE_ACCESS/,
      `ordinary ${operation} must not retain cleanup authority`,
    );
  }
  assert.match(
    windows,
    /RootDirectory = destination_parent\.as_raw_handle\(\)/,
  );
  assert.match(windows, /Anonymous\.ReplaceIfExists = false/);
  assert.doesNotMatch(windows, /F_GETPATH|\/proc\/self\/fd|\/dev\/fd|\.join\(/);
  assert.match(
    functionBlock(windows, "open_root_anchor"),
    /\.open\(/,
    "the volume anchor is the sole ambient Windows open",
  );
  assert.doesNotMatch(
    windows.replace(functionBlock(windows, "open_root_anchor"), ""),
    /\.open\(|File::open|CreateFileW/,
    "every managed Windows child must open relative to a retained root",
  );

  const unixReadOpen = functionBlock(unix, "open_file");
  assert.match(unixReadOpen, /OFlags::RDONLY/);
  assert.doesNotMatch(unixReadOpen, /OFlags::RDWR|OFlags::WRONLY/);
  const windowsReadOpen = functionBlock(windows, "open_file");
  assert.match(windowsReadOpen, /FILE_READ_DATA_ACCESS/);
  assert.doesNotMatch(windowsReadOpen, /FILE_WRITE_DATA_ACCESS|DELETE_ACCESS/);
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    for (const binding of ["file_binding_state", "directory_binding_state"]) {
      const observation = functionBlock(source, binding);
      assert.match(
        observation,
        /BindingState::Occupied/,
        `${platformName} ${binding} must distinguish occupied from absent`,
      );
      assert.doesNotMatch(
        observation,
        /(?:InvalidData|PermissionDenied)[\s\S]{0,160}?BindingState::Absent/,
        `${platformName} ${binding} cannot classify wrong type or refused proof as absence`,
      );
    }
  }
  const directoryHandle = itemBlock(windows, "struct", "DirectoryHandle");
  assert.match(directoryHandle, /(?:file|handle):\s*File/);
  assert.match(
    directoryHandle,
    /(?:enumeration|listing|cursor)[a-z_]*:\s*(?:std::sync::)?Mutex<\(\)>/,
    "each retained Windows directory needs one enumeration-cursor mutex",
  );
  const windowsEntries = uniqueReachableFunctions(
    windows,
    functionBlock(windows, "entries"),
  );
  assert.match(
    windowsEntries,
    /(?:enumeration|listing|cursor)[a-z_]*\.lock\(\)/,
  );
  assert.match(windowsEntries, /RestartScan|restart\s*=\s*true/);
  assert.match(windowsEntries, /NtQueryDirectoryFile|FILE_ID_BOTH_DIR_INFO/);
  assert.doesNotMatch(
    windowsEntries,
    /DuplicateHandle|ReOpenFile|try_clone\(/,
    "enumeration must serialize one retained native cursor, not share a duplicated cursor",
  );
  const windowsFunctions = functionBlocks(windows);
  for (const [removal, objectFlag, cleanupType, requiredAccess] of [
    [
      "remove_parked_file",
      "FILE_NON_DIRECTORY_FILE",
      "FileCleanupHandle",
      /FILE_READ_DATA_ACCESS[\s\S]*FILE_READ_ATTRIBUTES[\s\S]*FILE_WRITE_ATTRIBUTES[\s\S]*DELETE_ACCESS[\s\S]*SYNCHRONIZE_ACCESS/,
    ],
    [
      "remove_parked_directory",
      "FILE_DIRECTORY_FILE",
      "DirectoryCleanupHandle",
      /FILE_LIST_DIRECTORY[\s\S]*FILE_TRAVERSE_ACCESS[\s\S]*FILE_READ_ATTRIBUTES[\s\S]*FILE_WRITE_ATTRIBUTES[\s\S]*DELETE_ACCESS[\s\S]*SYNCHRONIZE_ACCESS/,
    ],
  ]) {
    const cleanupOpen = windowsFunctions.find(
      ({ name, source }) =>
        !name.includes("create") &&
        /nt_open_relative\(/.test(source) &&
        /DELETE_ACCESS/.test(source) &&
        /ntapi::ntioapi::FILE_OPEN\b/.test(source) &&
        source.includes(objectFlag),
    );
    assert.ok(
      cleanupOpen,
      `Windows ${removal} needs a transient relative open with DELETE authority`,
    );
    assert.match(cleanupOpen.source.split("{")[0], new RegExp(cleanupType));
    assert.match(cleanupOpen.source, requiredAccess);
    assert.match(cleanupOpen.source, /FILE_OPEN_REPARSE_POINT/);
    assert.match(cleanupOpen.source, /FILE_SYNCHRONOUS_IO_NONALERT/);
    assert.match(cleanupOpen.source, /FILE_SHARE_READ/);
    assert.doesNotMatch(
      cleanupOpen.source,
      /FILE_SHARE_WRITE|FILE_SHARE_DELETE/,
      "the transient cleanup handle must share read only",
    );
    const removalBlock = functionBlock(windows, removal);
    assert.match(removalBlock, new RegExp(`\\b${cleanupOpen.name}\\(`));
    assertOrdered(
      removalBlock,
      cleanupOpen.name,
      "set_delete",
      `exact ${removal} admission before deletion`,
    );
  }
  const setDelete = functionBlock(windows, "set_delete");
  assert.match(setDelete, /FILE_DISPOSITION_INFO_EX/);
  assert.match(setDelete, /FILE_DISPOSITION_FLAG_DELETE/);
  assert.match(
    setDelete,
    /FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE/,
    "managed cleanup must explicitly admit read-only file deletion",
  );
  const ntOpen = functionBlock(windows, "nt_open_relative_with_attributes");
  const failureMarker = ntOpen.match(
    /result\s*<\s*0|!NT_SUCCESS\(result\)/,
  )?.[0];
  assert.ok(failureMarker, "NtCreateFile failure classification is missing");
  const nullMarker = ntOpen.match(/handle\.is_null\(\)/)?.[0];
  assert.ok(
    nullMarker,
    "NT_SUCCESS with a null output handle must be rejected",
  );
  assertOrdered(
    ntOpen,
    failureMarker,
    nullMarker,
    "NT failure before output inspection",
  );
  assertOrdered(
    ntOpen,
    nullMarker,
    "File::from_raw_handle",
    "native handle ownership only after NT_SUCCESS and non-null proof",
  );
  const failureBranch = ntOpen.slice(
    ntOpen.indexOf(failureMarker),
    ntOpen.indexOf(nullMarker),
  );
  assert.doesNotMatch(
    failureBranch.replace(failureMarker, ""),
    /\bhandle\b|from_raw_handle/,
    "NtCreateFile failure output is unspecified and must not be inspected or wrapped",
  );
  assert.equal(
    ntOpen.split("File::from_raw_handle").length - 1,
    1,
    "NtCreateFile success must wrap the returned handle exactly once",
  );
});

test("P01-B02 streams through positional handles and proves completion", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  assert.doesNotMatch(
    windows,
    /\bReOpenFile\b/,
    "Windows streams must not reopen a shared-cursor file description",
  );

  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    for (const operation of ["read_at", "write_at"]) {
      const positional = functionBlock(source, operation);
      assert.match(
        positional.slice(0, positional.indexOf("{")),
        /offset:\s*u64/,
        `${platformName} ${operation} must receive an explicit offset`,
      );
      assert.doesNotMatch(
        positional,
        /SeekFrom|\.seek\s*\(/,
        `${platformName} ${operation} cannot mutate a shared stream cursor`,
      );
    }
  }

  const reader = itemBlock(library, "struct", "FileReader");
  assert.match(reader, /position:\s*u64/);
  assert.match(reader, /operation:\s*[A-Za-z0-9_]+/);
  const readerImplementation = implementationBlock(library, "FileReader");
  const readerFinish = functionBlock(readerImplementation, "finish");
  assert.match(readerFinish, /platform::read_at\s*\(/);
  assert.match(readerFinish, /(?:!=\s*0|match[\s\S]*?0\s*=>)/);
  assert.match(readerFinish, /InvalidData|UnexpectedEof/);
  const readerRead = functionBlock(
    traitImplementationBlock(library, "Read", "FileReader<'_>"),
    "read",
  );
  assert.match(readerRead, /platform::read_at\s*\(/);
  assert.match(readerRead, /position[\s\S]*?checked_add/);

  const writer = itemBlock(library, "struct", "StagedWriter");
  assert.match(writer, /position:\s*u64/);
  assert.match(writer, /operation:\s*[A-Za-z0-9_]+/);
  const writerImplementation = implementationBlock(library, "StagedWriter");
  const writerFinish = functionBlock(writerImplementation, "finish");
  assert.match(writerFinish, /sync_(?:all|data)\s*\(/);
  assert.match(writerFinish, /validate\s*\(/);
  const writerWrite = functionBlock(
    traitImplementationBlock(library, "Write", "StagedWriter<'_>"),
    "write",
  );
  assert.match(writerWrite, /platform::write_at\s*\(/);
  assert.match(writerWrite, /position[\s\S]*?checked_add/);
});

test("P01-B02 enumeration retains opaque cleanup tokens and reports overflow", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const entryStart = library.indexOf("pub struct DirectoryEntry");
  const entryEnd = library.indexOf("\n}", entryStart);
  assert.notEqual(entryStart, -1);
  assert.notEqual(entryEnd, -1);
  const entry = library.slice(entryStart, entryEnd + 2);
  assert.match(entry, /(?:name|leaf): (?:OsString|LeafName)/);
  assert.doesNotMatch(entry, /(?:name|leaf): String/);

  const entryImplementation = implementationBlock(library, "DirectoryEntry");
  const cleanupConsumesEntry = functionBlocks(library).some(
    ({ source }) =>
      /^pub fn/.test(source) &&
      /(?:remove|cleanup|delete|unlink)/.test(source) &&
      /DirectoryEntry/.test(source),
  );
  assert.ok(
    cleanupConsumesEntry ||
      /pub fn [a-z_]*(?:leaf|name)[a-z_]*\(&self\) -> &(?:LeafName|OsStr)/.test(
        entryImplementation,
      ),
    "an observed non-UTF-8 leaf must remain usable for exact cleanup",
  );
  const entryDebug = library.slice(
    library.indexOf("impl fmt::Debug for DirectoryEntry"),
    library.indexOf("impl DirectoryEntry"),
  );
  assert.doesNotMatch(
    entryDebug,
    /\.field\(\s*"(?:name|leaf)"/,
    "opaque directory enumeration must not disclose native leaf names in Debug",
  );

  const listingStart = library.search(
    /pub enum [A-Za-z0-9_]*(?:Entries|Enumeration|Listing)[A-Za-z0-9_]*\s*\{/,
  );
  assert.notEqual(
    listingStart,
    -1,
    "bounded enumeration needs an explicit result type",
  );
  const listingEnd = library.indexOf("\n}", listingStart);
  assert.notEqual(listingEnd, -1);
  const listing = library.slice(listingStart, listingEnd + 2);
  assert.match(listing, /\bComplete\b/);
  assert.match(
    listing,
    /\b(?:Overflow|LimitExceeded|LimitReached|Truncated)\b/,
  );
  const entries = functionBlocks(library).find(
    ({ name, source }) => name === "entries" && /limit: usize/.test(source),
  )?.source;
  assert.ok(entries, "missing bounded directory enumeration");
  assert.doesNotMatch(entries.split("{")[0], /Result<Vec<DirectoryEntry>>/);
  assert.match(entries, /Complete/);
  assert.match(entries, /Overflow|LimitExceeded|LimitReached|Truncated/);
  assert.doesNotMatch(
    entries,
    /limit\s*\.\s*(?:saturating|checked)_add\(1\)|limit\s*\+\s*1/,
    "bounded enumeration must not materialize an N+1 entry",
  );

  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const nativeEntries = functionBlock(source, "entries");
    assert.doesNotMatch(
      nativeEntries.split("{")[0],
      /Result<Vec<\(OsString,\s*EntryKind\)>>/,
      `${platformName} enumeration must return explicit completeness`,
    );
    assert.match(nativeEntries, /limit/);
    assert.match(
      nativeEntries,
      /Complete|Truncated|Overflow|LimitReached|is_complete|complete:/,
      `${platformName} enumeration must report whether the requested bound was complete`,
    );
    assert.doesNotMatch(
      nativeEntries,
      /limit\s*\.\s*(?:saturating|checked)_add\(1\)|limit\s*\+\s*1/,
    );
  }
});

test("P01-B02 owns capability-safe park restore and replacement primitives", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  for (const type of [
    "FileRevision",
    "ExpectedFileContent",
    "FileParkRequest",
    "ParkedFile",
    "ParkedDirectory",
    "StagedFile",
    "SealedStagedFile",
  ]) {
    assertLinear(library, type);
  }

  const filePark = itemBlock(library, "enum", "FileParkOutcome");
  assert.match(filePark, /Parked\(ParkedFile\)/);
  assert.match(filePark, /NoEffect[\s\S]*request:\s*FileParkRequest/);
  assert.match(filePark, /AppliedUnverified\(FileParkObligation\)/);
  const fileParkObligation = itemBlock(library, "struct", "FileParkObligation");
  assert.match(fileParkObligation, /request:\s*Option<FileParkRequest>/);
  assert.match(fileParkObligation, /park_name:\s*LeafName/);

  const directoryPark = itemBlock(library, "enum", "DirectoryParkOutcome");
  assert.match(directoryPark, /Parked\(ParkedDirectory\)/);
  assert.match(directoryPark, /AppliedUnverified\(DirectoryParkObligation\)/);
  const directoryParkObligation = itemBlock(
    library,
    "struct",
    "DirectoryParkObligation",
  );
  assert.match(directoryParkObligation, /park_name:\s*LeafName/);

  const replacement = itemBlock(library, "enum", "ReplaceDestination");
  assert.match(
    replacement,
    /Vacant\s*\{[\s\S]*parent:\s*Directory[\s\S]*name:\s*LeafName/,
  );
  assert.match(replacement, /Existing\(FileParkRequest\)/);
  const replaceOutcome = itemBlock(library, "enum", "FileReplaceOutcome");
  assert.match(
    replaceOutcome,
    /Replaced\s*\{[\s\S]*current:\s*FileCapability[\s\S]*displaced:\s*Option<ParkedFile>/,
    "replacement must return exact ownership of any displaced destination",
  );
  assert.match(replaceOutcome, /NoEffect/);
  assert.match(replaceOutcome, /AppliedUnverified/);
  const sealed = functionBlock(library, "replace_nondurable");
  assert.match(
    sealed,
    /pub fn replace_nondurable\s*\(/,
    "B02 replacement must not claim B03 durability",
  );

  const publicFunctions = functionBlocks(library).filter(({ source }) =>
    /^pub fn/.test(source),
  );
  for (const operation of ["park", "restore", "replace"]) {
    const candidates = publicFunctions.filter(({ name }) =>
      name.includes(operation),
    );
    assert.ok(candidates.length > 0, `missing shared ${operation} primitive`);
    assert.ok(
      candidates.some(
        ({ source }) =>
          /Directory|FileCapability|StagedFile|LeafName/.test(source) &&
          !/(?:&Path\b|PathBuf)/.test(source),
      ),
      `${operation} must consume filesystem capabilities rather than paths`,
    );
  }
  assert.doesNotMatch(library, /pub enum MutationOutcome/);

  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    for (const [operation, binding] of [
      ["park_file_no_replace", "file_binding_state"],
      ["park_directory_no_replace", "directory_binding_state"],
    ]) {
      const park = functionBlock(source, operation);
      const effect = park.match(/renameat|rename_handle|FILE_RENAME_INFO/)?.[0];
      assert.ok(effect, `${platformName} ${operation} has no relative rename`);
      const postEffect = park.slice(park.indexOf(effect));
      assertCountAtLeast(
        postEffect,
        new RegExp(`${binding}\\(`),
        2,
        `${platformName} ${operation} must reobserve both result bindings`,
      );
      assert.match(postEffect, /BindingState::Absent/);
      assert.match(postEffect, /BindingState::Exact/);
    }
  }
});

test("P01-B02 tracks user-origin parks with separate recoverable authorities", async () => {
  const library = await read("core/fs/src/lib.rs");
  const fileTokenName = library.match(
    /struct ([A-Za-z0-9_]*FilePark[A-Za-z0-9_]*Token[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  const directoryTokenName = library.match(
    /struct ([A-Za-z0-9_]*DirectoryPark[A-Za-z0-9_]*Token[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  const fileRecordName = library.match(
    /struct ([A-Za-z0-9_]*FilePark[A-Za-z0-9_]*Record[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  const directoryRecordName = library.match(
    /struct ([A-Za-z0-9_]*DirectoryPark[A-Za-z0-9_]*Record[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  assert.ok(
    fileTokenName &&
      directoryTokenName &&
      fileRecordName &&
      directoryRecordName,
    "file and directory parks need distinct token and registry record types",
  );
  assert.notEqual(fileTokenName, directoryTokenName);
  assert.notEqual(fileRecordName, directoryRecordName);
  assert.doesNotMatch(
    library,
    /pub (?:struct|enum) (?:Mutation|Effect)(?:Registry|Token|Record|Outcome)\b/,
    "park tracking cannot become a generic effect framework",
  );

  for (const [kind, tokenName, recordName, cleanupType] of [
    ["file", fileTokenName, fileRecordName, "FileCleanupHandle"],
    [
      "directory",
      directoryTokenName,
      directoryRecordName,
      "DirectoryCleanupHandle",
    ],
  ]) {
    const token = itemBlock(library, "struct", tokenName);
    assert.match(token, /Weak<CapabilityAuthority>/);
    assert.doesNotMatch(
      library.slice(
        Math.max(0, library.indexOf(token) - 180),
        library.indexOf(token),
      ),
      /#\[derive\([^\]]*\b(?:Clone|Copy)\b[^\]]*\)\]/,
      `${kind} park token must remain linear`,
    );
    const record = itemBlock(library, "struct", recordName);
    assert.match(record, /parent:\s*Directory/);
    assert.match(record, /name:\s*LeafName/);
    assert.match(record, /identity:\s*platform::Identity/);
    assert.match(record, new RegExp(`cleanup:\\s*platform::${cleanupType}`));
    assert.match(record, /(?:phase|state):/);

    const drop = traitImplementationBlock(library, "Drop", tokenName);
    const dropFlow = uniqueReachableFunctions(library, drop);
    assert.match(
      dropFlow,
      /\bAbandoned\b/,
      `${kind} park drop must retain an abandoned registry record`,
    );
    assert.doesNotMatch(
      dropFlow,
      /(?:\.remove\s*\(|unlink|delete|restore|cleanup_[a-z_]*\s*\(|platform::[a-z_]*(?:remove|restore|delete))/,
      `${kind} park drop cannot mutate user-origin content or discard recovery authority`,
    );
  }

  const gateStateName = itemBlock(
    library,
    "struct",
    "CapabilityAuthority",
  ).match(/Mutex<([A-Za-z0-9_]+)>/)?.[1];
  assert.ok(gateStateName, "park registries need the root gate state");
  const gateState = itemBlock(library, "struct", gateStateName);
  const fileRegistryName = gateState.match(
    new RegExp(
      `([a-z_]*file[a-z_]*park[a-z_]*):[^\\n]*\\b${escapeRegExp(fileRecordName)}\\b`,
    ),
  )?.[1];
  const directoryRegistryName = gateState.match(
    new RegExp(
      `([a-z_]*directory[a-z_]*park[a-z_]*):[^\\n]*\\b${escapeRegExp(directoryRecordName)}\\b`,
    ),
  )?.[1];
  assert.ok(
    fileRegistryName && directoryRegistryName,
    "the operation state must retain separate typed park registries",
  );
  assert.match(
    gateState,
    new RegExp(`\\b${escapeRegExp(fileRecordName)}\\b`),
    "the operation state must retain typed file-park records",
  );
  assert.match(
    gateState,
    new RegExp(`\\b${escapeRegExp(directoryRecordName)}\\b`),
    "the operation state must retain typed directory-park records",
  );

  const parkedFile = itemBlock(library, "struct", "ParkedFile");
  const parkedDirectory = itemBlock(library, "struct", "ParkedDirectory");
  const fileObligation = itemBlock(library, "struct", "FileParkObligation");
  const directoryObligation = itemBlock(
    library,
    "struct",
    "DirectoryParkObligation",
  );
  assert.match(parkedFile, new RegExp(`\\b${escapeRegExp(fileTokenName)}\\b`));
  assert.match(
    fileObligation,
    new RegExp(`\\b(?:${escapeRegExp(fileTokenName)}|ParkedFile)\\b`),
  );
  assert.match(
    parkedDirectory,
    new RegExp(`\\b${escapeRegExp(directoryTokenName)}\\b`),
  );
  assert.match(
    directoryObligation,
    new RegExp(`\\b(?:${escapeRegExp(directoryTokenName)}|ParkedDirectory)\\b`),
  );

  for (const [kind, outcome, recordName, effectExpression] of [
    [
      "file",
      "FileParkOutcome",
      fileRecordName,
      /platform::park_file_no_replace\s*\(/,
    ],
    [
      "directory",
      "DirectoryParkOutcome",
      directoryRecordName,
      /platform::park_directory_no_replace\s*\(/,
    ],
  ]) {
    const operation = functionBlocks(library).find(({ source }) =>
      new RegExp(`->\\s*${outcome}\\b`).test(source),
    );
    assert.ok(operation, `missing ${kind} park operation`);
    const flow = uniqueReachableFunctions(library, operation.source);
    const permit = operation.source.match(/\.enter\s*\(\)/)?.[0];
    const reservation = operation.source.match(
      new RegExp(
        `(?:reserve|register)[a-z_]*${kind}[a-z_]*park|${kind}[a-z_]*park[a-z_]*(?:reserve|register)`,
        "i",
      ),
    )?.[0];
    const effect = operation.source.match(effectExpression)?.[0];
    assert.ok(
      permit && reservation && effect,
      `${kind} park needs permit, exact reservation, and native effect`,
    );
    assertOrdered(
      operation.source,
      permit,
      reservation,
      `${kind} permit before park reservation`,
    );
    assertOrdered(
      operation.source,
      reservation,
      effect,
      `${kind} park reservation before namespace effect`,
    );
    assert.match(
      flow,
      new RegExp(`\\b${escapeRegExp(recordName)}\\b`),
      `${kind} park reservation must retain its exact typed record`,
    );
  }

  for (const [kind, carrier] of [
    ["file", "ParkedFile"],
    ["directory", "ParkedDirectory"],
  ]) {
    const implementation = implementationBlock(library, carrier);
    for (const operation of ["remove", "restore"]) {
      const terminal = functionBlocks(implementation).find(({ name }) =>
        name.includes(operation),
      );
      assert.ok(terminal, `${carrier} needs terminal ${operation}`);
      const proof = terminal.source.match(
        /BindingState::Exact|(?:file|directory)_binding_state\s*\(|platform::(?:remove_parked|restore_parked|settle_(?:removed|restored))[a-z_]*\s*\(/,
      )?.[0];
      const disarm = terminal.source.match(
        /\.disarm\s*\(|disarm_[a-z_]*park\s*\(/,
      )?.[0];
      assert.ok(
        proof && disarm,
        `${carrier} ${operation} needs proof then disarm`,
      );
      assertOrdered(
        terminal.source,
        proof,
        disarm,
        `${kind} ${operation} proof before park-token disarm`,
      );
    }
  }

  const recovery = [...library.matchAll(/pub struct ([A-Za-z0-9_]+)\s*\{/g)]
    .map((match) => ({
      name: match[1],
      source: itemBlock(library, "struct", match[1]),
    }))
    .find(
      ({ source }) =>
        /(?:Vec|Option|Box)<ParkedFile>/.test(source) &&
        /(?:Vec|Option|Box)<ParkedDirectory>/.test(source),
    );
  assert.ok(recovery, "root drain must expose typed abandoned park recovery");
  assertLinear(library, recovery.name);
  const rootSession = implementationBlock(library, "RootSession");
  const resetDrain = terminalDrainContract(
    library,
    functionBlock(rootSession, "begin_reset"),
    /AUTHORITY_RESETTING|\bResetting\b/,
    "reset",
  );
  const revokeDrain = terminalDrainContract(
    library,
    functionBlock(rootSession, "revoke"),
    /AUTHORITY_REVOKED|\bRevoked\b/,
    "revocation",
    /\bRevoked\b/,
  );
  const drainFlow = uniqueReachableFunctions(
    library,
    `${resetDrain.settlement}\n${revokeDrain.settlement}`,
  );
  assert.match(
    drainFlow,
    new RegExp(`\\b${escapeRegExp(fileRegistryName)}\\b`),
  );
  assert.match(
    drainFlow,
    new RegExp(`\\b${escapeRegExp(directoryRegistryName)}\\b`),
  );
  assert.match(drainFlow, /\bLive\b/);
  assert.match(
    drainFlow,
    /(?:file|directory)_parks\.is_empty\(\)[\s\S]{0,240}?(?:Pending|Recovery)/,
    "live user-origin parks must keep terminal drain unsettled",
  );
  assert.match(drainFlow, /\bAbandoned\b/);
  assert.match(drainFlow, /(?:limit|max|capacity|TooMany|Overflow)/i);
  assert.match(
    `${itemBlock(library, "enum", resetDrain.outcomeName)}\n${itemBlock(
      library,
      "enum",
      revokeDrain.outcomeName,
    )}\n${itemBlock(library, "struct", "RootResetAuthority")}`,
    new RegExp(`\\b${escapeRegExp(recovery.name)}\\b`),
    "terminal drain must transfer bounded abandoned carriers to its caller",
  );

  const recoveryImplementation = implementationBlock(library, recovery.name);
  assert.doesNotMatch(
    recoveryImplementation,
    /pub fn into_(?:parts|files|directories)\s*\(/,
    "abandoned parks cannot detach from their typed drain owner",
  );
  const recoveryPermitName = library.match(
    /struct ([A-Za-z0-9_]*(?:(?:Drain[A-Za-z0-9_]*Recovery)|(?:Recovery[A-Za-z0-9_]*Drain))[A-Za-z0-9_]*Permit[A-Za-z0-9_]*)\s*(?:<[^>{}]+>)?\s*\{/,
  )?.[1];
  assert.ok(
    recoveryPermitName,
    "drain recovery needs a private token-bound operation permit",
  );
  assert.doesNotMatch(
    library,
    new RegExp(
      `pub(?:\\([^)]*\\))?\\s+struct ${escapeRegExp(recoveryPermitName)}\\b`,
    ),
    "drain-recovery permits cannot be minted by consumers",
  );
  const recoveryPermit = itemBlock(library, "struct", recoveryPermitName);
  assert.match(recoveryPermit, /CapabilityAuthority/);
  assert.match(
    recoveryPermit,
    new RegExp(
      `(?:${escapeRegExp(fileTokenName)}|${escapeRegExp(directoryTokenName)}|(?:park|token|record)[a-z_]*_id:)`,
    ),
    "the drain-recovery permit must be bound to one exact park token",
  );

  for (const drain of [resetDrain, revokeDrain]) {
    const outcome = itemBlock(library, "enum", drain.outcomeName);
    const recoveryCarrierName =
      outcome.match(/\bRecovery\s*\(\s*([A-Za-z0-9_]+)\s*\)/)?.[1] ??
      outcome.match(
        /\bRecovery\s*\{[\s\S]{0,240}?\b(?:recovery|authority):\s*([A-Za-z0-9_]+)\b/,
      )?.[1];
    assert.ok(
      recoveryCarrierName,
      `${drain.outcomeName} must return one non-detachable recovery carrier`,
    );
    assertLinear(library, recoveryCarrierName);
    const carrier = itemBlock(library, "struct", recoveryCarrierName);
    assert.match(
      carrier,
      new RegExp(`\\b${escapeRegExp(drain.pendingName)}\\b`),
    );
    assert.match(carrier, new RegExp(`\\b${escapeRegExp(recovery.name)}\\b`));
    assert.match(
      carrier,
      new RegExp(`\\b${escapeRegExp(recoveryPermitName)}\\b`),
    );
  }

  const normalAdmission = functionBlock(library, "enter");
  assert.doesNotMatch(
    normalAdmission,
    /AUTHORITY_DRAINING|\bDraining\b/,
    "ordinary capability admission must remain LIVE-only",
  );
  for (const tokenType of [fileTokenName, directoryTokenName]) {
    const drainAdmission = functionBlocks(library).find(({ source }) => {
      const header = source.slice(0, source.indexOf("{"));
      return (
        new RegExp(`&${escapeRegExp(recoveryPermitName)}\\b`).test(header) &&
        new RegExp(`&${escapeRegExp(tokenType)}\\b`).test(header) &&
        /AUTHORITY_DRAINING|\bDraining\b/.test(source)
      );
    });
    assert.ok(
      drainAdmission,
      `${tokenType} needs exact drain-recovery admission while DRAINING`,
    );
    assert.match(drainAdmission.source, /(?:ptr_eq|as_ptr|\.id\b)/);
  }
});

test("P01-B02 tracks stage ownership through drop promotion and reset", async () => {
  const library = await read("core/fs/src/lib.rs");
  const tokenName = library.match(
    /struct ([A-Za-z0-9_]*Stage[A-Za-z0-9_]*Token[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  const recordName = library.match(
    /struct ([A-Za-z0-9_]*Stage[A-Za-z0-9_]*Record[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  assert.ok(
    tokenName && recordName,
    "stages need private token and registry record types",
  );

  const token = itemBlock(library, "struct", tokenName);
  assert.match(token, /Weak<CapabilityAuthority>/);
  assert.match(token, /(?:armed|active):\s*bool/);
  const record = itemBlock(library, "struct", recordName);
  assert.match(record, /parent:\s*Directory/);
  assert.match(record, /name:\s*LeafName/);
  assert.match(record, /identity:\s*platform::Identity/);
  assert.match(record, /cleanup:\s*platform::FileCleanupHandle/);
  assert.match(record, /(?:phase|state):/);
  const destinationName = record.match(
    /destination:\s*Option<([A-Za-z0-9_]+)>/,
  )?.[1];
  assert.ok(
    destinationName,
    "stage registry must retain an attempted promotion destination",
  );
  const destination = itemBlock(library, "struct", destinationName);
  assert.match(destination, /parent:\s*Directory/);
  assert.match(destination, /name:\s*LeafName/);

  const gateStateName = itemBlock(
    library,
    "struct",
    "CapabilityAuthority",
  ).match(/Mutex<([A-Za-z0-9_]+)>/)?.[1];
  assert.ok(gateStateName, "stage registry needs the authority gate state");
  const gateState = itemBlock(library, "struct", gateStateName);
  assert.match(
    gateState,
    new RegExp(`HashMap<[^,>]+,\\s*${escapeRegExp(recordName)}>`),
  );

  const staged = itemBlock(library, "struct", "StagedFile");
  const sealed = itemBlock(library, "struct", "SealedStagedFile");
  assert.match(staged, new RegExp(`\\b${escapeRegExp(tokenName)}\\b`));
  assert.match(
    sealed,
    new RegExp(`\\b(?:${escapeRegExp(tokenName)}|StagedFile)\\b`),
    "sealing must transfer rather than discard the stage token",
  );
  for (const type of ["StagedFile", "SealedStagedFile"]) {
    const declaration = library.indexOf(`pub struct ${type}`);
    assert.match(
      library.slice(Math.max(0, declaration - 180), declaration),
      /#\[must_use/,
      `${type} must make unresolved ownership visible`,
    );
  }

  const tokenDropStart = library.indexOf(`impl Drop for ${tokenName}`);
  assert.notEqual(
    tokenDropStart,
    -1,
    "stage token needs cancellation cleanup on drop",
  );
  const tokenDrop = library.slice(
    tokenDropStart,
    library.indexOf("\n}", tokenDropStart) + 2,
  );
  assert.match(tokenDrop, /(?:cleanup|discard)[a-z_]*\(/);
  const tokenImplementation = implementationBlock(library, tokenName);
  const discard = functionBlock(tokenImplementation, "discard");
  const cleanupName = discard.match(
    /\.([a-z_]*(?:cleanup|discard)[a-z_]*stage[a-z_]*)\s*\(/,
  )?.[1];
  assert.ok(
    cleanupName,
    "stage discard must delegate to exact registry cleanup",
  );
  const stageCleanup = `${tokenDrop}\n${discard}\n${functionBlock(
    implementationBlock(library, "CapabilityAuthority"),
    cleanupName,
  )}`;
  assert.match(stageCleanup, /BindingState::Exact|file_binding_state\s*\(/);
  assert.doesNotMatch(
    stageCleanup,
    /ParkedFile|ParkedDirectory|FilePark|DirectoryPark/,
    "the app-created stage registry cannot consume user-origin park authority",
  );

  const seal = functionBlock(
    implementationBlock(library, "StagedFile"),
    "seal",
  );
  const contentSync = seal.match(/sync_(?:all|data)\s*\(/)?.[0];
  const sealedState = seal.match(
    /StageRegistryPhase::Sealed|mark_[a-z_]*sealed\s*\(/,
  )?.[0];
  assert.ok(
    contentSync && sealedState,
    "stage sealing needs content sync and registry state",
  );
  assertOrdered(
    seal,
    contentSync,
    sealedState,
    "stage content sync before sealed registry state",
  );

  const promotionMethods = functionBlocks(library).filter(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:mut )?self\b/.test(source.slice(0, source.indexOf("{"))) &&
      /promote|replace/.test(name) &&
      /File(?:Promotion|Replace)Outcome/.test(
        source.slice(0, source.indexOf("{")),
      ),
  );
  assert.ok(
    promotionMethods.length > 0,
    "sealed stage needs a promotion operation",
  );
  for (const { name, source } of promotionMethods) {
    const flow = uniqueReachableFunctions(library, source);
    const preparation = flow.match(
      /\.((?:prepare|reserve|register)[a-z_]*(?:promotion|replace|stage)[a-z_]*)\s*\(/,
    );
    assert.ok(preparation, `${name} does not reserve its stage transition`);
    const preparationIndex = flow.indexOf(preparation[0]);
    const namespaceEffect = flow
      .slice(preparationIndex)
      .match(/platform::(?:rename|replace)[a-z_]*|renameat|rename_handle/)?.[0];
    assert.ok(namespaceEffect, `${name} has no namespace effect marker`);
    const namespaceIndex = flow.indexOf(namespaceEffect, preparationIndex);
    const parentSync = flow
      .slice(namespaceIndex)
      .match(
        /(?:sync_[a-z_]*(?:rename|source|destination|parents)|sync_directory)\s*\(/,
      )?.[0];
    assert.ok(parentSync, `${name} does not synchronize the renamed namespace`);
    const parentSyncIndex = flow.indexOf(parentSync, namespaceIndex);
    const disarm = flow
      .slice(parentSyncIndex)
      .match(/\.disarm\(|disarm_stage\(/)?.[0];
    assert.ok(disarm, `${name} does not disarm its stage token`);
    const disarmIndex = flow.indexOf(disarm, parentSyncIndex);
    const resultProof = flow
      .slice(namespaceIndex, disarmIndex)
      .match(/BindingState::Exact|file_binding_state/)?.[0];
    assert.match(flow, /PromotionAttempted/);
    assert.ok(resultProof, `${name} has no exact result-binding proof`);
    assert.ok(
      preparationIndex < namespaceIndex,
      `${name} registry transfer before namespace effect`,
    );
    const preparationFlow = uniqueReachableFunctions(
      library,
      functionBlock(library, preparation[1]),
    );
    assert.match(preparationFlow, /destination\s*=|destination:/);
    assert.match(preparationFlow, /PromotionAttempted/);
    assert.ok(
      namespaceIndex < parentSyncIndex && parentSyncIndex < disarmIndex,
      `${name} effect then parent sync then stage-token disarm`,
    );
  }

  const parentSyncHelper = functionBlocks(library).find(
    ({ name, source }) =>
      /sync/.test(name) &&
      /source:\s*&Directory/.test(source) &&
      /destination:\s*&Directory/.test(source),
  );
  assert.ok(
    parentSyncHelper,
    "cross-directory publication needs a shared parent-sync primitive",
  );
  assertCountAtLeast(
    parentSyncHelper.source,
    /platform::sync_directory\s*\(/,
    2,
    "cross-directory publication must sync both retained parent capabilities",
  );
  const destinationSync = parentSyncHelper.source.match(
    /platform::sync_directory\s*\(\s*&destination\b[^)]*\)/,
  )?.[0];
  const sourceSync = parentSyncHelper.source.match(
    /platform::sync_directory\s*\(\s*&source\b[^)]*\)/,
  )?.[0];
  assert.ok(
    destinationSync && sourceSync,
    "parent synchronization must target both exact directory capabilities",
  );
  assertOrdered(
    parentSyncHelper.source,
    destinationSync,
    sourceSync,
    "destination durability before distinct source removal durability",
  );
  assert.match(
    parentSyncHelper.source,
    /(?:ptr_eq|identity|same_[a-z_]*(?:directory|parent))\s*\(/,
    "same-parent publication may collapse the duplicate parent sync",
  );

  const promotionObligation = itemBlock(
    library,
    "struct",
    "FilePromotionObligation",
  );
  assert.match(
    promotionObligation,
    new RegExp(
      `\\b(?:${escapeRegExp(tokenName)}|SealedStagedFile|StagedFile)\\b`,
    ),
    "promotion ambiguity must retain the armed stage token",
  );
  const replaceObligationName = itemBlock(
    library,
    "enum",
    "FileReplaceOutcome",
  ).match(
    /AppliedUnverified(?:\(\s*|\s*\{[\s\S]{0,320}?obligation:\s*)([A-Za-z0-9_]+Obligation)\b/,
  )?.[1];
  assert.ok(replaceObligationName, "replacement ambiguity needs an obligation");
  const replaceObligation = itemBlock(library, "struct", replaceObligationName);
  const replaceStateName = replaceObligation.match(
    /(?:state|retained):\s*Option<([A-Za-z0-9_]+)>/,
  )?.[1];
  const replaceRetainedState = replaceStateName
    ? itemBlock(library, "enum", replaceStateName)
    : replaceObligation;
  assert.match(
    replaceRetainedState,
    new RegExp(
      `\\b(?:${escapeRegExp(tokenName)}|SealedStagedFile|StagedFile)\\b`,
    ),
    "replacement ambiguity must retain the armed stage token",
  );
});

test("P01-B02 root lease is retained, identity-bound, and fail-fast", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const rootSessionImplementation = implementationBlock(library, "RootSession");
  const acquire = functionBlock(rootSessionImplementation, "acquire");

  const authority = itemBlock(library, "struct", "CapabilityAuthority");
  assert.match(authority, /Mutex</);
  assert.match(authority, /root: platform::RootGuard/);
  assert.match(authority, /lease: platform::LeaseHandle/);
  assert.doesNotMatch(library, /RwLock|RwLockReadGuard|RwLockWriteGuard/);
  assert.doesNotMatch(library, /AtomicU8/);
  const gateStateName = authority.match(/Mutex<([A-Za-z0-9_]+)>/)?.[1];
  assert.ok(
    gateStateName,
    "capability authority needs a mutex-protected gate state",
  );
  const gateState = itemBlock(library, "struct", gateStateName);
  assert.match(gateState, /(?:phase|state):/);
  assert.match(gateState, /(?:active|in_flight|operations):/);
  assert.match(
    library,
    /pub struct RootSession\s*\{[\s\S]*?authority: Arc<CapabilityAuthority>/,
  );
  assert.doesNotMatch(
    library,
    /#\[derive\([^\]]*Clone[^\]]*\)\]\s*pub struct RootSession/,
  );
  assert.doesNotMatch(library, /impl Clone for RootSession\b/);
  assert.match(
    itemBlock(library, "struct", "DirectoryInner"),
    /Weak<CapabilityAuthority>/,
  );
  assert.match(
    itemBlock(library, "struct", "FileCapability"),
    /Weak<CapabilityAuthority>/,
  );
  const permitName = [
    ...library.matchAll(/(?:^|\n)struct ([A-Za-z0-9_]+)\s*\{/g),
  ]
    .map((match) => match[1])
    .find((name) => {
      const item = itemBlock(library, "struct", name);
      if (!/Arc<CapabilityAuthority>/.test(item)) return false;
      try {
        return /(?:active|in_flight|operations)[\s\S]*?(?:-=\s*1|checked_sub\(1\))/.test(
          traitImplementationBlock(library, "Drop", name),
        );
      } catch {
        return false;
      }
    });
  assert.ok(permitName, "active operations need an owned private permit");
  assert.doesNotMatch(library, new RegExp(`pub struct ${permitName}\\b`));
  assert.match(
    library,
    /\.authority\.upgrade\(\)\.ok_or_else\(stale_capability\)/,
  );
  const enter = functionBlock(library, "enter");
  assert.match(enter, /\.lock\(\)/);
  assert.match(enter, /AUTHORITY_LIVE|\bLive\b/);
  const activeIncrement = enter.match(
    /(?:active|in_flight|operations)[\s\S]{0,120}?checked_add\(1\)/,
  );
  assert.ok(
    activeIncrement,
    "operation admission must increment the active count",
  );
  const liveMarker = enter.match(/AUTHORITY_LIVE|\bLive\b/)[0];
  assertOrdered(
    enter,
    ".lock()",
    liveMarker,
    "operation gate lock before phase check",
  );
  assertOrdered(
    enter,
    liveMarker,
    activeIncrement[0],
    "LIVE check before operation admission",
  );
  assert.doesNotMatch(enter, /saturating_add/);
  const permitConstruction = enter.match(
    new RegExp(`${escapeRegExp(permitName)}\\s*\\{`),
  )?.[0];
  assert.ok(
    permitConstruction,
    "operation admission must construct its owned permit",
  );
  const releaseGate = enter.match(/drop\((?:state|gate|guard)\)/)?.[0];
  const validateLease = enter.match(
    /platform::validate_lease\(&self\.lease\)\??/,
  )?.[0];
  const validateRoot = enter.match(
    /platform::validate_root\(&self\.root\)\??/,
  )?.[0];
  const validateAuthority = enter.match(
    /(?:platform::)?validate_[a-z_]*(?:authority|session|root_and_lease|lease_and_root)[a-z_]*\([^;]*\)\??/,
  )?.[0];
  assert.ok(
    (validateLease && validateRoot) || validateAuthority,
    "operation admission must validate root and lease",
  );
  assertOrdered(
    enter,
    activeIncrement[0],
    permitConstruction,
    "active admission before owned permit construction",
  );
  const validationMarkers = [
    validateLease,
    validateRoot,
    validateAuthority,
  ].filter(Boolean);
  const firstValidation = validationMarkers.reduce((first, marker) =>
    enter.indexOf(marker) < enter.indexOf(first) ? marker : first,
  );
  assertOrdered(
    enter,
    permitConstruction,
    firstValidation,
    "owned permit before fallible native validation",
  );
  if (releaseGate) {
    assertOrdered(
      enter,
      permitConstruction,
      releaseGate,
      "owned permit construction before explicit gate release",
    );
    assertOrdered(
      enter,
      releaseGate,
      firstValidation,
      "explicit gate release before native validation",
    );
  }
  for (const validation of validationMarkers) {
    assert.match(validation, /\?$/, "native validation must propagate failure");
  }
  const permitDropStart = library.indexOf(`impl Drop for ${permitName}`);
  const permitDropEnd = library.indexOf("\n}", permitDropStart);
  assert.notEqual(permitDropStart, -1);
  assert.notEqual(permitDropEnd, -1);
  const permitDrop = library.slice(permitDropStart, permitDropEnd + 2);
  const activeDecrement = permitDrop.match(
    /(?:active|in_flight|operations)[\s\S]*?(?:-=\s*1|checked_sub\(1\))/,
  );
  assert.ok(activeDecrement, "permit drop must decrement the active count");
  assert.doesNotMatch(
    permitDrop,
    /saturating_sub/,
    "permit drop must expose active-count underflow instead of hiding it",
  );
  if (/notify_(?:one|all)\(/.test(permitDrop)) {
    assertOrdered(
      permitDrop,
      activeDecrement[0],
      "notify_",
      "permit decrement before optional drain notification",
    );
  }
  const rootCapability = functionBlock(library, "root");
  assert.match(rootCapability, /Arc::downgrade\(&self\.authority\)/);
  assert.doesNotMatch(rootCapability, /self\.authority\.clone\(\)/);
  assertOrdered(
    acquire,
    "open_or_create_root",
    "try_acquire_lease",
    "root lease acquisition",
  );
  assert.match(acquire, /io::ErrorKind::WouldBlock/);
  assert.match(acquire, /RootSessionError::Busy/);
  assert.match(unix, /libc::LOCK_EX \| libc::LOCK_NB/);
  assert.match(unix, /struct LeaseHandle[\s\S]*?root_identity: Identity/);
  assert.match(unix, /fn validate_lease\(/);
  assert.match(windows, /ntapi::ntioapi::FILE_OPEN_IF/);
  assert.match(
    functionBlock(windows, "try_acquire_lease"),
    /FILE_OPEN_IF[\s\S]*?\n\s*0,\n/,
  );
  assert.match(windows, /ERROR_SHARING_VIOLATION/);
  assert.match(windows, /fn validate_lease\(/);
  assert.match(library, /pub fn revoke\(self\)/);
  assert.doesNotMatch(library, /pub fn revoke_capabilities\(&self\)/);
  const beginReset = functionBlock(rootSessionImplementation, "begin_reset");
  assert.match(beginReset.split("{")[0], /begin_reset\(self\)/);
  const resetDrain = terminalDrainContract(
    library,
    beginReset,
    /AUTHORITY_RESETTING|\bResetting\b/,
    "reset",
  );
  const revoke = functionBlock(rootSessionImplementation, "revoke");
  const revokeDrain = terminalDrainContract(
    library,
    revoke,
    /AUTHORITY_REVOKED|\bRevoked\b/,
    "revocation",
    /\bRevoked\b/,
  );
  const rootSessionDrop = traitImplementationBlock(
    library,
    "Drop",
    "RootSession",
  );
  assert.doesNotMatch(
    rootSessionDrop,
    /\.wait(?:_while)?\(|while\s+[^\{]*(?:active|in_flight|operations)/,
  );
  assert.doesNotMatch(
    rootSessionDrop,
    /AUTHORITY_RESETTING|AUTHORITY_REVOKED|\bResetting\b|\bRevoked\b/,
    "RootSession drop cannot claim terminal settlement",
  );
  const resetAuthority = itemBlock(library, "struct", "RootResetAuthority");
  assert.match(
    resetAuthority,
    /(?:RootSession|Arc<CapabilityAuthority>)/,
    "reset authority must become the unique strong root and lease owner",
  );
  const resetImplementationStart = library.indexOf("impl RootResetAuthority");
  const resetImplementationEnd = library.indexOf(
    "impl Drop for RootResetAuthority",
  );
  assert.notEqual(resetImplementationStart, -1);
  assert.notEqual(resetImplementationEnd, -1);
  const resetImplementation = library.slice(
    resetImplementationStart,
    resetImplementationEnd,
  );
  assert.match(
    resetImplementation,
    /pub fn (?:finish|release)\((?:mut )?self\)/,
  );
  assert.match(
    resetImplementation,
    /pub fn [a-z_]*(?:clear|remove)[a-z_]*\(/,
    "only reset authority may clear the retained root",
  );
  assert.doesNotMatch(resetImplementation, /(?:&Path\b|PathBuf)/);
  assert.match(
    library,
    /pub struct FileCapability\s*\{[\s\S]*?parent: Directory,[\s\S]*?name: LeafName/,
  );
  const strongAuthorityOwners = new Set([
    "RootSession",
    "RootResetAuthority",
    resetDrain.pendingName,
    revokeDrain.pendingName,
  ]);
  for (const match of library.matchAll(/pub struct ([A-Za-z0-9_]+)\s*\{/g)) {
    if (strongAuthorityOwners.has(match[1])) continue;
    const item = itemBlock(library, "struct", match[1]);
    assert.doesNotMatch(
      item,
      /Arc<CapabilityAuthority>/,
      `${match[1]} must not prolong the root lease after session reset`,
    );
  }
});

test("P01-B02 admits external absolute roots into the one live session", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const rootSession = implementationBlock(library, "RootSession");
  const admission = functionBlocks(rootSession).find(
    ({ source }) =>
      /^pub fn/.test(source) &&
      /(?:path|root):\s*&Path/.test(source) &&
      /Directory/.test(source) &&
      !/fn acquire\b/.test(source),
  );
  assert.ok(
    admission,
    "RootSession needs session-bound absolute-directory admission for configured external roots",
  );
  assert.match(admission.source, /\.is_absolute\(\)/);
  assert.match(admission.source, /Arc::downgrade\(&self\.authority\)/);
  assert.doesNotMatch(
    admission.source,
    /Arc::new\(\s*CapabilityAuthority|RootSession::acquire/,
    "external roots cannot mint an independent authority or lease",
  );
  assert.match(
    admission.source,
    /(?:bindings|ancestors|ancestry|parent):/,
    "external root admission must retain its absolute ancestry guard",
  );

  const platformAdmission = admission.source.match(
    /platform::([a-z_]*(?:absolute|external|root)[a-z_]*(?:directory|ancestry|guard)[a-z_]*)\s*\(/,
  )?.[1];
  assert.ok(
    platformAdmission,
    "external root admission must delegate to native walking",
  );
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const unixAdmission = functionBlock(unix, platformAdmission);
  assert.match(unixAdmission, /openat\(/);
  assert.match(unixAdmission, /OFlags::NOFOLLOW|directory_flags\(\)/);
  assert.match(unixAdmission, /(?:bindings|ancestors|ancestry)/);
  const windowsAdmission = functionBlock(windows, platformAdmission);
  assert.match(windowsAdmission, /open_root_anchor|NtCreateFile/);
  assert.match(windowsAdmission, /open_root_chain_directory/);
  assert.match(windowsAdmission, /OBJ_CASE_INSENSITIVE/);
  assert.match(windowsAdmission, /(?:bindings|ancestors|ancestry)/);

  const directoryValidation = uniqueReachableFunctions(
    library,
    functionBlock(implementationBlock(library, "Directory"), "validate"),
  );
  assert.match(directoryValidation, /(?:bindings|ancestors|ancestry|parent)/);
  assert.match(directoryValidation, /BindingState::Exact|validate_[a-z_]*root/);
});

terminalTest(
  "P01-B02 preserves B01 root selection and portable naming authority",
  async () => {
    const [
      bootstrap,
      paths,
      portable,
      fsLibrary,
      productionTauri,
      developmentTauri,
    ] = await Promise.all([
      read("apps/api/src/bootstrap.rs"),
      read("core/config/src/paths/mod.rs"),
      read("core/minecraft/src/portable_path.rs"),
      read("core/fs/src/lib.rs"),
      read("apps/desktop/tauri.conf.json"),
      read("apps/desktop/tauri.dev.conf.json"),
    ]);

    assert.match(
      bootstrap,
      /pub const APP_IDENTIFIER: &str = "dev\.mateoltd\.axial";/,
    );
    assert.match(
      bootstrap,
      /pub const DEVELOPMENT_APP_IDENTIFIER: &str = "dev\.mateoltd\.axial\.dev";/,
    );
    assert.match(bootstrap, /NativeIdentifierMismatch/);
    assert.match(paths, /pub fn from_root\(/);
    assert.doesNotMatch(paths, /pub fn detect\(|pub fn root\s*\(/);
    assert.doesNotMatch(paths.split("#[cfg(test)]", 1)[0], /std::env/);
    assert.equal(JSON.parse(productionTauri).identifier, "dev.mateoltd.axial");
    assert.equal(
      JSON.parse(developmentTauri).identifier,
      "dev.mateoltd.axial.dev",
    );

    for (const type of [
      "PortableFileName",
      "PortableRelativePath",
      "PortablePathKey",
    ]) {
      assert.match(portable, new RegExp(`pub struct ${type}\\b`));
      assert.doesNotMatch(fsLibrary, new RegExp(`\\b${type}\\b`));
    }
    assert.match(portable, /value\.case_fold\(\)\.collect::<String>\(\)/);
    assert.match(portable, /folded\.as_str\(\)\.nfc\(\)\.collect\(\)/);
  },
);

terminalTest(
  "P01-B02 acquires and retains the application root before every store",
  async () => {
    const [
      configLibrary,
      configSources,
      fsLibrary,
      bootstrap,
      apiMain,
      desktopMain,
      state,
    ] = await Promise.all([
      read("core/config/src/lib.rs"),
      readRustTree("core/config/src"),
      read("core/fs/src/lib.rs"),
      read("apps/api/src/bootstrap.rs"),
      read("apps/api/src/main.rs"),
      read("apps/desktop/src/main.rs"),
      read("apps/api/src/state/mod.rs"),
    ]);
    const combinedConfig = configSources.map(([, source]) => source).join("\n");

    assert.match(configLibrary, /AppRootSession/);
    assert.match(combinedConfig, /pub struct AppRootSession/);
    assert.match(
      combinedConfig,
      /axial_fs::RootSession|use axial_fs::[^;]*RootSession/,
    );
    const appRootSession = itemBlock(
      combinedConfig,
      "struct",
      "AppRootSession",
    );
    assert.match(
      appRootSession,
      /Mutex<[^>]*Option<RootSession>[^>]*>/,
      "AppRootSession must uniquely own one takeable native session",
    );
    assert.doesNotMatch(
      appRootSession,
      /Arc<RootSession>|RootSession\s*,\s*RootSession/,
    );
    const appRootImplementation = implementationBlock(
      combinedConfig,
      "AppRootSession",
    );
    const externalAdmission = functionBlocks(appRootImplementation).find(
      ({ source }) =>
        /^pub fn/.test(source) &&
        /(?:path|root):\s*&Path/.test(source) &&
        /Directory/.test(source),
    );
    assert.ok(
      externalAdmission,
      "AppRootSession must expose same-session admission for configured external roots",
    );
    assert.match(
      externalAdmission.source,
      /(?:admit|open)[a-z_]*(?:absolute|external|directory|root)[a-z_]*\(/,
    );
    const resetOwner = functionBlocks(appRootImplementation).find(
      ({ name, source }) =>
        /begin_reset|take_reset|reset_authority/.test(name) &&
        /\.take\(\)/.test(source),
    );
    assert.ok(
      resetOwner,
      "reset must take the sole native session from the shared wrapper",
    );
    assert.match(bootstrap, /pub fn open_app_root_session\(/);
    assert.match(bootstrap, /Result<AppRootSession/);
    const openSession = functionBlock(bootstrap, "open_app_root_session");
    const rootAcquire = functionBlock(fsLibrary, "acquire");
    assert.match(
      `${openSession}\n${rootAcquire}`,
      /current_exe\(|executable|process_image/,
      "startup must capture executable ancestry as part of root admission",
    );
    assert.match(
      itemBlock(fsLibrary, "struct", "CapabilityAuthority"),
      /(?:executable|process_image|image_ancestry)/i,
      "the one retained native authority must own executable ancestry proof",
    );
    assert.match(
      `${combinedConfig}\n${fsLibrary}`,
      /(?:Executable|ProcessImage)[A-Za-z0-9_]*[\s\S]{0,2400}?(?:DirectoryIdentity|platform::Identity)|(?:DirectoryIdentity|platform::Identity)[\s\S]{0,2400}?(?:executable|process_image)/i,
      "executable ancestry must use physical capability identity",
    );
    assert.match(state, /(?:_)?root_session:\s*Arc<AppRootSession>/);

    for (const [name, binary] of [
      ["API", apiMain],
      ["desktop", desktopMain],
    ]) {
      assert.equal(
        binary.split("open_app_root_session(").length - 1,
        1,
        `${name} must acquire exactly one root session`,
      );
      assertOrdered(
        binary,
        "open_app_root_session(",
        "ConfigStore::load_for_startup",
        `${name} lease before ConfigStore`,
      );
      for (const store of [
        "InstanceStore::load_for_startup",
        "PerformanceManager::load_for_startup",
      ]) {
        assertOrdered(
          binary,
          "open_app_root_session(",
          store,
          `${name} lease before ${store}`,
        );
      }
      assert.match(
        binary,
        /Arc::new\([^)]*root_session[^)]*\)|root_session\.clone\(\)|Arc::clone\(&[a-z_]*root_session\)/,
      );
      const startup = functionBlocks(binary).find(
        ({ name }) => name === "run",
      )?.source;
      assert.ok(startup, `${name} needs one startup composition function`);
      for (const store of [
        "ConfigStore::load_for_startup",
        "InstanceStore::load_for_startup",
        "PerformanceManager::load_for_startup",
      ]) {
        const storeIndex = startup.indexOf(store);
        assert.notEqual(storeIndex, -1, `${name} is missing ${store}`);
        assert.match(
          startup.slice(Math.max(0, storeIndex - 500), storeIndex + 500),
          /root_session|app_root/,
          `${name} ${store} must consume the shared root authority`,
        );
      }
    }
  },
);

terminalTest("P01-B02 leaves one shared physical adapter", async () => {
  const [anchoredRecord, managedFs, launchReports, performanceLibrary] =
    await Promise.all([
      read("apps/api/src/execution/anchored_record.rs"),
      read("core/minecraft/src/managed_fs.rs"),
      read("apps/api/src/state/launch_reports.rs"),
      read("core/performance/src/lib.rs"),
    ]);

  for (const [path, source] of [
    ["apps/api/src/execution/anchored_record.rs", anchoredRecord],
    ["core/minecraft/src/managed_fs.rs", managedFs],
  ]) {
    assert.match(
      source,
      /axial_fs::|use axial_fs::/,
      `${path} must adapt axial-fs`,
    );
    assert.doesNotMatch(source, /mod (?:platform|native)\s*\{/);
    assert.doesNotMatch(
      source,
      /rustix::|windows_sys::|ntapi::|libc::|F_GETPATH/,
    );
  }
  assert.equal(await exists("core/performance/src/file_identity.rs"), false);
  assert.doesNotMatch(performanceLibrary, /^mod file_identity;$/m);
  assert.doesNotMatch(
    launchReports,
    /AdmittedFileIdentity|admitted_(?:path_snapshot|unix_identity|file_identity)|GetFileInformationByHandleEx|MetadataExt/,
  );
});

terminalTest("P01-B02 deletes raw mutation and migration residue", async () => {
  const rustSources = await readRustTree("apps", "core");
  const byPath = new Map(rustSources);
  assertAbsent(rustSources, [
    /\bFileWriteRequest\b/,
    /\bPromoteTempFileRequest\b/,
    /\bDeleteFileRequest\b/,
    /\bDownloadToTempRequest\b/,
    /\bwrite_file_atomically\b/,
    /\bpromote_temp_file\b/,
    /\bdelete_launcher_managed_file\b/,
    /\batomic_temp_path_for\b/,
    /\bAnchoredDirectory\b/,
    /\bpersistent_binding\b/,
  ]);

  const migratedMutationOwners = [
    "apps/desktop/src/commands/mod.rs",
    "apps/api/src/execution/anchored_record.rs",
    "apps/api/src/execution/file.rs",
    "apps/api/src/execution/persistence.rs",
    "apps/api/src/state/known_good.rs",
    "apps/api/src/state/reconciliation.rs",
    "apps/api/src/state/skins.rs",
    "core/minecraft/src/download/assets.rs",
    "core/minecraft/src/download/content_transfer.rs",
    "core/minecraft/src/download/transfer.rs",
    "core/minecraft/src/loaders/install_flight.rs",
    "core/minecraft/src/loaders/mod.rs",
    "core/minecraft/src/launch/mod.rs",
    "core/minecraft/src/managed_fs.rs",
    "core/minecraft/src/runtime/discovery.rs",
    "core/minecraft/src/runtime/file_download.rs",
    "core/minecraft/src/runtime/install.rs",
    "core/minecraft/src/version/mod.rs",
  ];
  for (const path of migratedMutationOwners) {
    const source = byPath.get(path);
    assert.ok(source, `missing migrated B02 owner ${path}`);
    assert.doesNotMatch(
      source,
      /\b(?:std::|tokio::)?fs::(?:write|rename|remove_file|remove_dir|remove_dir_all|create_dir|create_dir_all)\s*\(|\basync_fs::(?:write|rename|remove_file|remove_dir|remove_dir_all|create_dir|create_dir_all)\s*\(|\bFile::create\s*\(|OpenOptions::new\(\)[\s\S]{0,240}?\.write\(true\)/,
      `${path} retains ambient production mutation after capability migration`,
    );
    assert.doesNotMatch(
      source,
      /pub(?:\([^)]*\))?\s+(?:async\s+)?fn\s+[a-z_]*(?:write|create|promote|replace|remove|delete|install|repair)[a-z_]*\s*\([^)]*(?:&Path\b|PathBuf)/,
      `${path} retains a public raw-path mutation overload`,
    );
  }

  const persistence = byPath.get("apps/api/src/execution/persistence.rs");
  assert.ok(persistence, "missing persistence owner");
  assert.doesNotMatch(persistence, /\bnormalize_path\b|\bphysical_paths\b/);
  assert.doesNotMatch(persistence, /HashMap<PathBuf|destination:\s*PathBuf/);

  const skins = byPath.get("apps/api/src/state/skins.rs");
  assert.ok(skins, "missing skin state owner");
  assert.doesNotMatch(
    skins,
    /fn (?:write_atomic|park_file_for_delete|restore_parked_file|replace_file)\s*\(/,
  );

  assert.equal(await exists("core/minecraft/src/download/promotion.rs"), false);
  const downloadModule = byPath.get("core/minecraft/src/download/mod.rs");
  const transfer = byPath.get("core/minecraft/src/download/transfer.rs");
  assert.ok(downloadModule && transfer, "missing download owners");
  assert.doesNotMatch(downloadModule, /^mod promotion;$/m);
  assert.doesNotMatch(
    transfer,
    /promotion_backup_path|sweep_stale_promotion_backups/,
  );

  const contentTransfer = byPath.get(
    "core/minecraft/src/download/content_transfer.rs",
  );
  assert.ok(contentTransfer, "missing content transfer owner");
  assert.doesNotMatch(contentTransfer, /StagingDestination::Legacy/);
  assert.doesNotMatch(
    contentTransfer,
    /\bdownload_verified_content_to_staging\b|\bdownload_verified_content_to_staging_with_retry_delays\b|release_to_legacy_caller|validate_legacy_staging_destination/,
  );

  for (const path of [
    "apps/api/src/execution/anchored_record.rs",
    "core/minecraft/src/managed_fs.rs",
    "apps/api/src/state/launch_reports.rs",
  ]) {
    assert.doesNotMatch(
      byPath.get(path) ?? "",
      /rustix::fs|windows_sys::Win32::Storage::FileSystem|ntapi::ntioapi|std::os::unix::fs::MetadataExt/,
      `${path} retains a duplicate native filesystem implementation`,
    );
  }
});

terminalTest(
  "P01-B02 reset and loader authority are capability-bound and pathless",
  async () => {
    const [
      desktopCommands,
      configLibrary,
      configSources,
      paths,
      installFlight,
    ] = await Promise.all([
      read("apps/desktop/src/commands/mod.rs"),
      read("core/config/src/lib.rs"),
      readRustTree("core/config/src"),
      read("core/config/src/paths/mod.rs"),
      read("core/minecraft/src/loaders/install_flight.rs"),
    ]);
    const resetSources = [
      ["apps/desktop/src/commands/mod.rs", desktopCommands],
      ["core/config/src/lib.rs", configLibrary],
      ["core/config/src/paths/mod.rs", paths],
    ];
    assertAbsent(resetSources, [
      /\bTerminalResetScope\b/,
      /\bterminal_reset_scope\b/,
      /\bTerminalResetPlan\b/,
      /\bResetRootExpectation\b/,
      /\bResetRootIdentity\b/,
      /\bcapture_reset_root\b/,
      /\bopen_reset_root\b/,
      /\breset_root_identity_from_file\b/,
      /\bdelete_reset_root_off_runtime\b/,
      /\bdelete_reset_root\b/,
      /remove_dir_all\(/,
      /contains_resolved/,
      /canonicalize\(/,
      /current_exe\(/,
    ]);
    assert.match(desktopCommands, /begin_reset\(/);
    assert.match(desktopCommands, /relaunch|restart/i);
    const combinedConfig = configSources.map(([, source]) => source).join("\n");
    const appRootSession = implementationBlock(
      combinedConfig,
      "AppRootSession",
    );
    const drainDriver = functionBlocks(appRootSession).find(
      ({ name, source }) => {
        if (!/^pub (?:async )?fn/.test(source) || !/reset/.test(name))
          return false;
        const flow = uniqueReachableFunctions(combinedConfig, source);
        return /begin_reset\(/.test(flow) && /try_settle\(/.test(flow);
      },
    );
    assert.ok(
      drainDriver,
      "AppRootSession must drive nonblocking reset settlement after quiescence",
    );
    const drainFlow = uniqueReachableFunctions(
      combinedConfig,
      drainDriver.source,
    );
    assert.doesNotMatch(drainFlow, /\.wait(?:_while)?\(/);
    assert.match(drainFlow, /\b(?:loop|while)\b/);
    assert.match(
      drainFlow,
      /(?:yield_now|sleep)\s*\([^)]*\)\s*\.await/,
      "the reset owner must yield between nonblocking settlement probes",
    );
    const executableResetProof = functionBlocks(combinedConfig).find(
      ({ name, source }) =>
        /^pub fn/.test(source) &&
        /reset/.test(name) &&
        /root_session|begin_reset|reset_preflight/.test(source),
    );
    assert.ok(
      executableResetProof,
      "AppRootSession needs a physical executable-in-root reset refusal",
    );
    const appReset = functionBlock(desktopCommands, "app_reset");
    assert.match(
      appReset,
      new RegExp(`\\b${executableResetProof.name}\\(`),
      "desktop reset must consult the startup-captured executable proof",
    );
    const preflightCall = `${executableResetProof.name}(`;
    const stopIngress = appReset.match(
      /prepare_terminal_exit_with_api\(|(?:stop|quiesce|shutdown)[a-z_]*\(/,
    )?.[0];
    const drainReset = appReset.match(
      new RegExp(`\\b${escapeRegExp(drainDriver.name)}\\s*\\(`),
    )?.[0];
    const clear = appReset.match(
      /[a-z_]*(?:clear|remove)[a-z_]*(?:root|owned|children)[a-z_]*\(/,
    )?.[0];
    const release = appReset.match(
      /\.(?:release|finish)\(|release_[a-z_]*root[a-z_]*\(/,
    )?.[0];
    const relaunch = appReset.match(/request_restart\(|relaunch\(/)?.[0];
    assert.ok(
      stopIngress && drainReset && clear && release && relaunch,
      "desktop reset must own quiesce, reset, clear, release, and relaunch",
    );
    assertOrdered(
      appReset,
      preflightCall,
      stopIngress,
      "preflight before quiescence",
    );
    assertOrdered(
      appReset,
      stopIngress,
      drainReset,
      "quiescence before terminal drain",
    );
    assertOrdered(
      appReset,
      drainReset,
      clear,
      "reset authority before root clear",
    );
    assertOrdered(
      appReset,
      clear,
      release,
      "root clear before authority release",
    );
    assertOrdered(appReset, release, relaunch, "lease release before relaunch");
    assert.match(
      appReset.slice(appReset.indexOf(stopIngress), appReset.indexOf(relaunch)),
      /\?/,
      "any post-quiescence failure must stop before relaunch",
    );

    assert.match(installFlight, /DirectoryIdentity/);
    assert.match(installFlight, /PortablePathKey/);
    assert.doesNotMatch(installFlight, /namespace:\s*PathBuf|canonicalize\(/);
  },
);

terminalTest(
  "P01-B02 documents and retains only the one required park digest lifetime",
  async () => {
    const [library, namespaceAdr, guardianArchitecture] = await Promise.all([
      read("core/fs/src/lib.rs"),
      read("docs/adr/0004-performance-internal-namespace-ownership.md"),
      read("docs/GUARDIAN-ARCHITECTURE.md"),
    ]);
    const parkedFile = itemBlock(library, "struct", "ParkedFile");
    assert.doesNotMatch(
      parkedFile,
      /\b(?:ExpectedFileContent|FileRevision|sha_?256|digest)\b/i,
      "ParkedFile must not retain the digest after initial post-park verification",
    );
    for (const [path, source] of [
      [
        "docs/adr/0004-performance-internal-namespace-ownership.md",
        namespaceAdr,
      ],
      ["docs/GUARDIAN-ARCHITECTURE.md", guardianArchitecture],
    ]) {
      assert.doesNotMatch(
        source,
        /(?:final\s+settlement|final\s+cleanup|\bunlink\b)[^.]{0,600}?(?:re-?verif|rehash)[^.]{0,240}?(?:sha-?256|digest)/i,
        `${path} still claims final parked cleanup rehashes content`,
      );
      assert.doesNotMatch(
        source,
        /(?:re-?verif|rehash)[^.]{0,240}?(?:sha-?256|digest)[^.]{0,400}?(?:final\s+settlement|final\s+cleanup)/i,
        `${path} still retains digest verification through final park settlement`,
      );
    }
  },
);

terminalTest(
  "P01-B02 removes dependencies owned only by displaced adapters",
  async () => {
    const [performanceManifest, desktopManifest] = await Promise.all([
      read("core/performance/Cargo.toml"),
      read("apps/desktop/Cargo.toml"),
    ]);
    assert.doesNotMatch(performanceManifest, /^windows-sys\s*=/m);
    assert.doesNotMatch(desktopManifest, /^windows-sys\s*=/m);
  },
);
