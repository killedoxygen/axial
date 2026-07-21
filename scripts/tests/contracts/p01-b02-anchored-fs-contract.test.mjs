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

const reachableFunctionBlocks = (source, seed) => {
  const reachable = uniqueReachableFunctions(source, seed);
  return functionBlocks(source).filter(({ source: block }) =>
    reachable.includes(block),
  );
};

const conditionalBlocks = (source) => {
  const blocks = [];
  const marker = /\bif\b/g;
  for (let match = marker.exec(source); match; match = marker.exec(source)) {
    const openingBrace = source.indexOf("{", match.index + match[0].length);
    if (openingBrace === -1) continue;
    let depth = 0;
    for (let offset = openingBrace; offset < source.length; offset += 1) {
      if (source[offset] === "{") depth += 1;
      if (source[offset] === "}") depth -= 1;
      if (depth === 0) {
        blocks.push({
          condition: source.slice(match.index, openingBrace),
          body: source.slice(openingBrace, offset + 1),
          source: source.slice(match.index, offset + 1),
        });
        break;
      }
    }
  }
  return blocks;
};

const matchArmBlocks = (source, markerExpression) => {
  const blocks = [];
  const marker = new RegExp(
    markerExpression.source,
    markerExpression.flags.includes("g")
      ? markerExpression.flags
      : `${markerExpression.flags}g`,
  );
  for (let match = marker.exec(source); match; match = marker.exec(source)) {
    const arrow = source.indexOf("=>", match.index + match[0].length);
    if (arrow === -1) continue;
    const expressionStart = source.slice(arrow + 2).search(/\S/);
    if (expressionStart === -1) continue;
    const start = arrow + 2 + expressionStart;
    if (source[start] !== "{") {
      const end = source.indexOf(",", start);
      blocks.push({
        marker: match[0],
        body: source.slice(start, end === -1 ? source.length : end + 1),
      });
      continue;
    }
    let depth = 0;
    for (let offset = start; offset < source.length; offset += 1) {
      if (source[offset] === "{") depth += 1;
      if (source[offset] === "}") depth -= 1;
      if (depth === 0) {
        blocks.push({
          marker: match[0],
          body: source.slice(start, offset + 1),
        });
        break;
      }
    }
  }
  return blocks;
};

const bracedStatementBlocks = (source, markerExpression) => {
  const blocks = [];
  const marker = new RegExp(
    markerExpression.source,
    markerExpression.flags.includes("g")
      ? markerExpression.flags
      : `${markerExpression.flags}g`,
  );
  for (let match = marker.exec(source); match; match = marker.exec(source)) {
    const openingBrace = source.indexOf("{", match.index + match[0].length);
    if (openingBrace === -1) continue;
    let depth = 0;
    for (let offset = openingBrace; offset < source.length; offset += 1) {
      if (source[offset] === "{") depth += 1;
      if (source[offset] === "}") depth -= 1;
      if (depth === 0) {
        blocks.push({
          header: source.slice(match.index, openingBrace),
          body: source.slice(openingBrace, offset + 1),
          source: source.slice(match.index, offset + 1),
        });
        break;
      }
    }
  }
  return blocks;
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

const assertMustUse = (source, kind, name) => {
  const declaration = new RegExp(
    `((?:#\\[[^\\]]*\\]\\s*)*)(?:pub(?:\\([^)]*\\))?\\s+)?${kind}\\s+${escapeRegExp(name)}\\b`,
  ).exec(source);
  assert.ok(declaration, `missing ${kind} ${name}`);
  assert.match(
    declaration[1],
    /#\[must_use(?:\s*=\s*"[^"]*")?\]/,
    `${name} must warn when its ownership-bearing outcome is ignored`,
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
  assertMustUse(library, "enum", outcomeName);
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
  const failureName = outcome.match(
    /\bFailed\s*(?:\(\s*|\{[\s\S]{0,200}?\b(?:failure|refusal):\s*)([A-Za-z0-9_]+)\b/,
  )?.[1];
  assert.ok(failureName, `${label} needs a typed DRAINING settlement failure`);
  assert.notEqual(
    refusalName,
    failureName,
    `${label} LIVE start refusal and DRAINING settlement failure need distinct carriers`,
  );
  assertLinear(library, refusalName);
  const refusal = itemBlock(library, "struct", refusalName);
  assert.match(
    refusal,
    /(?:RootSession|Arc<CapabilityAuthority>)/,
    `${label} start refusal must retain the sole LIVE session`,
  );
  const refusalImplementation = implementationBlock(library, refusalName);
  assert.match(
    refusalImplementation,
    /pub fn retry\((?:mut )?self\)/,
    `${label} start refusal must expose consuming retry`,
  );
  const startRetry = functionBlock(refusalImplementation, "retry");
  const restartExpression =
    label === "reset" ? /\.begin_reset\s*\(/ : /\.revoke\s*\(/;
  assert.match(
    startRetry,
    restartExpression,
    `${label} LIVE refusal must retry terminal-drain admission`,
  );
  assert.doesNotMatch(
    startRetry,
    /\.try_settle\s*\(/,
    `${label} LIVE refusal cannot skip directly to DRAINING settlement`,
  );
  assert.match(
    start,
    new RegExp(
      `${escapeRegExp(outcomeName)}::Refused[\\s\\S]{0,200}${escapeRegExp(refusalName)}`,
    ),
    `${label} start refusal must use the LIVE refusal carrier`,
  );

  assertLinear(library, failureName);
  const failure = itemBlock(library, "struct", failureName);
  assert.match(
    failure,
    new RegExp(`\\b${escapeRegExp(pendingName)}\\b`),
    `${label} settlement failure must retain the DRAINING authority`,
  );
  const failureImplementation = implementationBlock(library, failureName);
  assert.match(
    failureImplementation,
    /pub fn retry\((?:mut )?self\)/,
    `${label} settlement failure must expose consuming retry`,
  );
  const settlementRetry = functionBlock(failureImplementation, "retry");
  assert.match(
    settlementRetry,
    /\.try_settle\s*\(/,
    `${label} DRAINING failure must retry settlement without restarting admission`,
  );
  assert.doesNotMatch(
    settlementRetry,
    restartExpression,
    `${label} DRAINING failure cannot retry LIVE terminal-drain admission`,
  );
  assert.match(
    settlementDirect,
    new RegExp(
      `${escapeRegExp(outcomeName)}::Failed[\\s\\S]{0,200}${escapeRegExp(failureName)}`,
    ),
    `${label} settlement errors must use the DRAINING failure carrier`,
  );

  return {
    outcomeName,
    pendingName,
    refusalName,
    failureName,
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
  assertMustUse(library, "enum", outcomeName);
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
  const constructionCarriers = new Map();
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const construction = itemBlock(source, "struct", constructionName);
    const carrierNames = [
      ...construction.matchAll(
        /Vec<(Root[A-Za-z0-9_]*(?:Created|Creation)[A-Za-z0-9_]*(?:State|Binding|Reservation)|CreatedRoot[A-Za-z0-9_]*(?:State|Binding|Reservation))>/g,
      ),
    ].map((match) => match[1]);
    assert.ok(
      carrierNames.length > 0,
      `${platformName} root construction must directly own its chronological creation state`,
    );
    const carrierSources = carrierNames.map((name) => {
      const kind = source.match(
        new RegExp(`\\b(struct|enum)\\s+${escapeRegExp(name)}\\b`),
      )?.[1];
      assert.ok(
        kind,
        `${platformName} root creation carrier ${name} is missing`,
      );
      return itemBlock(source, kind, name);
    });
    const nestedCarrierNames = carrierSources.flatMap((carrier) =>
      [
        ...carrier.matchAll(
          /\b(Root[A-Za-z0-9_]*(?:Created|Creation)[A-Za-z0-9_]*(?:Binding|Reservation))\b/g,
        ),
      ]
        .map((match) => match[1])
        .filter((name) => !carrierNames.includes(name)),
    );
    const nestedCarrierSources = [...new Set(nestedCarrierNames)].map(
      (name) => {
        const kind = source.match(
          new RegExp(`\\b(struct|enum)\\s+${escapeRegExp(name)}\\b`),
        )?.[1];
        assert.ok(
          kind,
          `${platformName} nested root creation carrier ${name} is missing`,
        );
        return itemBlock(source, kind, name);
      },
    );
    const retainedCreation = [...carrierSources, ...nestedCarrierSources].join(
      "\n",
    );
    assert.match(retainedCreation, /parent:\s*(?:Option<)?DirectoryHandle/);
    assert.match(retainedCreation, /name:\s*(?:OsString|LeafName)/);
    assert.match(
      retainedCreation,
      /DirectoryHandle/,
      `${platformName} created-root cleanup needs retained native authority`,
    );
    constructionCarriers.set(platformName, {
      construction,
      names: [...carrierNames, ...nestedCarrierNames],
      source: retainedCreation,
    });
  }

  const unixRootOpen = functionBlock(unix, "open_or_create_root");
  const unixRootCreation = reachableFunctionBlocks(unix, unixRootOpen);
  const unixRootOpenFlow = unixRootCreation
    .map(({ source }) => source)
    .join("\n");
  const mkdirBlock = unixRootCreation.find(({ source }) =>
    /mkdirat\s*\(/.test(source),
  );
  assert.ok(mkdirBlock, "Unix root creation needs a reachable mkdir effect");
  const mkdirCaller = unixRootCreation.find(({ source }) =>
    new RegExp(`\\b${escapeRegExp(mkdirBlock.name)}\\s*\\(`).test(
      source.slice(source.indexOf("{") + 1),
    ),
  );
  const mkdirCall = mkdirCaller?.source.match(
    new RegExp(`\\b${escapeRegExp(mkdirBlock.name)}\\s*\\(`),
  )?.[0];
  const recoveryReserve = mkdirCaller?.source.match(
    /\.try_reserve\s*\(\s*1\s*\)/,
  )?.[0];
  assert.ok(
    mkdirCaller && mkdirCall && recoveryReserve,
    "Unix root creation must reserve recovery-carrier capacity before its mkdir helper",
  );
  assertOrdered(
    mkdirCaller.source,
    recoveryReserve,
    mkdirCall,
    "Unix root recovery capacity before possible namespace effect",
  );
  const createSibling = mkdirBlock.source.match(/mkdirat\s*\(/)?.[0];
  const randomSibling = mkdirBlock.source.match(
    /\b(?:random|nonce|temporary|staging|candidate|sibling)[a-z_]*\s*(?:\(|:)|OsRng/,
  )?.[0];
  const creationErrorName = mkdirBlock.source
    .slice(0, mkdirBlock.source.indexOf("{"))
    .match(/Result\s*<[\s\S]*?,\s*([A-Z][A-Za-z0-9_]*Error)\s*>/)?.[1];
  assert.ok(
    creationErrorName,
    "Unix root creation needs a typed partial-effect error",
  );
  const creationError = itemBlock(unix, "enum", creationErrorName);
  const unixConstruction = constructionCarriers.get("Unix");
  const preOpenCarrierName =
    unixConstruction.names.find((name) => /Reservation/.test(name)) ??
    unixConstruction.names.find((name) => {
      const carrier = itemBlock(
        unix,
        unix.match(
          new RegExp(`\\b(struct|enum)\\s+${escapeRegExp(name)}\\b`),
        )[1],
        name,
      );
      return /(?:Unclassified|Unopened|PreOpen|Reservation)/.test(
        `${name}\n${carrier}`,
      );
    });
  assert.ok(
    preOpenCarrierName,
    "Unix RootConstruction must own CreatedUnclassified state until the created sibling is opened",
  );
  const preOpenCarrierKind = unix.match(
    new RegExp(`\\b(struct|enum)\\s+${escapeRegExp(preOpenCarrierName)}\\b`),
  )?.[1];
  const preOpenCarrier = itemBlock(
    unix,
    preOpenCarrierKind,
    preOpenCarrierName,
  );
  assert.match(preOpenCarrier, /parent:\s*(?:Option<)?DirectoryHandle/);
  assert.match(preOpenCarrier, /name:\s*(?:OsString|LeafName)/);
  const effectTransferName = unixConstruction.names.find((name) =>
    new RegExp(`\\b${escapeRegExp(name)}\\b`).test(creationError),
  );
  assert.ok(
    effectTransferName,
    "Unix root creation error must transfer state owned by RootConstruction",
  );
  const effectCarrierVariant = creationError.match(
    new RegExp(
      `\\b(?:Applied(?:Unverified)?|Unclassified|CreatedUnopened)\\s*(?:\\{[\\s\\S]{0,260}?\\b(?:binding|reservation|creation|retained):\\s*${escapeRegExp(effectTransferName)}\\b|\\(\\s*${escapeRegExp(effectTransferName)}\\s*\\))`,
    ),
  )?.[0];
  assert.ok(
    effectCarrierVariant,
    "Unix post-mkdir error must own the exact CreatedUnclassified carrier",
  );
  const beforeMkdir = mkdirBlock.source.slice(
    0,
    mkdirBlock.source.indexOf(createSibling),
  );
  const creationReservation = beforeMkdir.match(
    new RegExp(
      `\\b${escapeRegExp(preOpenCarrierName)}(?:::[A-Za-z][A-Za-z0-9_]*)?\\s*[\\{\\(]`,
    ),
  )?.[0];
  assert.ok(
    randomSibling && creationReservation,
    "Unix root mkdir must follow construction of its exact random-sibling reservation",
  );
  assertOrdered(
    mkdirBlock.source,
    randomSibling,
    creationReservation,
    "Unix random sibling selection before typed reservation",
  );
  assertOrdered(
    mkdirBlock.source,
    creationReservation,
    createSibling,
    "Unix typed root reservation before mkdir",
  );
  assert.match(
    unixRootOpenFlow,
    /openat\s*\(/,
    "Unix root construction must retain the created sibling through no-follow open",
  );
  const publicationBlock = unixRootCreation.find(({ source }) =>
    /RenameFlags::NOREPLACE/.test(source),
  );
  assert.ok(
    publicationBlock && /renameat_with\s*\(/.test(publicationBlock.source),
    "Unix root construction must atomically publish with NOREPLACE",
  );
  const openMarker = mkdirBlock.source.match(/openat\s*\(/)?.[0];
  const afterOpen = openMarker
    ? mkdirBlock.source.slice(mkdirBlock.source.indexOf(openMarker))
    : "";
  const reachableByName = new Map(
    unixRootCreation.map((block) => [block.name, block.source]),
  );
  const verifiedCleanupCall = [
    ...afterOpen.matchAll(/\b([a-z_][a-z0-9_]*)\s*\(/g),
  ]
    .map((match) => ({ match, source: reachableByName.get(match[1]) }))
    .find(
      ({ source }) =>
        source &&
        /unlinkat\s*\(/.test(source) &&
        /AtFlags::SYMLINK_NOFOLLOW|OFlags::NOFOLLOW/.test(source) &&
        /BindingState::Absent|\.is_none\s*\(/.test(source) &&
        /sync_directory\s*\(/.test(source),
    );
  const inlineVerifiedCleanup =
    /unlinkat\s*\(/.test(afterOpen) &&
    /AtFlags::SYMLINK_NOFOLLOW|OFlags::NOFOLLOW/.test(afterOpen) &&
    /BindingState::Absent|\.is_none\s*\(/.test(afterOpen) &&
    /sync_directory\s*\(/.test(afterOpen);
  assert.ok(
    effectCarrierVariant || verifiedCleanupCall || inlineVerifiedCleanup,
    "Unix mkdir-success/open-failure must retain CreatedUnclassified authority or prove exact cleanup",
  );
  assert.match(
    mkdirBlock.source.slice(mkdirBlock.source.indexOf(createSibling)),
    new RegExp(
      `${escapeRegExp(creationErrorName)}::(?:Applied(?:Unverified)?|Unclassified|CreatedUnopened)[\\s\\S]{0,260}?(?:${escapeRegExp(effectTransferName)}|binding|reservation|creation)`,
    ),
    "Unix post-mkdir failure must transfer the exact reservation instead of silently dropping it",
  );
  assert.doesNotMatch(
    mkdirBlock.source.slice(
      mkdirBlock.source.indexOf(createSibling),
      publicationBlock === mkdirBlock
        ? mkdirBlock.source.indexOf("RenameFlags::NOREPLACE")
        : undefined,
    ),
    /drop\([^)]*(?:reservation|binding|creation)[^)]*\)/,
    "Unix root reservation cannot be silently dropped before publication",
  );

  const unixCleanupRoot = functionBlock(unix, "cleanup_root_construction");
  const unixClassifyRoot = functionBlock(
    unix,
    "classify_or_settle_root_creation",
  );
  const unixRemovalProof = reachableFunctionBlocks(
    unix,
    `${unixCleanupRoot}\n${unixClassifyRoot}`,
  ).find(
    ({ source: block }) =>
      /(?:st_nlink|link_count|number_of_links)/.test(block) &&
      /(?:directory_identity|fstat)\s*\(/.test(block) &&
      /(?:==\s*0|\.is_zero\s*\(\s*\))/.test(block),
  );
  assert.ok(
    unixRemovalProof,
    "Unix retained-child proof must require exact identity and zero live links",
  );
  const unixRemovalCall = new RegExp(
    `\\b${escapeRegExp(unixRemovalProof.name)}\\s*\\([^)]*(?:child|creation|binding)[^)]*\\)`,
  );
  const unixAbsentCleanup = matchArmBlocks(
    unixCleanupRoot,
    /BindingState::Absent/,
  );
  assert.ok(
    unixAbsentCleanup.length > 0 &&
      unixAbsentCleanup.every(({ body }) => unixRemovalCall.test(body)),
    "Unix classified cleanup must prove the retained child has zero links before settling original-name absence",
  );
  const unixCleanupRefusal = conditionalBlocks(unixCleanupRoot).find(
    ({ condition, body }) =>
      (new RegExp(`!\\s*${unixRemovalCall.source}`).test(condition) ||
        /(?:st_nlink|link_count|number_of_links)[\s\S]{0,120}?(?:!=\s*0|>\s*0)/.test(
          condition,
        )) &&
      /Err\s*\(/.test(body),
  );
  assert.ok(
    unixCleanupRefusal,
    "Unix classified cleanup must retain its obligation whenever the retained child still has a link",
  );
  const unixClassifyRemoval = unixClassifyRoot.match(unixRemovalCall)?.[0];
  const unixClassifySuccess =
    unixClassifyRoot.match(/Ok\s*\(\s*None\s*\)/)?.[0];
  const unixClassifyRetained = unixClassifyRoot.match(
    /Ok\s*\(\s*Some\s*\(|Err\s*\(\s*\([^,]+,\s*creation\s*\)\s*\)/,
  )?.[0];
  assert.ok(
    unixClassifyRemoval && unixClassifySuccess && unixClassifyRetained,
    "Unix unclassified recovery must settle only a zero-link retained child and otherwise return retained state",
  );
  assertOrdered(
    unixClassifyRoot,
    unixClassifyRemoval,
    unixClassifySuccess,
    "Unix retained-child removal proof before unclassified settlement",
  );

  const windowsConstruction = constructionCarriers.get("Windows");
  const windowsCreatedName = windowsConstruction.names.find((name) =>
    /Created.*Binding|CreatedBinding/.test(name),
  );
  assert.ok(
    windowsCreatedName,
    "Windows needs a classified created-root carrier",
  );
  const windowsCreated = itemBlock(windows, "struct", windowsCreatedName);
  const deletionField = windowsCreated.match(
    /([a-z_]*(?:delet|creator|cleanup)[a-z_]*):\s*(?:Option<)?(?:[A-Z][A-Za-z0-9_]*(?:File|Handle|Capability)|File)>?/,
  )?.[1];
  assert.ok(
    deletionField,
    "Windows created-root state must retain its exact creator DELETE handle",
  );
  const windowsRootCreation = uniqueReachableFunctions(
    windows,
    functionBlock(windows, "open_or_create_root"),
  );
  assert.match(
    windowsRootCreation,
    new RegExp(
      `${escapeRegExp(deletionField)}\\s*(?::|=)\\s*(?:Some\\s*\\()?[^;\\n]*(?:creator|retained|created|child)`,
    ),
    "Windows must transfer the exact creator DELETE handle into retained created-root state",
  );
  const windowsCleanupRoot = functionBlock(
    windows,
    "cleanup_root_construction",
  );
  const windowsClassifyRoot = functionBlock(
    windows,
    "classify_or_settle_root_creation",
  );
  const windowsRootCleanup = uniqueReachableFunctions(
    windows,
    windowsCleanupRoot,
  );
  assert.match(
    windowsRootCleanup,
    new RegExp(`\\.${escapeRegExp(deletionField)}\\b`),
    "Windows root cleanup must consume the retained creator DELETE handle",
  );
  assert.match(
    windowsRootCleanup,
    new RegExp(
      `${escapeRegExp(deletionField)}[\\s\\S]{0,500}?object_identity\\s*\\([\\s\\S]{0,300}?(?:binding\\.identity|created_identity)`,
    ),
    "Windows root cleanup must validate the retained creator handle against the exact created identity",
  );
  const windowsRemovalProof = reachableFunctionBlocks(
    windows,
    `${windowsCleanupRoot}\n${windowsClassifyRoot}`,
  ).find(
    ({ source: block }) =>
      /(?:NumberOfLinks|number_of_links|link_count)/.test(block) &&
      /object_identity\s*\(/.test(block) &&
      /(?:==\s*0|\.is_zero\s*\(\s*\))/.test(block),
  );
  assert.ok(
    windowsRemovalProof,
    "Windows retained creator proof must require exact identity and zero live links",
  );
  const windowsRemovalCall = new RegExp(
    `\\b${escapeRegExp(windowsRemovalProof.name)}\\s*\\([^)]*(?:child|creator|${escapeRegExp(deletionField)}|binding)[^)]*\\)`,
  );
  const windowsAbsentCleanup = matchArmBlocks(
    windowsCleanupRoot,
    /BindingState::Absent/,
  );
  assert.ok(
    windowsAbsentCleanup.length > 0 &&
      windowsAbsentCleanup.every(
        ({ body }) =>
          /set_delete\s*\(/.test(body) && windowsRemovalCall.test(body),
      ),
    "Windows original-name absence must still delete and prove removal through the retained creator capability",
  );
  const windowsSetDelete = windowsCleanupRoot.match(/set_delete\s*\(/)?.[0];
  const windowsPostDeleteProof = windowsCleanupRoot
    .slice(windowsCleanupRoot.indexOf(windowsSetDelete))
    .match(windowsRemovalCall)?.[0];
  assert.ok(
    windowsSetDelete && windowsPostDeleteProof,
    "Windows cleanup must prove the retained created object has zero links after delete disposition",
  );
  assertOrdered(
    windowsCleanupRoot,
    windowsSetDelete,
    windowsPostDeleteProof,
    "Windows delete disposition before retained-object link-zero proof",
  );
  const windowsClassifyRemoval =
    windowsClassifyRoot.match(windowsRemovalCall)?.[0];
  const windowsClassifySuccess =
    windowsClassifyRoot.match(/Ok\s*\(\s*None\s*\)/)?.[0];
  const windowsClassifyRetained = windowsClassifyRoot.match(
    /Ok\s*\(\s*Some\s*\(|Err\s*\(\s*\([^,]+,\s*creation\s*\)\s*\)/,
  )?.[0];
  assert.ok(
    windowsClassifyRemoval && windowsClassifySuccess && windowsClassifyRetained,
    "Windows unclassified recovery must settle only a zero-link retained creator and otherwise return retained state",
  );
  assertOrdered(
    windowsClassifyRoot,
    windowsClassifyRemoval,
    windowsClassifySuccess,
    "Windows retained-creator removal proof before unclassified settlement",
  );

  const implementation = implementationBlock(library, obligationName);
  for (const operation of ["reconcile", "cleanup"]) {
    assert.match(
      implementation,
      new RegExp(`pub fn ${operation}\\((?:mut )?self(?:[,)]|\\s*\\))`),
      `${obligationName}::${operation} must consume the obligation`,
    );
  }
  const windowsLeaseAcquire = functionBlock(windows, "try_acquire_lease");
  const leaseOutcomeName = windowsLeaseAcquire
    .slice(0, windowsLeaseAcquire.indexOf("{"))
    .match(/->\s*([A-Z][A-Za-z0-9_]*Outcome)\b/)?.[1];
  assert.ok(
    leaseOutcomeName,
    "Windows lease acquisition needs an operation-specific effect outcome",
  );
  const windowsLeaseOutcome = itemBlock(windows, "enum", leaseOutcomeName);
  assert.match(windowsLeaseOutcome, /\bAcquired\b/);
  assert.match(windowsLeaseOutcome, /\bNoEffect\b/);
  assert.match(windowsLeaseOutcome, /\bAppliedUnverified\b/);
  assert.match(
    windowsLeaseAcquire,
    /FILE_OPEN_IF[\s\S]{0,1200}?(?:FILE_CREATED|creation|disposition)[\s\S]{0,1200}?(?:lease_acquisition_failure|AppliedUnverified)[\s\S]{0,500}?(?:opened\.handle|retained_handle|handle)/,
    "Windows post-FILE_OPEN_IF validation failure must retain the handle and exact creation disposition",
  );
  for (const [operation, settlement] of [
    ["reconcile", "reconcile_lease_acquisition"],
    ["cleanup", "cleanup_lease_acquisition"],
  ]) {
    const operationBody = functionBlock(implementation, operation);
    const leaseTake = operationBody.match(/self\.lease\.take\s*\(\s*\)/)?.[0];
    const settlementCall = operationBody.match(
      new RegExp(`platform::${settlement}\\s*\\(`),
    )?.[0];
    assert.ok(
      leaseTake && settlementCall,
      `${obligationName}::${operation} must consume its retained lease through typed ${settlement}`,
    );
    assertOrdered(
      operationBody,
      leaseTake,
      settlementCall,
      `${operation} retains lease ownership through typed settlement`,
    );
    const rootSettlement = operationBody.match(
      new RegExp(
        `platform::${operation === "cleanup" ? "cleanup" : "reconcile"}_root_construction\\s*\\(`,
      ),
    )?.[0];
    assert.ok(
      rootSettlement,
      `${obligationName}::${operation} must explicitly settle retained root construction`,
    );
    assertOrdered(
      operationBody,
      settlementCall,
      rootSettlement,
      `${operation} lease settlement before root construction settlement`,
    );
    assert.match(
      operationBody.slice(
        operationBody.indexOf(settlementCall),
        operationBody.indexOf(rootSettlement),
      ),
      /(?:match|if\s+let|\?)/,
      `${operation} must stop and return the lease obligation when typed settlement fails`,
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
  const acquireFlow = uniqueReachableFunctions(library, acquire);
  assertOrdered(
    acquireFlow,
    "open_or_create_root",
    "try_acquire_lease",
    "root creation before lease acquisition",
  );
  const leaseFailure = acquireFlow.slice(
    acquireFlow.indexOf("try_acquire_lease"),
  );
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
  const ancestryField = authority.match(
    /([a-z_]*(?:executable|process_image|image_ancestry)[a-z_]*):\s*platform::([A-Za-z0-9_]+)/i,
  );
  assert.ok(
    ancestryField,
    "the root authority must retain startup process-image ancestry",
  );
  const ancestryFieldName = ancestryField[1];
  const ancestryTypeName = ancestryField[2];
  const rootSession = implementationBlock(library, "RootSession");
  const acquire = functionBlock(rootSession, "acquire");
  const acquireBlocks = reachableFunctionBlocks(library, acquire);
  const rootOpenHost = acquireBlocks.find(({ source }) =>
    /open_or_create_root\s*\(/.test(source),
  );
  assert.ok(
    rootOpenHost,
    "root acquisition must reach physical root admission",
  );
  const beforeRootOpen = rootOpenHost.source.slice(
    0,
    rootOpenHost.source.indexOf("open_or_create_root"),
  );
  const captureCall = [...beforeRootOpen.matchAll(/\b([a-z_][a-z0-9_]*)\s*\(/g)]
    .map((match) => ({ marker: match[0], name: match[1] }))
    .find(({ name }) => {
      try {
        return /current_exe\s*\(/.test(
          uniqueReachableFunctions(library, functionBlock(library, name)),
        );
      } catch {
        return false;
      }
    });
  assert.ok(
    captureCall,
    "root acquisition must capture its physical process image before root mutation",
  );
  const startupCapture = `${bootstrap}\n${uniqueReachableFunctions(
    library,
    functionBlock(library, captureCall.name),
  )}`;
  assert.match(startupCapture, /current_exe\(/);
  assert.match(
    startupCapture,
    new RegExp(`\\b${escapeRegExp(ancestryTypeName)}\\b`),
    "startup capture must return the exact retained ancestry type",
  );
  assertOrdered(
    rootOpenHost.source,
    captureCall.marker,
    "open_or_create_root",
    "process-image capture before root mutation",
  );

  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const ancestry = itemBlock(source, "struct", ancestryTypeName);
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
    const capturePlatformCall = [
      ...startupCapture.matchAll(/platform::([a-z_][a-z0-9_]*)\s*\(/g),
    ]
      .map((match) => match[1])
      .find((name) => {
        try {
          return new RegExp(`\\b${escapeRegExp(ancestryTypeName)}\\b`).test(
            functionBlock(source, name).split("{")[0],
          );
        } catch {
          return false;
        }
      });
    assert.ok(
      capturePlatformCall,
      `${platformName} startup capture must call its retained native ancestry walker`,
    );
    const captureFlow = uniqueReachableFunctions(
      source,
      functionBlock(source, capturePlatformCall),
    );
    if (platformName === "Unix") {
      assert.match(captureFlow, /openat\(/);
      assert.match(captureFlow, /OFlags::NOFOLLOW|directory_flags\(\)/);
      assert.match(captureFlow, /fstat\(|(?:file|directory)_identity\(/);
      assert.doesNotMatch(
        captureFlow,
        /st_nlink\s*(?:==|!=|<=|>=|<|>)|(?:single|one)[a-z_]*link/,
        "Unix process-image ancestry is binding-specific and must allow hard-linked executables",
      );
    } else {
      assert.match(captureFlow, /FILE_OPEN_REPARSE_POINT/);
      assert.match(
        captureFlow,
        /OBJ_CASE_INSENSITIVE/,
        "the Windows process-image path walker must follow AppPaths case semantics",
      );
      assert.match(captureFlow, /object_identity\(|directory_identity\(/);
    }
  }

  const beginReset = functionBlock(rootSession, "begin_reset");
  const resetDrain = terminalDrainContract(
    library,
    beginReset,
    /AUTHORITY_RESETTING|\bResetting\b/,
    "reset",
  );
  const preDrainSection = resetDrain.startFlow.slice(
    0,
    resetDrain.startFlow.indexOf(resetDrain.drainingMarker),
  );
  const postDrainSection = resetDrain.settlement.slice(
    resetDrain.settlement.indexOf(resetDrain.settleActiveNonzero),
    resetDrain.settlement.indexOf(resetDrain.terminalMarker),
  );
  const resolveValidator = (source, name) => {
    try {
      return functionBlock(library, name);
    } catch {
      return functionBlock(source, name);
    }
  };
  const validatorName = [
    ...preDrainSection.matchAll(/\b([a-z_][a-z0-9_]*)\s*\(/g),
  ]
    .map((match) => match[1])
    .find(
      (name) =>
        new RegExp(`\\b${escapeRegExp(name)}\\s*\\(`).test(postDrainSection) &&
        (() => {
          try {
            const flow = uniqueReachableFunctions(
              `${library}\n${unix}`,
              resolveValidator(unix, name),
            );
            return (
              new RegExp(`\\b${escapeRegExp(ancestryFieldName)}\\b`).test(
                flow,
              ) && /(?:bindings|ancestors)/.test(flow)
            );
          } catch {
            return false;
          }
        })(),
    );
  assert.ok(
    validatorName,
    "the same typed physical-containment validator must run before DRAINING and after active operations reach zero",
  );
  const validatorCall = new RegExp(`\\b${escapeRegExp(validatorName)}\\s*\\(`);
  assert.match(preDrainSection, validatorCall);
  assert.match(postDrainSection, validatorCall);
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const containment = uniqueReachableFunctions(
      `${library}\n${source}`,
      resolveValidator(source, validatorName),
    );
    const ancestorLoops = bracedStatementBlocks(
      containment,
      /\bfor\s+(?:\([^)]*\)|[a-z_][a-z0-9_]*)\s+in\s+[^{};]*(?:bindings|ancestors)[^{};]*/gi,
    );
    const exactBindingLoop = ancestorLoops.find(({ header, body }) => {
      const binding = header.match(
        /\bfor\s+(?:\([^)]*,\s*)?([a-z_][a-z0-9_]*)\)?\s+in\b/i,
      )?.[1];
      if (!binding) return false;
      const flow = uniqueReachableFunctions(`${library}\n${source}`, body);
      return (
        new RegExp(`\\b${escapeRegExp(binding)}\\.parent\\b`).test(flow) &&
        new RegExp(`\\b${escapeRegExp(binding)}\\.name\\b`).test(flow) &&
        new RegExp(`\\b${escapeRegExp(binding)}\\.identity\\b`).test(flow) &&
        /BindingState::Exact|(?:==|!=)[\s\S]{0,120}?identity/.test(flow) &&
        /return\s+Err\s*\(|\.ok_or(?:_else)?\s*\(/.test(flow)
      );
    });
    const exactBindingIterator = [
      ...containment.matchAll(
        /(?:bindings|ancestors)\s*\.\s*iter\s*\(\s*\)\s*\.\s*(?:all|any|try_for_each)\s*\(\s*\|([a-z_][a-z0-9_]*)\|([\s\S]{0,1200}?)(?:\}\s*\)|\)\s*[;?])/gi,
      ),
    ].find((match) => {
      const binding = match[1];
      const flow = uniqueReachableFunctions(`${library}\n${source}`, match[2]);
      return (
        new RegExp(`\\b${escapeRegExp(binding)}\\.parent\\b`).test(flow) &&
        new RegExp(`\\b${escapeRegExp(binding)}\\.name\\b`).test(flow) &&
        new RegExp(`\\b${escapeRegExp(binding)}\\.identity\\b`).test(flow) &&
        /BindingState::Exact|(?:==|!=)[\s\S]{0,120}?identity/.test(flow)
      );
    });
    assert.ok(
      exactBindingLoop || exactBindingIterator,
      `${platformName} reset validator must causally fail unless every retained parent/name/identity binding remains exact`,
    );
    const rootIdentityLoop = ancestorLoops.find(({ header, body }) => {
      const binding = header.match(
        /\bfor\s+(?:\([^)]*,\s*)?([a-z_][a-z0-9_]*)\)?\s+in\b/i,
      )?.[1];
      return (
        binding &&
        new RegExp(
          `(?:root[a-z0-9_.]*identity|root_identity|directory_identity\\s*\\([^)]*root)[\\s\\S]{0,240}?(?:==|!=)[\\s\\S]{0,160}?${escapeRegExp(binding)}\\.identity|${escapeRegExp(binding)}\\.identity[\\s\\S]{0,160}?(?:==|!=)[\\s\\S]{0,240}?(?:root[a-z0-9_.]*identity|root_identity)`,
          "i",
        ).test(body) &&
        /return\s+Err\s*\(/.test(body)
      );
    });
    const rootIdentityIterator = conditionalBlocks(containment).find(
      ({ condition, body }) => {
        const iterator = condition.match(
          /(?:bindings|ancestors)\s*\.\s*iter\s*\(\s*\)\s*\.\s*(?:any|all)\s*\(\s*\|([a-z_][a-z0-9_]*)\|([\s\S]{0,900})\)/i,
        );
        if (!iterator || !/return\s+Err\s*\(/.test(body)) return false;
        const binding = iterator[1];
        return new RegExp(
          `(?:root[a-z0-9_.]*identity|root_identity|directory_identity\\s*\\([^)]*root)[\\s\\S]{0,240}?(?:==|!=)[\\s\\S]{0,160}?${escapeRegExp(binding)}\\.identity|${escapeRegExp(binding)}\\.identity[\\s\\S]{0,160}?(?:==|!=)[\\s\\S]{0,240}?(?:root[a-z0-9_.]*identity|root_identity)`,
          "i",
        ).test(iterator[2]);
      },
    );
    assert.ok(
      rootIdentityLoop || rootIdentityIterator,
      `${platformName} reset validator must fail closed when the physical root matches an exactly retained image ancestor`,
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
  const ntOpenEntry = functionBlock(
    windows,
    "nt_open_relative_with_attributes",
  );
  const ntOpen = reachableFunctionBlocks(windows, ntOpenEntry).find(
    ({ source }) => /NtCreateFile\s*\(/.test(source),
  )?.source;
  assert.ok(ntOpen, "relative Windows open must reach one NtCreateFile owner");
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

test("P01-B02 bounds every outstanding native effect with one shared permit", async () => {
  const library = await read("core/fs/src/lib.rs");
  const operationState = itemBlock(library, "struct", "OperationState");
  const effectField = operationState.match(
    /([a-z_]*(?:outstanding|retained)[a-z_]*effects[a-z_]*):\s*usize\b/,
  )?.[1];
  assert.ok(
    effectField,
    "operation state needs one shared count for every outstanding native effect",
  );
  const operationStateImplementation = implementationBlock(
    library,
    "OperationState",
  );
  const reserveEffect = functionBlocks(operationStateImplementation).find(
    ({ source }) =>
      new RegExp(`\\bself\\.${escapeRegExp(effectField)}\\b`).test(source) &&
      /checked_add\s*\(\s*1\s*\)/.test(source),
  );
  const releaseEffect = functionBlocks(operationStateImplementation).find(
    ({ source }) =>
      new RegExp(`\\bself\\.${escapeRegExp(effectField)}\\b`).test(source) &&
      /(?:-=\s*1|checked_sub\s*\(\s*1\s*\))/.test(source),
  );
  assert.ok(
    reserveEffect && releaseEffect,
    "the shared effect permit needs checked reserve and non-saturating release",
  );
  assert.match(
    reserveEffect.source,
    new RegExp(
      `${escapeRegExp(effectField)}[\\s\\S]{0,120}?(?:MAX_[A-Z0-9_]*EFFECT|limit|capacity)`,
      "i",
    ),
    "the shared effect permit must enforce one finite capacity",
  );
  assert.doesNotMatch(releaseEffect.source, /saturating_sub/);
  assert.match(
    releaseEffect.source.slice(0, releaseEffect.source.indexOf("{")),
    /(?:operation|permit):\s*&CapabilityOperation\b/,
    "the shared decrement must borrow a live operation until release is published",
  );

  const reservations = [
    ["stage creation", "reserve_stage_create", /platform::create_file\s*\(/],
    [
      "directory creation",
      "reserve_directory_create",
      /platform::create_directory\s*\(/,
    ],
    ["file park", "reserve_file_park", /platform::park_file_no_replace\s*\(/],
    [
      "directory park",
      "reserve_directory_park",
      /platform::park_directory_no_replace\s*\(/,
    ],
  ];
  for (const [label, reservationName, effectExpression] of reservations) {
    const reservation = functionBlock(library, reservationName);
    const reserveCall = reservation.match(
      new RegExp(`\\.${escapeRegExp(reserveEffect.name)}\\s*\\(`),
    )?.[0];
    const registryInsert = reservation.match(/\.insert\s*\(/)?.[0];
    assert.ok(
      reserveCall && registryInsert,
      `${label} must reserve the shared effect permit before registration`,
    );
    assertOrdered(
      reservation,
      reserveCall,
      registryInsert,
      `${label} shared permit before retained registry state`,
    );
    const operation = functionBlocks(library).find(
      ({ source }) =>
        new RegExp(`\\b${escapeRegExp(reservationName)}\\s*\\(`).test(source) &&
        effectExpression.test(source),
    );
    assert.ok(operation, `${label} needs a reachable native-effect owner`);
    assertOrdered(
      operation.source,
      reservationName,
      operation.source.match(effectExpression)[0],
      `${label} shared permit before native effect`,
    );
  }

  const stageRegistration = functionBlock(library, "register_stage_record");
  assert.match(
    stageRegistration,
    new RegExp(`\\b${escapeRegExp(effectField)}\\b`),
    "stage creation-to-stage transfer must preserve the existing shared permit",
  );
  assert.doesNotMatch(
    stageRegistration,
    new RegExp(
      `\\.${escapeRegExp(reserveEffect.name)}\\s*\\(|\\.${escapeRegExp(releaseEffect.name)}\\s*\\(`,
    ),
    "creation-to-stage transfer cannot double-count or briefly release its permit",
  );
  const finishStageCreate = functionBlock(library, "finish_stage_create");
  const registerStage = finishStageCreate.match(
    /register_stage_record\s*\(/,
  )?.[0];
  const transferStage = finishStageCreate.match(/\.transfer\s*\(/)?.[0];
  assert.ok(
    registerStage && transferStage,
    "classified stage creation must transfer its existing effect permit into the stage record",
  );
  assertOrdered(
    finishStageCreate,
    registerStage,
    transferStage,
    "new stage registration before reservation ownership transfer",
  );

  for (const takeName of [
    "take_stage_create",
    "take_directory_create",
    "take_file_park",
    "take_directory_park",
  ]) {
    const take = functionBlock(library, takeName);
    assert.doesNotMatch(
      take,
      new RegExp(
        `\\.${escapeRegExp(reserveEffect.name)}\\s*\\(|\\.${escapeRegExp(releaseEffect.name)}\\s*\\(`,
      ),
      `${takeName} must transfer an already-counted carrier without changing the shared total`,
    );
    const guardName = take
      .slice(0, take.indexOf("{"))
      .match(/Result\s*<\s*([A-Za-z0-9_]*Guard)\b/)?.[1];
    assert.ok(guardName, `${takeName} needs an owned checked-out guard`);
    const guardDrop = traitImplementationBlock(library, "Drop", guardName);
    assert.doesNotMatch(
      guardDrop,
      new RegExp(`\\.${escapeRegExp(releaseEffect.name)}\\s*\\(`),
      `${guardName} drop must retain or re-register unresolved effect ownership`,
    );
  }

  const releaseCallers = functionBlocks(library).filter(
    ({ name, source }) =>
      name !== releaseEffect.name &&
      new RegExp(`\\.${escapeRegExp(releaseEffect.name)}\\s*\\(`).test(source),
  );
  assert.ok(
    releaseCallers.length > 0,
    "shared effect permits need proof-only release paths",
  );
  for (const release of releaseCallers) {
    assert.match(
      release.source,
      new RegExp(
        `\\.${escapeRegExp(releaseEffect.name)}\\s*\\(\\s*&?(?:operation|permit)\\s*\\)`,
      ),
      `${release.name} must pass its live operation into the shared decrement`,
    );
    assert.match(
      release.source,
      /\.operations\s*\.\s*lock\s*\(\s*\)/,
      `${release.name} must release its effect under the terminal operations lock`,
    );
    assert.match(
      release.source,
      /Arc::ptr_eq\s*\([^;]*(?:operation|permit)[^;]*\)|(?:operation|permit)\.authority\.operations\s*\.\s*lock\s*\(\s*\)|\blet\s+(?:operation|permit)\s*=\s*[^;]*\.enter[a-z_]*\s*\(/,
      `${release.name} must bind the decrement lock to the same active authority`,
    );
    assert.doesNotMatch(
      release.source.slice(
        0,
        release.source.search(
          new RegExp(`\\.${escapeRegExp(releaseEffect.name)}\\s*\\(`),
        ),
      ),
      /drop\s*\(\s*(?:operation|permit)\s*\)/,
      `${release.name} cannot drop active authority before publishing effect release`,
    );
  }

  const beginDrain = functionBlock(library, "begin_terminal_drain");
  const drainSettlement = uniqueReachableFunctions(
    library,
    functionBlock(library, "try_finish_terminal_drain"),
  );
  assert.match(beginDrain, new RegExp(`\\b${escapeRegExp(effectField)}\\b`));
  assert.match(
    drainSettlement,
    new RegExp(`\\b${escapeRegExp(effectField)}\\b\\s*!=\\s*0`),
    "terminal publication must remain pending while any shared effect permit exists",
  );
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
  const parkKinds = [
    {
      kind: "file",
      tokenName: fileTokenName,
      recordName: fileRecordName,
      registryName: fileRegistryName,
    },
    {
      kind: "directory",
      tokenName: directoryTokenName,
      recordName: directoryRecordName,
      registryName: directoryRegistryName,
    },
  ];
  const checkedOutFields = new Map();
  const checkedOutPhases = new Map();
  for (const { kind, recordName } of parkKinds) {
    const checkedOutField = gateState.match(
      new RegExp(
        `([a-z_]*${kind}[a-z_]*park[a-z_]*(?:checked_out|in_flight|borrowed)[a-z_]*|[a-z_]*(?:checked_out|in_flight|borrowed)[a-z_]*${kind}[a-z_]*park[a-z_]*):\\s*usize\\b`,
      ),
    )?.[1];
    const record = itemBlock(library, "struct", recordName);
    const phaseName = record.match(/(?:phase|state):\s*([A-Za-z0-9_]+)\b/)?.[1];
    const checkedOutPhase =
      !checkedOutField && phaseName
        ? itemBlock(library, "enum", phaseName).match(
            /\b(?:CheckedOut|InFlight|Borrowed)\b/,
          )?.[0]
        : undefined;
    assert.ok(
      checkedOutField || checkedOutPhase,
      `${kind} parks checked out of their map need an exact counter or an in-registry phase`,
    );
    checkedOutFields.set(kind, checkedOutField);
    checkedOutPhases.set(kind, checkedOutPhase);
  }

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
    const reservationName = operation.source.match(
      new RegExp(
        `\\b((?:reserve|register)[a-z_]*${kind}[a-z_]*park|${kind}[a-z_]*park[a-z_]*(?:reserve|register))\\s*\\(`,
        "i",
      ),
    )?.[1];
    const reservation = reservationName ? `${reservationName}(` : undefined;
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
    const reservationFlow = functionBlock(library, reservationName);
    const liveReservation = reservationFlow.match(
      /AUTHORITY_LIVE|\bLive\b/,
    )?.[0];
    const registryInsert = reservationFlow.match(
      new RegExp(
        `\\b[a-z_]+\\.${kind === "file" ? escapeRegExp(fileRegistryName) : escapeRegExp(directoryRegistryName)}\\.insert\\s*\\(`,
      ),
    )?.[0];
    assert.ok(
      liveReservation && registryInsert,
      `${kind} park reservation must be LIVE-only and registered atomically`,
    );
    assertOrdered(
      reservationFlow,
      ".lock()",
      liveReservation,
      `${kind} park reservation gate lock before LIVE check`,
    );
    assertOrdered(
      reservationFlow,
      liveReservation,
      registryInsert,
      `${kind} LIVE check before park registration`,
    );
    const admissionSection = reservationFlow.slice(
      reservationFlow.indexOf(liveReservation),
      reservationFlow.indexOf(registryInsert),
    );
    assert.doesNotMatch(
      admissionSection,
      /drop\((?:state|gate|guard)\)/,
      `${kind} park reservation must keep the gate through registration`,
    );
    assert.match(
      flow,
      new RegExp(`\\b${escapeRegExp(recordName)}\\b`),
      `${kind} park reservation must retain its exact typed record`,
    );
  }

  for (const { kind, tokenName, registryName } of parkKinds) {
    const checkedOutField = checkedOutFields.get(kind);
    const checkedOutPhase = checkedOutPhases.get(kind);
    const checkOut = functionBlocks(library).find(({ name, source }) => {
      const header = source.slice(0, source.indexOf("{"));
      return (
        /(?:take|check_out|borrow)/.test(name) &&
        new RegExp(`&${escapeRegExp(tokenName)}\\b`).test(header) &&
        new RegExp(`\\b${escapeRegExp(registryName)}\\b`).test(source)
      );
    });
    assert.ok(checkOut, `missing ${kind} park registry checkout`);
    if (checkedOutField) {
      assert.match(
        checkOut.source,
        new RegExp(`\\b${escapeRegExp(checkedOutField)}\\b`),
        `${kind} park checkout must enter exact counter accounting`,
      );
      const checkedIncrement = checkOut.source.match(
        new RegExp(
          `\\b${escapeRegExp(checkedOutField)}\\b[\\s\\S]{0,160}?\\.checked_add\\(1\\)`,
        ),
      )?.[0];
      const registryRemoval = checkOut.source.match(
        new RegExp(`\\b${escapeRegExp(registryName)}\\s*\\.\\s*remove\\s*\\(`),
      )?.[0];
      assert.ok(
        checkedIncrement && registryRemoval,
        `${kind} park checkout needs overflow-safe counter and registry transfer`,
      );
      assertOrdered(
        checkOut.source,
        checkedIncrement,
        registryRemoval,
        `${kind} checked-out capacity reservation before registry removal`,
      );
      assert.match(
        checkOut.source.slice(checkOut.source.indexOf(registryRemoval)),
        new RegExp(`\\b${escapeRegExp(checkedOutField)}\\b\\s*=`),
        `${kind} park checkout must publish its reserved counter after registry removal`,
      );
      const guardName = checkOut.source
        .slice(0, checkOut.source.indexOf("{"))
        .match(/Result\s*<\s*([A-Za-z0-9_]*Guard)\s*>/)?.[1];
      assert.ok(guardName, `${kind} park checkout needs an owned record guard`);
      const guardImplementation = uniqueReachableFunctions(
        library,
        implementationBlock(library, guardName),
      );
      const guardDrop = uniqueReachableFunctions(
        library,
        traitImplementationBlock(library, "Drop", guardName),
      );
      assert.match(
        guardImplementation,
        new RegExp(`\\b${escapeRegExp(checkedOutField)}\\b`),
        `${kind} park guard disarm must leave exact counter accounting`,
      );
      assert.match(
        guardDrop,
        new RegExp(`\\b${escapeRegExp(checkedOutField)}\\b`),
        `${kind} park guard drop must leave exact counter accounting`,
      );
    } else {
      assert.match(
        checkOut.source,
        new RegExp(`\\b${escapeRegExp(checkedOutPhase)}\\b`),
        `${kind} park checkout must remain represented by its in-registry phase`,
      );
      assert.doesNotMatch(
        checkOut.source,
        new RegExp(`\\b${escapeRegExp(registryName)}\\s*\\.\\s*remove\\s*\\(`),
        `${kind} park checkout cannot disappear from registry accounting`,
      );
    }
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
  const beginDrain = functionBlock(library, "begin_terminal_drain");
  const livePhase = beginDrain.match(/AUTHORITY_LIVE|\bLive\b/)?.[0];
  const activeRefusal = beginDrain.match(
    /\b(?:active|in_flight|operations)\b[\s\S]{0,80}?(?:!=|>)\s*0/,
  )?.[0];
  const drainingAssignment = beginDrain.match(
    /\b(?:[a-z_]+\.)?(?:phase|state)\s*=\s*(?:AUTHORITY_DRAINING|Draining)\b/,
  )?.[0];
  assert.ok(
    livePhase && activeRefusal && drainingAssignment,
    "terminal drain needs a LIVE-only, active-zero transition into DRAINING",
  );
  assertOrdered(
    beginDrain,
    livePhase,
    activeRefusal,
    "terminal LIVE check before active-operation refusal",
  );
  assertOrdered(
    beginDrain,
    activeRefusal,
    drainingAssignment,
    "active-operation refusal before DRAINING",
  );
  assert.match(
    beginDrain.slice(
      beginDrain.indexOf(activeRefusal),
      beginDrain.indexOf(drainingAssignment),
    ),
    /return\s+Err\s*\(/,
    "active operations must refuse terminal start before DRAINING",
  );
  const beforeDraining = beginDrain.slice(
    0,
    beginDrain.indexOf(drainingAssignment),
  );
  assert.doesNotMatch(
    beforeDraining,
    /\b[a-z_]+\.(?:phase|state)\s*=(?!=)/,
    "terminal start refusal must leave the session LIVE",
  );
  const startRefusals = conditionalBlocks(beforeDraining).filter(({ body }) =>
    /return\s+Err\s*\(/.test(body),
  );
  for (const { kind, registryName } of parkKinds) {
    const nonAbandonedRefusal = startRefusals.find(({ condition }) => {
      const conditionFlow = uniqueReachableFunctions(library, condition);
      const directNegative = new RegExp(
        `\\b${escapeRegExp(registryName)}\\b[\\s\\S]{0,420}?(?:!=[\\s\\S]{0,80}?\\bAbandoned\\b|!\\s*matches!\\([\\s\\S]{0,160}?\\bAbandoned\\b)`,
      ).test(conditionFlow);
      const negatedAll =
        /!\s*[a-z_][a-z0-9_]*\s*\(/.test(condition) &&
        new RegExp(
          `\\b${escapeRegExp(registryName)}\\b[\\s\\S]{0,420}?\\.all\\([\\s\\S]{0,180}?\\bAbandoned\\b`,
        ).test(conditionFlow);
      return directNegative || negatedAll;
    });
    assert.ok(
      nonAbandonedRefusal,
      `${kind} terminal start must refuse every non-Abandoned park`,
    );
    const checkedOutField = checkedOutFields.get(kind);
    if (checkedOutField) {
      const checkedOutRefusal = startRefusals.find(({ condition }) =>
        new RegExp(
          `(?:\\b${escapeRegExp(checkedOutField)}\\b\\s*(?:!=|>)\\s*0|0\\s*<\\s*\\b${escapeRegExp(checkedOutField)}\\b)`,
        ).test(uniqueReachableFunctions(library, condition)),
      );
      assert.ok(
        checkedOutRefusal,
        `${kind} checked-out parks must causally return refusal before DRAINING`,
      );
    }
  }
  assert.match(
    beforeDraining,
    /return\s+Err\s*\(/,
    "terminal preconditions must return a refusal without changing phase",
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
  assert.match(drainFlow, /\bAbandoned\b/);
  assert.match(drainFlow, /(?:limit|max|capacity|TooMany|Overflow)/i);

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
        return /\b[a-z_]+\.(?:active|in_flight|operations)\s*(?:-=\s*1|=[\s\S]{0,100}?\.checked_sub\(1\))/.test(
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
    /\b[a-z_]+\.(?:active|in_flight|operations)\s*=[\s\S]{0,100}?\.checked_add\(1\)/,
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
    /\b[a-z_]+\.(?:active|in_flight|operations)\s*(?:-=\s*1|=[\s\S]{0,100}?\.checked_sub\(1\))/,
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
  const leaseAdmission = uniqueReachableFunctions(library, acquire);
  assertOrdered(
    leaseAdmission,
    "open_or_create_root",
    "try_acquire_lease",
    "root lease acquisition",
  );
  assert.match(leaseAdmission, /io::ErrorKind::WouldBlock/);
  assert.match(leaseAdmission, /RootSessionError::Busy/);
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
  const resetClear = functionBlocks(resetImplementation).find(
    ({ name, source }) =>
      /(?:clear|remove)/.test(name) && /^pub fn/.test(source),
  );
  assert.ok(resetClear, "reset authority needs a public retained-root clear");
  const platformClearName = resetClear.source.match(
    /platform::([a-z_]*(?:(?:clear|remove)[a-z_]*(?:root|children)|(?:root|children)[a-z_]*(?:clear|remove))[a-z_]*)\s*\(/,
  )?.[1];
  assert.ok(
    platformClearName,
    "reset clear must delegate to the anchored platform root capability",
  );
  for (const [platformName, source] of [
    ["Unix", unix],
    ["Windows", windows],
  ]) {
    const nativeClear = functionBlock(source, platformClearName);
    const nativeClearHeader = nativeClear.slice(0, nativeClear.indexOf("{"));
    const rootParameter = nativeClearHeader.match(
      /\b([a-z_][a-z0-9_]*):\s*&RootGuard\b/,
    )?.[1];
    assert.ok(
      rootParameter,
      `${platformName} reset clear must operate on the retained root guard`,
    );
    const reachableClearBlocks = reachableFunctionBlocks(source, nativeClear);
    const clearByName = new Map(
      reachableClearBlocks.map((block) => [block.name, block.source]),
    );
    const proveBudget = (kind, identifierExpression) => {
      for (const block of reachableClearBlocks) {
        for (const branch of conditionalBlocks(block.source)) {
          const identifiers = [
            ...branch.condition.matchAll(identifierExpression),
          ].map((match) => match[0]);
          for (const identifier of identifiers) {
            const escaped = escapeRegExp(identifier);
            const update = new RegExp(
              `(?:\\blet\\s+(?:mut\\s+)?${escaped}\\s*=\\s*[a-z_][a-z0-9_.]*${kind}[a-z0-9_]*\\.checked_(?:add|sub)\\s*\\(|\\b${escaped}\\.checked_(?:add|sub)\\s*\\(|\\b${escaped}\\s*(?:\\+=|-=)\\s*1)`,
              "i",
            ).test(block.source);
            const boundedError =
              /return\s+Err\s*\(|\.ok_or(?:_else)?\s*\(/.test(branch.body) &&
              new RegExp(
                `(?:${kind}|limit|budget|capacity|too_[a-z_]+)`,
                "i",
              ).test(branch.body);
            if (!update || !boundedError) continue;
            const propagated =
              block.name === platformClearName ||
              reachableClearBlocks.some(({ name, source: caller }) => {
                if (name === block.name) return false;
                return new RegExp(
                  `\\b${escapeRegExp(block.name)}\\s*\\([^;]*\\)(?:\\?|[\\s\\S]{0,120}?Err\\s*\\()`,
                ).test(caller);
              });
            if (propagated) return { block, branch, identifier };
          }
        }
      }
      assert.fail(
        `${platformName} reset clear ${kind} budget needs its own update, guard, and propagated bounded error`,
      );
    };
    const depthBudget = proveBudget("depth", /\b[a-z_]*depth[a-z_]*\b/gi);
    const entryBudget = proveBudget(
      "entr(?:y|ies)",
      /\b[a-z_]*(?:entry|entries|nodes|objects)[a-z_]*\b/gi,
    );
    assert.notEqual(
      depthBudget.identifier,
      entryBudget.identifier,
      `${platformName} reset clear depth and total-entry limits must be independent`,
    );

    const nativeBody = nativeClear.slice(nativeClear.indexOf("{") + 1);
    const enumerationExpression =
      /\b(?:entries|enumerate[a-z_]*|list_(?:directory|entries)[a-z_]*)\s*\(/;
    const deletionExpression =
      platformName === "Unix"
        ? /\bunlinkat\s*\(/
        : /\b(?:NtSetInformationFile|SetFileInformationByHandle)\s*\(|FileDispositionInfoEx/;
    const directCalls = [...nativeBody.matchAll(/\b([a-z_][a-z0-9_]*)\s*\(/g)]
      .map((match) => {
        const statementEnd = nativeBody.indexOf(";", match.index);
        return {
          index: match.index,
          marker: match[0],
          name: match[1],
          source: clearByName.get(match[1]),
          callSite: nativeBody.slice(
            match.index,
            statementEnd === -1 ? match.index + 500 : statementEnd + 1,
          ),
        };
      })
      .filter(({ source: block }) => block);
    const directDeletion = [
      ...nativeBody.matchAll(new RegExp(deletionExpression, "g")),
    ]
      .map((match) => ({ index: match.index, marker: match[0] }))
      .at(-1);
    const delegatedDeletion = directCalls
      .filter(({ source: block }) =>
        deletionExpression.test(uniqueReachableFunctions(source, block)),
      )
      .at(-1);
    const mutation = [directDeletion, delegatedDeletion]
      .filter(Boolean)
      .sort((left, right) => right.index - left.index)[0];
    assert.ok(
      mutation,
      `${platformName} reset clear needs a reachable mutation`,
    );

    const finalProofCall = directCalls.find(
      ({ index, source: block, callSite }) =>
        index > mutation.index &&
        (() => {
          const flow = uniqueReachableFunctions(source, block);
          const helperHeader = block.slice(0, block.indexOf("{"));
          const consumesRetainedRoot =
            new RegExp(`\\b${escapeRegExp(rootParameter)}\\b`).test(callSite) &&
            (/&RootGuard\b/.test(helperHeader) ||
              new RegExp(
                `\\b${escapeRegExp(rootParameter)}\\s*\\.\\s*handle\\b`,
              ).test(callSite));
          return (
            consumesRetainedRoot &&
            enumerationExpression.test(flow) &&
            /(?:\.complete\b|\bComplete\b)/.test(flow) &&
            /\.is_empty\s*\(|\.all\s*\(/.test(flow)
          );
        })(),
    );
    const inlineTail = nativeBody.slice(
      mutation.index + mutation.marker.length,
    );
    const inlineEnumeration = inlineTail.match(
      /\b(?:entries|enumerate[a-z_]*|list_(?:directory|entries)[a-z_]*)\s*\(([^;]*)\)/,
    );
    const rootedInlineEnumeration =
      inlineEnumeration &&
      new RegExp(`\\b${escapeRegExp(rootParameter)}\\b`).test(
        inlineEnumeration[1],
      )
        ? inlineEnumeration
        : undefined;
    const finalProofSource = finalProofCall
      ? finalProofCall.source
      : rootedInlineEnumeration
        ? inlineTail
        : "";
    const finalProofFlow = finalProofSource
      ? uniqueReachableFunctions(source, finalProofSource)
      : "";
    assert.ok(
      finalProofFlow,
      `${platformName} reset clear needs a distinct final retained-root re-enumeration`,
    );
    const proofRootParameter = finalProofCall
      ? finalProofSource
          .slice(0, finalProofSource.indexOf("{"))
          .match(
            /\b([a-z_][a-z0-9_]*):\s*&(?:RootGuard|DirectoryHandle)\b/,
          )?.[1]
      : rootParameter;
    assert.ok(
      proofRootParameter,
      `${platformName} final proof helper must retain its root argument`,
    );
    const finalListingAssignment = finalProofSource.match(
      /\blet\s+(?:mut\s+)?([a-z_][a-z0-9_]*)\s*=\s*(?:[a-z_][a-z0-9_]*::)*[a-z_]*(?:entries|enumerate|list_directory|list_entries)[a-z0-9_]*\s*\(([^;]*)\)\s*\?\s*;/i,
    );
    assert.ok(
      finalListingAssignment &&
        new RegExp(`\\b${escapeRegExp(proofRootParameter)}\\b`).test(
          finalListingAssignment[2],
        ),
      `${platformName} final listing variable must come from re-enumerating the retained root argument`,
    );
    const finalListing = finalListingAssignment[1];
    const finalConditions = conditionalBlocks(finalProofSource);
    const completeCondition = finalConditions.find(
      ({ condition, body }) =>
        new RegExp(
          `(?:!\\s*${escapeRegExp(finalListing)}\\.complete\\b|${escapeRegExp(finalListing)}\\.complete\\s*==\\s*false\\b|${escapeRegExp(finalListing)}\\.(?:state|completeness)\\s*!=\\s*(?:[A-Za-z0-9_]+::)*Complete\\b)`,
          "i",
        ).test(condition) && /\bErr\s*\(/.test(body),
    );
    const emptyCondition = finalConditions.find(
      ({ condition, body }) =>
        new RegExp(
          `(?:!\\s*${escapeRegExp(finalListing)}(?:\\.entries)?\\.is_empty\\s*\\(|${escapeRegExp(finalListing)}(?:\\.entries)?\\.is_empty\\s*\\(\\s*\\)\\s*==\\s*false\\b|${escapeRegExp(finalListing)}(?:\\.entries)?\\.len\\s*\\(\\s*\\)\\s*(?:!=|>)\\s*0\\b|!\\s*${escapeRegExp(finalListing)}\\.entries[\\s\\S]{0,300}?\\.all\\s*\\()`,
          "i",
        ).test(condition) && /\bErr\s*\(/.test(body),
    );
    assert.ok(
      completeCondition && emptyCondition,
      `${platformName} final root listing must independently require Complete and permitted-only emptiness`,
    );

    if (platformName === "Unix") {
      const reclassification = reachableClearBlocks.find(
        ({ source: block }) =>
          /AtFlags::SYMLINK_NOFOLLOW|OFlags::NOFOLLOW/.test(block) &&
          /(?:statat|openat|entry_kind|file_type)/.test(block),
      );
      assert.ok(
        reclassification,
        "Unix reset clear must reclassify every entry without following links before removal",
      );
      const classificationConsumer = /unlinkat\s*\(/.test(
        reclassification.source,
      )
        ? reclassification
        : reachableClearBlocks.find(
            ({ source: block }) =>
              new RegExp(
                `\\b${escapeRegExp(reclassification.name)}\\s*\\(`,
              ).test(block.slice(block.indexOf("{") + 1)) &&
              deletionExpression.test(uniqueReachableFunctions(source, block)),
          );
      assert.ok(
        classificationConsumer,
        "Unix reset clear must consume no-follow classification in its removal walk",
      );
      const noFollow = reclassification.source.match(
        /AtFlags::SYMLINK_NOFOLLOW|OFlags::NOFOLLOW/,
      )?.[0];
      const classificationMarker =
        classificationConsumer === reclassification
          ? noFollow
          : classificationConsumer.source.match(
              new RegExp(`\\b${escapeRegExp(reclassification.name)}\\s*\\(`),
            )?.[0];
      const directoryDecision = classificationConsumer.source.match(
        /\b[A-Za-z0-9_]*(?:Kind|Type|Class)::Directory\b|\.is_dir\s*\(/,
      )?.[0];
      assert.ok(
        noFollow && classificationMarker && directoryDecision,
        "Unix reset clear needs no-follow classification before directory recursion",
      );
      assertOrdered(
        classificationConsumer.source,
        classificationMarker,
        directoryDecision,
        "Unix no-follow reclassification before recursion decision",
      );
      assert.match(
        emptyCondition.condition,
        /\.is_empty\s*\(/,
        "Unix final complete root listing must be empty",
      );
    } else {
      const reclassification = reachableClearBlocks.find(
        ({ source: block }) =>
          /FILE_OPEN_REPARSE_POINT/.test(block) &&
          /FILE_ATTRIBUTE_REPARSE_POINT/.test(block) &&
          /object_identity\s*\(/.test(block),
      );
      assert.ok(
        reclassification,
        "Windows reset clear must re-open entries as reparse points and reclassify them authoritatively",
      );
      const classificationConsumer = reachableClearBlocks.find(
        ({ source: block }) =>
          (block === reclassification.source ||
            new RegExp(`\\b${escapeRegExp(reclassification.name)}\\s*\\(`).test(
              block.slice(block.indexOf("{") + 1),
            )) &&
          /\b[A-Za-z0-9_]*(?:Kind|Type|Class)::Directory\b|\.is_dir\s*\(/.test(
            block,
          ) &&
          deletionExpression.test(uniqueReachableFunctions(source, block)),
      );
      assert.ok(
        classificationConsumer,
        "Windows reset clear must consume authoritative reparse classification in its removal walk",
      );
      const classificationMarker =
        classificationConsumer.source === reclassification.source
          ? classificationConsumer.source.match(/FILE_OPEN_REPARSE_POINT/)?.[0]
          : classificationConsumer.source.match(
              new RegExp(`\\b${escapeRegExp(reclassification.name)}\\s*\\(`),
            )?.[0];
      const directoryDecision = classificationConsumer.source.match(
        /\b[A-Za-z0-9_]*(?:Kind|Type|Class)::Directory\b|\.is_dir\s*\(/,
      )?.[0];
      assert.ok(classificationMarker && directoryDecision);
      assertOrdered(
        classificationConsumer.source,
        classificationMarker,
        directoryDecision,
        "Windows reparse-point reclassification before recursion decision",
      );
      assert.match(
        nativeClear.slice(0, nativeClear.indexOf("{")),
        /&LeaseHandle\b/,
        "Windows reset clear needs the retained lease capability",
      );
      assert.match(
        finalProofFlow,
        /object_identity\s*\(/,
        "Windows final root proof must observe exact remaining identities",
      );
      assert.match(
        finalProofFlow,
        /lease\.identity/,
        "Windows reset clear must retain the exact lease identity, not only its name",
      );
      assert.match(
        finalProofFlow,
        /(?:(?:object_identity\s*\([^)]*\)|(?:entry|child|object)_identity)[\s\S]{0,240}?(?:==|!=)[\s\S]{0,120}?(?:lease\.identity|lease_identity|retained_lease_identity)|(?:lease\.identity|lease_identity|retained_lease_identity)[\s\S]{0,120}?(?:==|!=)[\s\S]{0,240}?(?:object_identity\s*\([^)]*\)|(?:entry|child|object)_identity))/,
        "Windows final complete root proof may retain only the exact lease identity",
      );
    }
  }
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
