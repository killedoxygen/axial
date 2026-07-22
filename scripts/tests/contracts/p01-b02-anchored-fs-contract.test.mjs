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

const implementationBlock = (source, name) =>
  implementationBlocks(source, name)[0];

const implementationBlocks = (source, name) => {
  const blocks = [];
  const marker = new RegExp(
    `impl\\s+${escapeRegExp(name)}(?:<[^>{}]+>)?\\s*\\{`,
    "g",
  );
  for (let match = marker.exec(source); match; match = marker.exec(source)) {
    const openingBrace = source.indexOf("{", match.index);
    let depth = 0;
    for (let offset = openingBrace; offset < source.length; offset += 1) {
      if (source[offset] === "{") depth += 1;
      if (source[offset] === "}") depth -= 1;
      if (depth === 0) {
        blocks.push(source.slice(match.index, offset + 1));
        marker.lastIndex = offset + 1;
        break;
      }
    }
  }
  assert.ok(blocks.length > 0, `missing impl ${name}`);
  return blocks;
};

const uniqueMethodBlock = (source, type, method) => {
  const methods = implementationBlocks(source, type)
    .flatMap((implementation) => functionBlocks(implementation))
    .filter(({ name }) => name === method);
  assert.equal(
    methods.length,
    1,
    `${type} must define exactly one ${method} method across all inherent impl blocks`,
  );
  return methods[0].source;
};

const callBlocks = (source, expression) => {
  const calls = [];
  const marker = new RegExp(expression.source, `${expression.flags}g`);
  for (let match = marker.exec(source); match; match = marker.exec(source)) {
    const opening = source.indexOf("(", match.index);
    let depth = 0;
    for (let offset = opening; offset < source.length; offset += 1) {
      if (source[offset] === "(") depth += 1;
      if (source[offset] === ")") depth -= 1;
      if (depth === 0) {
        calls.push({
          index: match.index,
          source: source.slice(match.index, offset + 1),
        });
        marker.lastIndex = offset + 1;
        break;
      }
    }
  }
  return calls;
};

const callArguments = (call) => {
  const body = call.slice(call.indexOf("(") + 1, -1);
  const argumentsList = [];
  let start = 0;
  let parentheses = 0;
  let brackets = 0;
  let braces = 0;
  for (let offset = 0; offset < body.length; offset += 1) {
    if (body[offset] === "(") parentheses += 1;
    if (body[offset] === ")") parentheses -= 1;
    if (body[offset] === "[") brackets += 1;
    if (body[offset] === "]") brackets -= 1;
    if (body[offset] === "{") braces += 1;
    if (body[offset] === "}") braces -= 1;
    if (
      body[offset] === "," &&
      parentheses === 0 &&
      brackets === 0 &&
      braces === 0
    ) {
      argumentsList.push(body.slice(start, offset).trim());
      start = offset + 1;
    }
  }
  const final = body.slice(start).trim();
  if (final) argumentsList.push(final);
  return argumentsList;
};

const assertStructFieldDataflow = (
  source,
  targetField,
  originExpression,
  label,
) => {
  const assigned = new RegExp(
    `\\b${escapeRegExp(targetField)}\\s*:\\s*([^,}\\n]+)`,
  ).exec(source);
  const shorthand = new RegExp(
    `\\b${escapeRegExp(targetField)}\\s*(?:,|\\})`,
  ).test(source);
  assert.ok(assigned || shorthand, `${label} must populate ${targetField}`);
  const value = assigned?.[1].trim() ?? targetField;
  if (originExpression.test(value)) return;
  const local = /^([a-z_][a-z0-9_]*)$/.exec(value)?.[1];
  assert.ok(local, `${label} must trace ${targetField} to its native field`);
  assert.match(
    source,
    new RegExp(
      `\\blet\\s+(?:mut\\s+)?${escapeRegExp(local)}(?:\\s*:[^=;]+)?\\s*=\\s*[^;]{0,240}${originExpression.source}`,
      originExpression.flags,
    ),
    `${label} must derive ${targetField} from its native field`,
  );
};

const assertReturnedStampConversion = (
  source,
  fields,
  scale,
  label,
  epochOffset,
) => {
  const carriers = fields.map((field) => {
    const local = source.match(
      new RegExp(
        `\\blet\\s+([a-z_][a-z0-9_]*)\\s*=\\s*[^;]{0,300}\\b${escapeRegExp(field)}\\b[^;]*;`,
      ),
    )?.[1];
    return local ?? field;
  });
  const calculations = [
    ...source.matchAll(
      /let\s+([a-z_][a-z0-9_]*)\s*=\s*([^;]*checked_mul\s*\([^;]*);/g,
    ),
  ];
  const calculation = calculations.find(
    (candidate) =>
      (fields.length === 1 || /checked_add\s*\(/.test(candidate[2])) &&
      carriers.every((carrier) =>
        new RegExp(`\\b${escapeRegExp(carrier)}\\b`).test(candidate[2]),
      ),
  );
  assert.ok(
    calculation,
    `${label} must use the selected native stamp fields in checked arithmetic`,
  );
  const formula =
    fields.length === 2
      ? new RegExp(
          `\\b${escapeRegExp(carriers[0])}\\b[^;]{0,200}checked_mul\\s*\\(\\s*${escapeRegExp(scale)}(?:_u64)?\\s*\\)[^;]{0,300}checked_add\\s*\\([^;]{0,160}\\b${escapeRegExp(carriers[1])}\\b`,
        )
      : epochOffset
        ? new RegExp(
            `\\b${escapeRegExp(carriers[0])}\\b[^;]{0,200}checked_sub\\s*\\(\\s*${escapeRegExp(epochOffset)}(?:_u64)?\\s*\\)[^;]{0,300}checked_mul\\s*\\(\\s*${escapeRegExp(scale)}(?:_u64)?\\s*\\)`,
          )
        : new RegExp(
            `\\b${escapeRegExp(carriers[0])}\\b[^;]{0,200}checked_mul\\s*\\(\\s*${escapeRegExp(scale)}(?:_u64)?\\s*\\)`,
          );
  assert.match(calculation[2], formula, `${label} uses the exact native units`);
  assert.match(
    source.slice(calculation.index + calculation[0].length),
    new RegExp(
      `(?:Ok\\s*\\(\\s*|return\\s+)${escapeRegExp(calculation[1])}\\b`,
    ),
    `${label} must return the checked stamp calculation`,
  );
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

const assertLiteralListingBound = (source, name, label) => {
  const declaration = new RegExp(
    `(?:pub(?:\\([^)]*\\))?\\s+)?const\\s+${escapeRegExp(name)}\\s*:\\s*usize\\s*=\\s*([0-9][0-9_]*)\\s*;`,
  ).exec(source);
  assert.ok(declaration, `${label} must resolve to a literal usize constant`);
  const value = Number(declaration[1].replaceAll("_", ""));
  assert.ok(
    Number.isSafeInteger(value) && value >= 1 && value <= 100_000,
    `${label} must resolve to a literal in 1..=100000`,
  );
};

const exactOperationsLock = (source, label) => {
  const header = source.slice(0, source.indexOf("{"));
  const operation = header.match(
    /([a-z_][a-z0-9_]*):\s*&CapabilityOperation\b/,
  )?.[1];
  const lock = source.match(
    /let\s+mut\s+([a-z_][a-z0-9_]*)\s*=\s*(?:self\s*\.\s*)?operations\s*\.\s*lock\s*\(/,
  );
  assert.ok(operation && lock, `${label} needs one live operation lock`);
  assert.equal(
    source.match(/operations\s*\.\s*lock\s*\(/g)?.length ?? 0,
    1,
    `${label} must acquire the operations lock once`,
  );
  assert.match(
    source,
    new RegExp(
      `Arc::ptr_eq\\s*\\([^)]*\\b${escapeRegExp(operation)}\\.authority\\b[^)]*\\)`,
    ),
    `${label} must use the caller's exact CapabilityOperation`,
  );
  return { lock, operation, state: lock[1] };
};

const assertPlatformLeafEquivalence = ({
  platform,
  workspaceManifest,
  fsManifest,
}) => {
  const shared = functionBlock(platform, "leaf_names_equal");
  assert.match(
    shared.slice(0, shared.indexOf("{")),
    /first:\s*&OsStr\s*,\s*second:\s*&OsStr\s*\)\s*->\s*bool/,
  );
  assert.match(shared, /if\s+first\s*==\s*second[\s\S]{0,80}return\s+true/);
  assertCountAtLeast(
    shared,
    /\.case_fold\s*\(/,
    2,
    "both UTF-8 leaves case-fold",
  );
  assertCountAtLeast(
    shared,
    /\.nfc\s*\(/,
    2,
    "both folded leaves normalize to NFC",
  );
  assert.match(
    shared,
    /first\.to_str\s*\(\s*\)[\s\S]{0,120}second\.to_str\s*\(\s*\)/,
  );
  assert.match(
    shared,
    /native::leaf_names_equal_native\s*\(\s*first\s*,\s*second\s*\)/,
  );
  assert.doesNotMatch(shared, /to_(?:ascii_)?lowercase|to_string_lossy/);

  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const unixNative = functionBlock(unix, "leaf_names_equal_native");
  assert.match(unixNative, /->\s*bool\s*\{\s*false\s*\}/);
  const windowsNative = functionBlock(windows, "leaf_names_equal_native");
  assertCountAtLeast(
    windowsNative,
    /encode_leaf\s*\(/,
    2,
    "both NT leaves encode",
  );
  assert.match(windowsNative, /\bUNICODE_STRING\b/);
  assert.match(
    windowsNative,
    /RtlEqualUnicodeString\s*\(\s*&first\s*,\s*&second\s*,\s*1\s*\)\s*!=\s*0/,
  );
  assert.doesNotMatch(windowsNative, /to_(?:ascii_)?lowercase|to_string_lossy/);
  assert.match(fsManifest, /^unicode-casefold\.workspace\s*=\s*true$/m);
  assert.match(fsManifest, /^unicode-normalization\.workspace\s*=\s*true$/m);
  assert.match(workspaceManifest, /^unicode-casefold\s*=\s*"=0\.2\.0"$/m);
  assert.match(workspaceManifest, /^unicode-normalization\s*=\s*"=0\.1\.25"$/m);
};

const assertAtomicParkRegistration = ({
  library,
  admission,
  registry,
  owners,
  keyType,
  ownerVariant,
  label,
}) => {
  const keyDeclaration = new RegExp(
    `((?:#\\[[^\\]]*\\]\\s*)*)struct\\s+${escapeRegExp(keyType)}\\b`,
  ).exec(library);
  assert.ok(keyDeclaration);
  assert.match(keyDeclaration[1], /#\[derive\([^\]]*\bEq\b[^\]]*\)\]/);
  assert.match(keyDeclaration[1], /#\[derive\([^\]]*\bHash\b[^\]]*\)\]/);
  const keyState = itemBlock(library, "struct", keyType);
  for (const [field, type] of [
    ["parent", "DirectoryIdentity"],
    ["original_name", "LeafName"],
    ["park_name", "LeafName"],
    ["identity", "platform::Identity"],
  ]) {
    assert.match(keyState, new RegExp(`\\b${field}:\\s*${escapeRegExp(type)}`));
  }
  const operationState = itemBlock(library, "struct", "OperationState");
  assert.match(
    operationState,
    new RegExp(
      `\\b${escapeRegExp(owners)}:[^\\n]+${escapeRegExp(keyType)}[^\\n]+ParkRegistryOwner`,
    ),
    `${label} ownership index must retain exact keys and token ids`,
  );
  const ownerState = itemBlock(library, "enum", "ParkRegistryOwner");
  assert.match(ownerState, /\bFile\s*\(\s*u64\s*\)/);
  assert.match(ownerState, /\bDirectory\s*\(\s*u64\s*\)/);
  const registrationOwner = reachableFunctionBlocks(library, admission).find(
    ({ source }) =>
      new RegExp(`${escapeRegExp(registry)}\\s*\\.\\s*insert\\s*\\(`).test(
        source,
      ) && /reserve_effect\s*\(\s*\)/.test(source),
  );
  assert.ok(
    registrationOwner,
    `${label} duplicate proof, effect reserve, and registration need one atomic owner`,
  );
  const registration = registrationOwner.source;
  const { lock, state } = exactOperationsLock(
    registration,
    `${label} registration`,
  );
  const liveAuthority = registration.match(
    new RegExp(
      `\\b${escapeRegExp(state)}\\s*\\.\\s*phase\\b[^;{}]{0,160}\\bAUTHORITY_LIVE\\b`,
    ),
  )?.[0];
  assert.ok(
    liveAuthority,
    `${label} registration must stay under live authority`,
  );

  const registryAccess = `${escapeRegExp(state)}\\s*\\.\\s*${escapeRegExp(registry)}`;
  const ownerAccess = `${escapeRegExp(state)}\\s*\\.\\s*${escapeRegExp(owners)}`;
  const keyCreation = callBlocks(
    registration,
    new RegExp(`${escapeRegExp(keyType)}::new\\s*\\(`),
  )[0];
  const key = keyCreation
    ? registration
        .slice(Math.max(0, keyCreation.index - 80), keyCreation.index)
        .match(/let\s+([a-z_][a-z0-9_]*)\s*=\s*$/)?.[1]
    : undefined;
  assert.ok(key && keyCreation, `${label} registration needs one exact key`);
  assert.deepEqual(
    callArguments(keyCreation.source),
    ["parent", "&original_name", "&park_name", "identity"],
    `${label} registration key must bind its exact admission inputs`,
  );
  const keyConstructor = uniqueMethodBlock(library, keyType, "new");
  assert.match(
    keyConstructor,
    /parent:\s*(?:parent|request\.file\.parent)\.inner\.identity/,
  );
  assert.match(
    keyConstructor,
    /original_name:\s*(?:original_name|request\.file\.name)(?:\.clone\s*\(\s*\))?/,
  );
  assert.match(keyConstructor, /park_name:\s*park_name(?:\.clone\s*\(\s*\))?/);
  assert.match(
    keyConstructor,
    /(?:identity:\s*(?:identity|request\.file\.identity|directory\.inner\.identity\.physical)|\bidentity\s*,)/,
  );
  const duplicate = conditionalBlocks(registration).find(
    ({ condition, body }) =>
      new RegExp(
        `${escapeRegExp(state)}\\s*\\.\\s*park_conflicts\\s*\\(\\s*&${escapeRegExp(key)}\\s*\\)`,
      ).test(condition) && /return\s+Err\s*\(/.test(body),
  );
  assert.ok(
    duplicate,
    `${label} registration must reject an exact record in every retained phase`,
  );
  const sharedConflict = uniqueMethodBlock(
    library,
    "OperationState",
    "park_conflicts",
  );
  assert.match(
    sharedConflict.slice(0, sharedConflict.indexOf("{")),
    new RegExp(`key:\\s*&${escapeRegExp(keyType)}\\b`),
  );
  assert.match(
    sharedConflict,
    new RegExp(
      `self\\s*\\.\\s*${escapeRegExp(owners)}\\s*\\.\\s*keys\\s*\\(\\s*\\)`,
    ),
    `${label} conflicts must include every checked-out file and directory park`,
  );
  const ownerConflict = callBlocks(sharedConflict, /\.\s*any\s*\(/)[0];
  const record = ownerConflict?.source.match(
    /\.\s*any\s*\(\s*\|\s*([a-z_][a-z0-9_]*)\b/,
  )?.[1];
  assert.ok(
    record && ownerConflict,
    `${label} duplicate proof needs one record predicate`,
  );
  assert.match(
    ownerConflict.source,
    new RegExp(
      `\\.\\s*any\\s*\\(\\s*\\|\\s*${escapeRegExp(record)}\\s*\\|\\s*${escapeRegExp(record)}\\.conflicts_with\\s*\\(\\s*key\\s*\\)\\s*\\)$`,
    ),
    `${label} unified ownership must use the exact key conflict predicate`,
  );
  const keyConflict = uniqueMethodBlock(library, keyType, "conflicts_with");
  const other = keyConflict
    .slice(0, keyConflict.indexOf("{"))
    .match(/([a-z_][a-z0-9_]*):\s*&Self\b/)?.[1];
  assert.ok(
    other,
    `${label} conflict predicate must compare another exact key`,
  );
  const leafClosure = bracedStatementBlocks(
    keyConflict,
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*\|\s*[a-z_][a-z0-9_]*:\s*&LeafName\s*,\s*[a-z_][a-z0-9_]*:\s*&LeafName\s*\|\s*/,
  )[0];
  const sameLeaf = leafClosure?.header.match(
    /let\s+([a-z_][a-z0-9_]*)\s*=/,
  )?.[1];
  assert.ok(
    sameLeaf && leafClosure,
    `${label} needs one platform leaf comparator`,
  );
  assert.match(
    leafClosure.source,
    /platform::leaf_names_equal\s*\([^;]+\.as_os_str\s*\(\s*\)[^;]+\.as_os_str\s*\(\s*\)\s*\)/,
  );
  const equal = (left, right) =>
    `(?:${left}\\s*==\\s*${right}|${right}\\s*==\\s*${left})`;
  const selfField = (field) => `self\\.${field}`;
  const otherField = (field) => `${escapeRegExp(other)}\\.${field}`;
  const sameLeafCall = (left, right) =>
    `(?:${escapeRegExp(sameLeaf)}\\s*\\(\\s*&${left}\\s*,\\s*&${right}\\s*\\)|${escapeRegExp(sameLeaf)}\\s*\\(\\s*&${right}\\s*,\\s*&${left}\\s*\\))`;
  const bindingConflict = `${equal(
    selfField("parent"),
    otherField("parent"),
  )}\\s*&&\\s*\\(\\s*${sameLeafCall(
    selfField("original_name"),
    otherField("original_name"),
  )}\\s*\\|\\|\\s*${sameLeafCall(
    selfField("original_name"),
    otherField("park_name"),
  )}\\s*\\|\\|\\s*${sameLeafCall(
    selfField("park_name"),
    otherField("original_name"),
  )}\\s*\\|\\|\\s*${sameLeafCall(
    selfField("park_name"),
    otherField("park_name"),
  )}\\s*\\)`;
  const exactConflict = new RegExp(
    `^\\s*(?:return\\s+)?(?:${bindingConflict}|\\(\\s*${bindingConflict}\\s*\\))\\s*\\|\\|\\s*${equal(selfField("identity"), otherField("identity"))}\\s*;?\\s*$`,
  );
  assert.match(
    keyConflict.slice(
      keyConflict.indexOf(";", keyConflict.indexOf(leafClosure.source)) + 1,
      keyConflict.lastIndexOf("}"),
    ),
    exactConflict,
    `${label} must reject original, parked, or physical identity overlap exactly`,
  );

  const reserve = registration.match(
    new RegExp(
      `\\b${escapeRegExp(state)}\\s*\\.\\s*reserve_effect\\s*\\(\\s*\\)`,
    ),
  )?.[0];
  const insert = registration.match(
    new RegExp(`${registryAccess}\\s*\\.\\s*insert\\s*\\(`),
  )?.[0];
  const own = registration.match(
    new RegExp(
      `${ownerAccess}\\s*\\.\\s*insert\\s*\\(\\s*${escapeRegExp(key)}\\s*,\\s*ParkRegistryOwner::${escapeRegExp(ownerVariant)}\\s*\\(\\s*id\\s*\\)\\s*\\)`,
    ),
  )?.[0];
  assert.ok(
    reserve && insert && own,
    `${label} reserve, record, and ownership must use one operations lock`,
  );
  const order = [liveAuthority, duplicate.source, reserve, insert, own].map(
    (marker) => registration.indexOf(marker),
  );
  assert.ok(
    order.every(
      (position, index) => index === 0 || order[index - 1] < position,
    ),
    `${label} must check overlap, reserve, then publish record and ownership`,
  );
  const criticalSection = registration.slice(
    lock.index,
    registration.indexOf(own) + own.length,
  );
  assert.doesNotMatch(
    criticalSection,
    new RegExp(`\\bdrop\\s*\\(\\s*${escapeRegExp(state)}\\s*\\)`),
    `${label} registration cannot drop and reacquire its authority lock`,
  );
  return registrationOwner;
};

const assertAdmissionRevalidationRollback = ({
  library,
  admission,
  registrationOwner,
  inlineRegistration,
  proof,
  requiredPreProof,
  registry,
  owners,
  ownerVariant,
  tokenType,
  label,
}) => {
  const registrationBoundary =
    registrationOwner.source === admission
      ? inlineRegistration.exec(admission)
      : new RegExp(`\\b${escapeRegExp(registrationOwner.name)}\\s*\\(`).exec(
          admission,
        );
  assert.ok(
    registrationBoundary,
    `${label} admission must directly publish through its atomic registration owner`,
  );
  const registrationPrefix = admission.slice(0, registrationBoundary.index);
  const registeredToken = registrationPrefix.match(
    /let\s+mut\s+([a-z_][a-z0-9_]*)\s*=\s*[^;]*$/,
  )?.[1];
  const admissionOperation = admission.match(
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*[^;]*\.enter\s*\(\s*\)/,
  )?.[1];
  const registrationCall = admission.slice(
    registrationBoundary.index,
    admission.indexOf(";", registrationBoundary.index),
  );
  assert.ok(
    registeredToken && admissionOperation,
    `${label} admission must retain its registration token and operation`,
  );
  assert.match(
    registrationCall,
    new RegExp(`&${escapeRegExp(admissionOperation)}\\b`),
    `${label} registration must consume the admission CapabilityOperation`,
  );

  const proofs = [
    ...admission.matchAll(
      new RegExp(
        proof.source,
        proof.flags.includes("g") ? proof.flags : `${proof.flags}g`,
      ),
    ),
  ];
  const before = proofs.find(
    (candidate) =>
      candidate.index < registrationBoundary.index &&
      (!requiredPreProof || requiredPreProof.test(candidate[0])),
  );
  const after = proofs.find(
    (candidate) => candidate.index > registrationBoundary.index,
  );
  assert.ok(
    before,
    `${label} admission must prove revision before publication`,
  );
  assert.ok(after, `${label} admission must revalidate after publication`);

  const directFailedProof = conditionalBlocks(admission).find((candidate) => {
    const start = admission.indexOf(candidate.source);
    const end = start + candidate.source.length;
    return (
      start !== -1 &&
      after.index >= start &&
      after.index < end &&
      /\bErr\b|\.is_err\s*\(\s*\)/.test(candidate.source)
    );
  });
  const postProofOwner = bracedStatementBlocks(
    admission,
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*\(\|\|\s*/,
  ).find((candidate) => {
    const start = admission.indexOf(candidate.source);
    return (
      after.index >= start && after.index < start + candidate.source.length
    );
  });
  const postProofResult = postProofOwner?.header.match(
    /let\s+([a-z_][a-z0-9_]*)\s*=/,
  )?.[1];
  const failedProof =
    directFailedProof ??
    (postProofResult
      ? conditionalBlocks(admission).find(({ condition }) =>
          new RegExp(
            `let\\s+Err\\s*\\(\\s*[a-z_][a-z0-9_]*\\s*\\)\\s*=\\s*${escapeRegExp(postProofResult)}\\b`,
          ).test(condition),
        )
      : undefined);
  assert.ok(
    failedProof,
    `${label} post-publication proof failure must have an explicit rollback branch`,
  );
  assert.match(
    failedProof.body,
    /\breturn\s+Err\s*\(|\bErr\s*\(/,
    `${label} admission must return the post-publication proof error`,
  );

  const rollbackOwner = reachableFunctionBlocks(library, failedProof.body).find(
    ({ source }) =>
      new RegExp(`${escapeRegExp(registry)}\\s*\\.\\s*remove\\s*\\(`).test(
        source,
      ) &&
      /release_effect\s*\(/.test(source) &&
      /\.armed\s*=\s*false/.test(source),
  );
  assert.ok(
    rollbackOwner,
    `${label} proof failure must remove registration, release its effect, and disarm its token in one rollback owner`,
  );
  const rollbackCall = failedProof.body.match(
    new RegExp(`\\b${escapeRegExp(rollbackOwner.name)}\\s*\\(`),
  )?.[0];
  const errorReturn = failedProof.body.match(/\breturn\s+Err\s*\(/)?.[0];
  assert.ok(
    rollbackCall && errorReturn,
    `${label} failure branch must invoke rollback before returning its proof error`,
  );
  assertOrdered(
    failedProof.body,
    rollbackCall,
    errorReturn,
    `${label} rollback before proof-error return`,
  );
  const rollbackInvocation = failedProof.body.slice(
    failedProof.body.indexOf(rollbackCall),
    failedProof.body.indexOf(";", failedProof.body.indexOf(rollbackCall)),
  );
  assert.match(
    rollbackInvocation,
    new RegExp(
      `&${escapeRegExp(admissionOperation)}\\b[\\s\\S]*&mut\\s+${escapeRegExp(registeredToken)}\\b|&mut\\s+${escapeRegExp(registeredToken)}\\b[\\s\\S]*&${escapeRegExp(admissionOperation)}\\b`,
    ),
    `${label} proof failure must roll back its exact registration token and operation`,
  );

  const rollbackHeader = rollbackOwner.source.slice(
    0,
    rollbackOwner.source.indexOf("{"),
  );
  const token = rollbackHeader.match(
    new RegExp(`([a-z_][a-z0-9_]*):\\s*&mut\\s*${escapeRegExp(tokenType)}\\b`),
  )?.[1];
  const { lock, operation, state } = exactOperationsLock(
    rollbackOwner.source,
    `${label} rollback`,
  );
  assert.ok(
    token,
    `${label} rollback must consume the exact registration token`,
  );
  const tokenGuard = conditionalBlocks(rollbackOwner.source).find(
    ({ condition, body }) => {
      const unarmed = new RegExp(`!${escapeRegExp(token)}\\.armed\\b`).exec(
        condition,
      );
      const foreign = new RegExp(
        `${escapeRegExp(token)}\\.authority\\.as_ptr\\s*\\(\\s*\\)\\s*!=\\s*Arc::as_ptr\\s*\\(\\s*self\\s*\\)`,
      ).exec(condition);
      if (!unarmed || !foreign || !/return\s+Err\s*\(/.test(body)) {
        return false;
      }
      const start = Math.min(unarmed.index, foreign.index);
      const end = Math.max(
        unarmed.index + unarmed[0].length,
        foreign.index + foreign[0].length,
      );
      return /\|\|/.test(condition.slice(start, end));
    },
  );
  assert.ok(
    tokenGuard,
    `${label} rollback must reject disarmed or cross-authority tokens`,
  );
  const removalExpression = new RegExp(
    `${escapeRegExp(state)}\\s*\\.\\s*${escapeRegExp(registry)}\\s*\\.\\s*remove\\s*\\(\\s*&${escapeRegExp(token)}\\.id\\s*\\)`,
  );
  const releaseExpression = new RegExp(
    `${escapeRegExp(state)}\\s*\\.\\s*release_effect\\s*\\(\\s*${escapeRegExp(operation)}\\s*\\)`,
  );
  const disarmExpression = new RegExp(
    `${escapeRegExp(token)}\\s*\\.\\s*armed\\s*=\\s*false`,
  );
  const removal = rollbackOwner.source.match(removalExpression)?.[0];
  const removalBinding = rollbackOwner.source.match(
    new RegExp(
      `let\\s+[_a-z][a-z0-9_]*\\s*=\\s*${removalExpression.source}[\\s\\S]{0,300}?\\.ok_or(?:_else)?\\s*\\([^;]{0,300}\\)\\s*\\?\\s*;`,
    ),
  )?.[0];
  const record = removalBinding?.match(/let\s+([_a-z][a-z0-9_]*)\s*=/)?.[1];
  const keyBinding = rollbackOwner.source.match(
    new RegExp(
      `let\\s+([_a-z][a-z0-9_]*)\\s*=\\s*${escapeRegExp(state)}\\s*\\.\\s*${escapeRegExp(registry)}\\s*\\.\\s*get\\s*\\(\\s*&${escapeRegExp(token)}\\.id\\s*\\)[\\s\\S]{0,360}?\\?\\s*\\.\\s*key\\s*\\(\\s*\\)\\s*;`,
    ),
  );
  const key = keyBinding?.[1];
  const ownershipGuard = key
    ? conditionalBlocks(rollbackOwner.source).find(
        ({ condition, body }) =>
          new RegExp(
            `${escapeRegExp(state)}\\s*\\.\\s*${escapeRegExp(owners)}\\s*\\.\\s*get\\s*\\(\\s*&${escapeRegExp(key)}\\s*\\)\\s*!=\\s*Some\\s*\\(\\s*&ParkRegistryOwner::${escapeRegExp(ownerVariant)}\\s*\\(\\s*${escapeRegExp(token)}\\.id\\s*\\)\\s*\\)`,
          ).test(condition) && /return\s+Err\s*\(/.test(body),
      )
    : undefined;
  const release = rollbackOwner.source.match(releaseExpression)?.[0];
  const ownershipRemoval = callBlocks(
    rollbackOwner.source,
    new RegExp(
      `${escapeRegExp(state)}\\s*\\.\\s*${escapeRegExp(owners)}\\s*\\.\\s*remove\\s*\\(`,
    ),
  )[0];
  const disarm = rollbackOwner.source.match(disarmExpression)?.[0];
  assert.ok(
    removal &&
      removalBinding &&
      record &&
      keyBinding &&
      ownershipGuard &&
      ownershipRemoval &&
      release &&
      disarm,
    `${label} rollback must prevalidate and bind exact record ownership`,
  );
  assert.deepEqual(
    callArguments(ownershipRemoval.source),
    [`&${record}.key()`],
    `${label} rollback must remove the ownership key derived from its removed record`,
  );
  const ownershipBinding = rollbackOwner.source.slice(
    rollbackOwner.source.lastIndexOf("let ", ownershipRemoval.index),
    rollbackOwner.source.indexOf(";", ownershipRemoval.index) + 1,
  );
  const removedOwner = ownershipBinding.match(
    /let\s+([_a-z][a-z0-9_]*)\s*=/,
  )?.[1];
  assert.match(
    ownershipBinding,
    /\.\s*expect\s*\(/,
    `${label} prevalidated ownership removal must remain mandatory`,
  );
  assert.ok(removedOwner);
  const ownershipProof = rollbackOwner.source.slice(
    rollbackOwner.source.indexOf(";", ownershipRemoval.index) + 1,
    rollbackOwner.source.indexOf(release),
  );
  assert.match(
    ownershipProof,
    new RegExp(
      `assert_eq!\\s*\\(\\s*${escapeRegExp(removedOwner)}\\s*,\\s*ParkRegistryOwner::${escapeRegExp(ownerVariant)}\\s*\\(\\s*${escapeRegExp(token)}\\.id\\s*\\)\\s*\\)`,
    ),
    `${label} rollback must prove the removed owner is its exact token id`,
  );
  const rollbackOrder = [
    keyBinding[0],
    ownershipGuard.source,
    removal,
    ownershipRemoval.source,
    release,
    disarm,
  ].map((marker) => rollbackOwner.source.indexOf(marker));
  assert.ok(
    rollbackOrder.every(
      (position, index) => index === 0 || rollbackOrder[index - 1] < position,
    ),
    `${label} rollback must remove record/ownership, release, then disarm`,
  );
  const rollbackCriticalSection = rollbackOwner.source.slice(
    lock.index,
    rollbackOwner.source.indexOf(disarm) + disarm.length,
  );
  assert.doesNotMatch(
    rollbackCriticalSection,
    new RegExp(`\\bdrop\\s*\\(\\s*${escapeRegExp(state)}\\s*\\)`),
    `${label} rollback cannot drop and reacquire its operations lock`,
  );
};

const assertPersistentParkOwnership = ({
  library,
  registry,
  owners,
  ownerVariant,
  tokenType,
  takeMethod,
  guardType,
  label,
}) => {
  const take = uniqueMethodBlock(library, "CapabilityAuthority", takeMethod);
  assert.match(take, new RegExp(`${escapeRegExp(registry)}\\s*\\.\\s*remove`));
  assert.doesNotMatch(
    take,
    new RegExp(`${escapeRegExp(owners)}\\s*\\.\\s*(?:remove|clear)`),
    `${label} ownership must survive checked-out operational records`,
  );
  const drop = traitImplementationBlock(library, "Drop", guardType);
  assert.match(drop, new RegExp(`${escapeRegExp(registry)}\\s*\\.\\s*insert`));
  assert.doesNotMatch(
    drop,
    new RegExp(`${escapeRegExp(owners)}\\s*\\.\\s*(?:remove|clear)`),
    `${label} guard Drop must preserve ownership while reinserting`,
  );

  const disarm = uniqueMethodBlock(library, guardType, "disarm");
  const disarmHeader = disarm.slice(0, disarm.indexOf("{"));
  const token = disarmHeader.match(
    new RegExp(`([a-z_][a-z0-9_]*):\\s*&mut\\s*${escapeRegExp(tokenType)}\\b`),
  )?.[1];
  assert.ok(token, `${label} disarm must consume its exact registry token`);
  const record = disarm.match(
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*self\s*\.\s*record\s*\.\s*take\s*\(\s*\)/,
  )?.[1];
  const key = record
    ? disarm.match(
        new RegExp(
          `let\\s+([a-z_][a-z0-9_]*)\\s*=\\s*${escapeRegExp(record)}\\s*\\.\\s*key\\s*\\(\\s*\\)\\s*;`,
        ),
      )?.[1]
    : undefined;
  assert.ok(record && key, `${label} disarm must retain its exact record key`);
  const ownershipRemoval = callBlocks(
    disarm,
    new RegExp(`${escapeRegExp(owners)}\\s*\\.\\s*remove\\s*\\(`),
  )[0];
  const release = disarm.match(/release_effect\s*\(/)?.[0];
  assert.ok(ownershipRemoval && release);
  assert.deepEqual(
    callArguments(ownershipRemoval.source),
    [`&${key}`],
    `${label} disarm must remove the key of its checked-out record`,
  );
  const exactRemoval = disarm.slice(
    disarm.lastIndexOf("assert_eq!", ownershipRemoval.index),
    disarm.indexOf(";", ownershipRemoval.index) + 1,
  );
  assert.match(
    exactRemoval,
    new RegExp(
      `assert_eq!\\s*\\([\\s\\S]{0,240}Some\\s*\\(\\s*ParkRegistryOwner::${escapeRegExp(ownerVariant)}\\s*\\(\\s*self\\.id\\s*\\)\\s*\\)\\s*\\)`,
    ),
    `${label} disarm must prove the key belonged to its checked-out record`,
  );
  const tokenDisarm = disarm.match(
    new RegExp(`${escapeRegExp(token)}\\s*\\.\\s*armed\\s*=\\s*false`),
  )?.[0];
  assert.ok(tokenDisarm, `${label} disarm must consume its exact token`);
  assertOrdered(
    disarm,
    ownershipRemoval.source,
    release,
    `${label} exact ownership removal before effect release`,
  );
  assertOrdered(
    disarm,
    release,
    tokenDisarm,
    `${label} release before token disarm`,
  );
  assert.equal(
    library.match(
      new RegExp(`${escapeRegExp(owners)}\\s*\\.\\s*remove\\s*\\(`, "g"),
    )?.length ?? 0,
    4,
    `${label} ownership may be removed only by exact disarm or rollback`,
  );
  assert.doesNotMatch(
    library,
    new RegExp(`${escapeRegExp(owners)}\\s*\\.\\s*clear\\s*\\(`),
    `${label} ownership cannot be cleared outside exact settlement`,
  );
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
    assertMustUse(library, "enum", outcomeName);
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
    assertMustUse(library, "struct", obligation);
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

  for (const resolution of [
    "DirectoryCreateResolution",
    "FileCreateResolution",
    "FilePromotionResolution",
    "FileParkResolution",
    "FileRemovalResolution",
    "FileRestoreResolution",
    "DirectoryParkResolution",
    "DirectoryRemovalResolution",
    "DirectoryRestoreResolution",
    "FileReplaceResolution",
    "StageDiscardOutcome",
    "StageDiscardResolution",
    "RootClearOutcome",
  ]) {
    assertMustUse(library, "enum", resolution);
  }
  for (const carrier of [
    "StageDiscardObligation",
    "StageSealFailure",
    "RootClearFailure",
  ]) {
    assertLinear(library, carrier);
    assertMustUse(library, "struct", carrier);
  }
  for (const carrier of [
    "RootSession",
    "RootSessionAcquireObligation",
    "RootResetAuthority",
    "RootRevokeDrain",
    "RootRevokeRecovery",
    "RootRevokeStartFailure",
    "RootRevokeDrainFailure",
    "ResetDrainAuthority",
    "ResetDrainRecovery",
    "ResetStartFailure",
    "ResetDrainFailure",
  ]) {
    assertLinear(library, carrier);
    assertMustUse(library, "struct", carrier);
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

test("P01-B02 preserves Unix mkdir effects that never yielded retained identity", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const preserveMarker =
    /(?:CreatedUnclassified|PreserveOnly|PreservedResidue|UnclassifiedResidue|NameOnlyResidue)/;

  const createDirectory = functionBlock(unix, "create_directory");
  const mkdir = createDirectory.match(/mkdirat\s*\(/)?.[0];
  const open = createDirectory.match(/openat\s*\(/)?.[0];
  assert.ok(
    mkdir && open,
    "Unix managed directory create needs mkdir then no-follow open",
  );
  assertOrdered(
    createDirectory,
    mkdir,
    open,
    "Unix managed mkdir before retained-handle admission",
  );
  const managedOpenFailure = matchArmBlocks(
    createDirectory.slice(createDirectory.indexOf(open)),
    /Err\s*\([^)]*\)/,
  ).find(({ body }) => /CreateDirectoryError/.test(body));
  assert.ok(
    managedOpenFailure,
    "Unix managed mkdir needs an explicit post-mkdir open-failure arm",
  );
  assert.match(
    managedOpenFailure.body,
    preserveMarker,
    "mkdir success without a retained handle must become typed preserve-only residue",
  );
  assert.doesNotMatch(
    managedOpenFailure.body,
    /entry_observation|statat\s*\(|directory_identity\s*\(|fstat\s*\(/,
    "a post-hoc name observation cannot identify the directory created by mkdir",
  );

  const directoryRecordName = library.match(
    /struct ([A-Za-z0-9_]*DirectoryCreate[A-Za-z0-9_]*(?:Record|Reservation)[A-Za-z0-9_]*)\s*\{/,
  )?.[1];
  assert.ok(
    directoryRecordName,
    "managed directory residue needs a typed record",
  );
  const directoryRecord = itemBlock(library, "struct", directoryRecordName);
  const directoryPhaseName = directoryRecord.match(
    /(?:phase|state):\s*([A-Za-z0-9_]+)\b/,
  )?.[1];
  assert.ok(
    directoryPhaseName,
    "managed directory residue needs typed phase state",
  );
  const directoryPhase = itemBlock(library, "enum", directoryPhaseName);
  const directoryResidue = directoryPhase.match(preserveMarker)?.[0];
  assert.ok(
    directoryResidue,
    "managed directory registry must distinguish name-only residue from identity-owned creation",
  );

  const directoryOutcome = itemBlock(library, "enum", "DirectoryCreateOutcome");
  const preservationName = directoryOutcome.match(
    /\bCreatedUnclassified\s*\{[\s\S]{0,320}?\b(?:preservation|residue|authority):\s*([A-Za-z0-9_]+)\b/,
  )?.[1];
  assert.ok(
    preservationName,
    "managed mkdir ambiguity must return a dedicated preservation carrier",
  );
  assertLinear(library, preservationName);
  assertMustUse(library, "struct", preservationName);
  const preservation = itemBlock(library, "struct", preservationName);
  assert.match(
    preservation,
    /(?:Token|Reservation|CapabilityAuthority)/,
    "managed directory preservation must retain its registered effect ownership",
  );
  const preservationImplementation = implementationBlock(
    library,
    preservationName,
  );
  const acknowledgeDirectory = functionBlocks(preservationImplementation).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:acknowledge|preserve)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    acknowledgeDirectory,
    "a managed directory preservation carrier must expose consuming acknowledgement",
  );
  const directoryAcknowledgeFlow = uniqueReachableFunctions(
    library,
    acknowledgeDirectory.source,
  );
  assert.match(directoryAcknowledgeFlow, preserveMarker);
  assert.match(
    directoryAcknowledgeFlow,
    /release_effect\s*\(|\.disarm\s*\([^;]*(?:token|operation)/,
    "preservation acknowledgement must consume the registered effect permit",
  );
  assert.doesNotMatch(
    directoryAcknowledgeFlow,
    /remove_parked_directory\s*\(|finish_directory_create\s*\(|DirectoryCreateResolution::Created/,
    "preserve acknowledgement cannot delete or mint a directory capability",
  );
  const directoryAckReturn = acknowledgeDirectory.source
    .slice(0, acknowledgeDirectory.source.indexOf("{"))
    .match(
      /->\s*(?:(?:io::)?Result\s*<\s*)?([A-Za-z0-9_]+(?:Outcome|Resolution))\b/,
    )?.[1];
  if (directoryAckReturn) {
    assertMustUse(library, "enum", directoryAckReturn);
  }

  const revokeRecoveryItem = itemBlock(library, "struct", "RootRevokeRecovery");
  const revokeRecovery = implementationBlock(library, "RootRevokeRecovery");
  const acknowledgeDroppedDirectory = functionBlocks(revokeRecovery).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:acknowledge|preserve)/.test(name) &&
      /(?:preserv|residue|create|unclassified)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    acknowledgeDroppedDirectory,
    "revocation recovery must transfer dropped name-only creates to an explicit preserve acknowledgement",
  );
  const recoveryStateName = revokeRecoveryItem.match(
    /(?:recovery|state):\s*([A-Za-z0-9_]+)\b/,
  )?.[1];
  assert.ok(
    recoveryStateName,
    "revocation recovery must retain its non-detachable terminal state",
  );
  const recoveryStateImplementation = implementationBlock(
    library,
    recoveryStateName,
  );
  const acknowledgeRecoveryState = functionBlocks(
    recoveryStateImplementation,
  ).find(({ name }) => /(?:acknowledge|preserve)/.test(name));
  assert.ok(
    acknowledgeRecoveryState,
    "terminal recovery state must own preserve acknowledgement",
  );
  const acknowledgePreservationRecovery = functionBlocks(
    preservationImplementation,
  ).find(({ name }) =>
    /(?:acknowledge|preserve).*recovery|recovery.*(?:acknowledge|preserve)/.test(
      name,
    ),
  );
  assert.ok(
    acknowledgePreservationRecovery,
    "preservation carrier must remain bound to exact drain recovery authority",
  );
  const revokeAcknowledgeFlow = `${acknowledgeDroppedDirectory.source}\n${acknowledgeRecoveryState.source}\n${acknowledgePreservationRecovery.source}\n${directoryAcknowledgeFlow}`;
  assert.match(revokeAcknowledgeFlow, preserveMarker);
  assert.match(
    revokeAcknowledgeFlow,
    /release_effect\s*\(|\.disarm\s*\([^;]*(?:token|operation)/,
  );
  assert.doesNotMatch(
    revokeAcknowledgeFlow,
    /remove_parked_directory\s*\(|unlinkat\s*\(/,
    "ordinary revocation cannot delete a directory known only by its former name",
  );

  const resetAuthority = implementationBlock(library, "RootResetAuthority");
  const resetRecovery = implementationBlock(library, "ResetDrainRecovery");
  const acknowledgeExternal = functionBlocks(resetRecovery).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /acknowledge/.test(name) &&
      /(?:external|all.*preserv)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    acknowledgeExternal,
    "reset recovery must explicitly acknowledge external-root residues before reset publication",
  );
  const externalAcknowledgeFlow = uniqueReachableFunctions(
    library,
    acknowledgeExternal.source,
  );
  assert.match(externalAcknowledgeFlow, preserveMarker);
  assert.match(
    externalAcknowledgeFlow,
    /release_effect\s*\(|\.disarm\s*\(/,
    "external acknowledgement must consume only the acknowledged effect permits",
  );
  if (/external/.test(acknowledgeExternal.name)) {
    assert.match(
      externalAcknowledgeFlow,
      /directory_create_is_external|!\s*[^;\n]*is_managed_root_descendant\s*\(/,
      "an external-only acknowledgement name requires retained-ancestry classification",
    );
    const externalSettlement = reachableFunctionBlocks(
      library,
      acknowledgeExternal.source,
    ).find(
      ({ source }) =>
        /directory_create_is_external|is_managed_root_descendant/.test(
          source,
        ) &&
        /acknowledge_preserved_with_recovery|release_effect\s*\(|\.disarm\s*\(/.test(
          source,
        ),
    );
    assert.ok(
      externalSettlement,
      "external reset acknowledgement must causally filter each retained parent before settlement",
    );
    const externalOnlyGuard = conditionalBlocks(externalSettlement.source).find(
      ({ condition, body }) =>
        /directory_create_is_external|is_managed_root_descendant/.test(
          condition,
        ) &&
        (/acknowledge_preserved_with_recovery|release_effect\s*\(|\.disarm\s*\(/.test(
          body,
        ) ||
          /\b(?:continue|return)\b/.test(body)),
    );
    assert.ok(
      externalOnlyGuard,
      "external reset acknowledgement must settle only the ancestry-classified external branch",
    );
  } else {
    assert.match(
      acknowledgeExternal.name,
      /acknowledge_all_preserved/,
      "an unfiltered acknowledgement must truthfully say that it preserves every residue",
    );
  }
  const transferToReset = functionBlocks(resetRecovery).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:defer|transfer|retain|continue)/.test(name) &&
      /(?:residue|preserv|create|reset)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    transferToReset,
    "reset recovery must explicitly transfer preserve-only creates into reset authority",
  );
  const transferFlow = uniqueReachableFunctions(
    library,
    transferToReset.source,
  );
  assert.match(
    transferToReset.source.slice(0, transferToReset.source.indexOf("{")),
    /->\s*ResetStartOutcome\b/,
    "reset residue transfer must return the retained reset-drain outcome",
  );
  assert.doesNotMatch(
    transferFlow,
    /let\s+_[a-z0-9_]*\s*:\s*Option\s*<\s*RootResetAuthority\s*>\s*=\s*None|let\s+_[a-z0-9_]*\s*=\s*(?:RootResetAuthority|ResetStartOutcome::Ready)/,
    "unused authority-shaped markers are not causal reset ownership transfer",
  );
  const transferCall = transferToReset.source.match(
    /\.([a-z_]*transfer[a-z_]*directory[a-z_]*create[a-z_]*reset[a-z_]*)\s*\(/,
  )?.[0];
  const finishCall = transferToReset.source.match(/\bself\.finish\s*\(/)?.[0];
  const recoveryFinish = functionBlock(resetRecovery, "finish");
  assert.ok(
    transferCall &&
      finishCall &&
      /\.drain\.try_settle\s*\(/.test(recoveryFinish),
    "reset recovery transfer must continue through its retained drain into Ready authority",
  );
  assertOrdered(
    transferToReset.source,
    transferCall,
    finishCall,
    "typed residue transfer before reset drain continuation",
  );
  assert.doesNotMatch(
    transferFlow,
    /release_effect\s*\(|remove_parked_directory\s*\(/,
    "reset transfer must retain, not settle, preserve-only effect permits",
  );
  const operationState = itemBlock(library, "struct", "OperationState");
  const liveDirectoryCreates = operationState.match(
    new RegExp(
      `([a-z_]*directory[a-z_]*creat[a-z_]*):\\s*(?:HashMap|BTreeMap)<[^>]*${escapeRegExp(directoryRecordName)}[^>]*>`,
    ),
  )?.[1];
  const unresolvedEffectCount = operationState.match(
    /([a-z_]*(?:outstanding|retained)[a-z_]*effects[a-z_]*):\s*usize\b/,
  )?.[1];
  const resetPendingVariant = directoryPhase.match(
    /\b[A-Za-z0-9_]*CreatedUnclassified[A-Za-z0-9_]*Reset[A-Za-z0-9_]*Pending[A-Za-z0-9_]*\b|\b[A-Za-z0-9_]*Managed[A-Za-z0-9_]*Reset[A-Za-z0-9_]*Pending[A-Za-z0-9_]*\b/,
  )?.[0];
  assert.ok(
    liveDirectoryCreates && unresolvedEffectCount && resetPendingVariant,
    "reset transfer needs a typed managed reset-pending phase under shared unresolved accounting",
  );
  const resetTransferOwner = reachableFunctionBlocks(
    library,
    transferToReset.source,
  ).find(({ source }) =>
    new RegExp(
      `\\bphase\\s*=\\s*${escapeRegExp(directoryPhaseName)}::${escapeRegExp(resetPendingVariant)}`,
    ).test(source),
  );
  assert.ok(
    resetTransferOwner,
    "reset recovery must transfer each managed residue into a typed reset-pending phase",
  );
  assert.match(
    resetTransferOwner.source,
    /is_managed_root_descendant\s*\(|DirectoryParent|ManagedRoot/,
    "only retained ancestry rooted at the managed app root may become reset-pending",
  );
  const resetAssignment = resetTransferOwner.source.match(
    new RegExp(
      `\\bphase\\s*=\\s*${escapeRegExp(directoryPhaseName)}::${escapeRegExp(resetPendingVariant)}`,
    ),
  )?.[0];
  const managedGuard = conditionalBlocks(resetTransferOwner.source).find(
    ({ condition, body }) =>
      /is_managed_root_descendant\s*\(/.test(condition) &&
      (/\b(?:return|continue)\b/.test(body) || body.includes(resetAssignment)),
  );
  assert.ok(
    resetAssignment && managedGuard,
    "managed-root provenance must causally gate reset-pending ownership transfer",
  );
  const transferDisarm = resetTransferOwner.source.match(
    /(?:token|preservation)[a-z0-9_.]*armed\s*=\s*false|\.disarm\s*\(/,
  )?.[0];
  assert.ok(
    transferDisarm,
    "reset transfer must disarm the superseded recovery carrier without releasing its effect",
  );
  assertOrdered(
    resetTransferOwner.source,
    resetAssignment,
    transferDisarm,
    "typed reset ownership before old recovery carrier disarm",
  );
  const resetAssignments = functionBlocks(library).filter(({ source }) =>
    new RegExp(
      `\\bphase\\s*=\\s*${escapeRegExp(directoryPhaseName)}::${escapeRegExp(resetPendingVariant)}`,
    ).test(source),
  );
  assert.deepEqual(
    resetAssignments.map(({ name }) => name),
    [resetTransferOwner.name],
    "every reset-pending directory create must originate in the managed-proven transfer",
  );
  assert.doesNotMatch(
    transferFlow,
    new RegExp(
      `release_effect\\s*\\(|\\b${escapeRegExp(unresolvedEffectCount)}\\b\\s*(?:-=|=\\s*[^;]*checked_sub)`,
    ),
    "reset transfer cannot decrement unresolved-effect accounting",
  );
  const resetDrain = implementationBlock(library, "ResetDrainAuthority");
  const resetSettlement = uniqueReachableFunctions(
    library,
    functionBlock(resetDrain, "try_settle"),
  );
  const resetTerminal = reachableFunctionBlocks(
    library,
    functionBlock(resetDrain, "try_settle"),
  ).find(
    ({ source }) =>
      /\.lock\s*\(\s*\)/.test(source) &&
      new RegExp(`\\b${escapeRegExp(liveDirectoryCreates)}\\b`).test(source) &&
      new RegExp(`\\b${escapeRegExp(resetPendingVariant)}\\b`).test(source) &&
      /AUTHORITY_RESETTING|\bResetting\b/.test(source),
  );
  assert.ok(
    resetTerminal,
    "RESETTING publication needs a final locked proof over every directory-create residue",
  );
  const resetPendingCount = resetTerminal.source.match(
    new RegExp(
      `let\\s+([a-z_]*reset[a-z_]*(?:count|effects)[a-z_]*)\\s*=[\\s\\S]{0,320}?\\b${escapeRegExp(liveDirectoryCreates)}\\b[\\s\\S]{0,320}?\\b${escapeRegExp(resetPendingVariant)}\\b[\\s\\S]{0,160}?\\.count\\s*\\(\\s*\\)`,
    ),
  )?.[1];
  const expectedResetEffects = resetPendingCount
    ? resetTerminal.source.match(
        new RegExp(
          `let\\s+([a-z_]*expected[a-z_]*(?:effect|outstanding)[a-z_]*)\\s*=\\s*if[\\s\\S]{0,180}?(?:AUTHORITY_RESETTING|Resetting)[\\s\\S]{0,120}?\\b${escapeRegExp(resetPendingCount)}\\b`,
        ),
      )?.[1]
    : undefined;
  assert.ok(
    resetPendingCount && expectedResetEffects,
    "reset settlement must count exactly the managed reset-pending effects",
  );
  const finalResetLock = resetTerminal.source.lastIndexOf(".lock(");
  const resetPublication = resetTerminal.source.lastIndexOf(
    "state.phase = terminal_phase",
  );
  const finalResetProof = resetTerminal.source.slice(
    finalResetLock,
    resetPublication,
  );
  assert.match(
    finalResetProof,
    new RegExp(
      `\\b${escapeRegExp(liveDirectoryCreates)}\\b[\\s\\S]{0,420}?\\.all\\s*\\([\\s\\S]{0,260}?\\b${escapeRegExp(resetPendingVariant)}\\b`,
    ),
    "the final reset lock must prove every remaining directory-create record is reset-owned",
  );
  assert.match(
    finalResetProof,
    new RegExp(
      `\\b${escapeRegExp(unresolvedEffectCount)}\\b\\s*!=\\s*\\b${escapeRegExp(expectedResetEffects)}\\b`,
    ),
    "the final reset lock must bind shared effect accounting to the exact reset-pending count",
  );
  assert.ok(
    resetPublication > finalResetLock,
    "RESETTING may publish only after the final locked ownership proof",
  );
  assert.match(resetSettlement, /ResetStartOutcome::Ready|\bReady\s*\(/);
  const resetClear = functionBlocks(resetAuthority).find(
    ({ name, source }) =>
      /^pub fn/.test(source) && /(?:clear|remove)/.test(name),
  );
  assert.ok(
    resetClear,
    "whole-root reset authority must own reset settlement for preserved managed creates",
  );
  const resetClearFlow = uniqueReachableFunctions(library, resetClear.source);
  const nativeRootClear = resetClearFlow.match(
    /platform::[a-z_]*(?:clear|remove)[a-z_]*(?:root|children)[a-z_]*\s*\(/,
  )?.[0];
  assert.ok(
    nativeRootClear &&
      new RegExp(`\\b${escapeRegExp(resetPendingVariant)}\\b`).test(
        resetClearFlow,
      ),
    "reset clear must settle managed preserved creates only under whole-root deletion authority",
  );
  const retireManagedPending = reachableFunctionBlocks(
    library,
    resetClear.source,
  ).find(
    ({ name, source }) =>
      name !== resetClear.name &&
      /\.lock\s*\(\s*\)/.test(source) &&
      /AUTHORITY_RESETTING|\bResetting\b/.test(source) &&
      new RegExp(`\\b${escapeRegExp(resetPendingVariant)}\\b`).test(source) &&
      /release_effect\s*\(/.test(source),
  );
  assert.ok(
    retireManagedPending,
    "successful whole-root clear needs typed managed-residue retirement",
  );
  assert.match(
    retireManagedPending.source,
    new RegExp(
      `(?:phase|state)[\\s\\S]{0,120}?(?:==|matches!)[\\s\\S]{0,180}?\\b${escapeRegExp(resetPendingVariant)}\\b`,
    ),
    "root clear may release only records already classified as managed reset-pending",
  );
  const retireManagedCall = resetClear.source.match(
    new RegExp(`\\b${escapeRegExp(retireManagedPending.name)}\\s*\\(`),
  )?.[0];
  assert.ok(
    retireManagedCall,
    "root clear must causally invoke managed reset-pending retirement",
  );
  assertOrdered(
    resetClear.source,
    nativeRootClear,
    retireManagedCall,
    "destructive root proof before atomic managed-residue retirement",
  );
  const acknowledgeResetPreserved = functionBlocks(resetAuthority).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:acknowledge|preserve)/.test(name) &&
      /(?:preserv|residue|create|unclassified)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    acknowledgeResetPreserved,
    "declined clear and external residues need explicit reset-authority acknowledgement",
  );
  assert.match(
    acknowledgeResetPreserved.source.slice(
      0,
      acknowledgeResetPreserved.source.indexOf("{"),
    ),
    /->\s*(?:Result|[A-Za-z0-9_]*(?:Outcome|Resolution|Preservation))\b/,
    "reset preservation acknowledgement must retain failure authority in a must-use result",
  );
  const acknowledgeResetFlow = uniqueReachableFunctions(
    library,
    acknowledgeResetPreserved.source,
  );
  assert.match(acknowledgeResetFlow, /release_effect\s*\(|\.disarm\s*\(/);
  assert.match(
    acknowledgeResetFlow,
    new RegExp(`\\b${escapeRegExp(resetPendingVariant)}\\b`),
    "declining clear may acknowledge only the managed reset-pending records",
  );
  assert.doesNotMatch(
    acknowledgeResetFlow,
    /platform::[a-z_]*(?:clear|remove)[a-z_]*(?:root|children)/,
    "reset preservation acknowledgement must not claim deletion",
  );
  const resetRelease = functionBlocks(resetAuthority).find(
    ({ name, source }) =>
      /^pub fn/.test(source) && /^(?:finish|release)$/.test(name),
  );
  assert.ok(resetRelease, "reset authority needs explicit terminal release");
  assert.match(
    resetRelease.source,
    /(?:is_empty|has_[a-z_]*(?:preserv|residue|reset[a-z_]*pending)[a-z_]*|(?:preserv|residue|reset[a-z_]*pending)[a-z_]*\.is_empty)\s*\(/,
    "reset release must refuse to discard transferred preserve-only carriers",
  );
  assert.match(
    resetRelease.source.slice(0, resetRelease.source.indexOf("{")),
    /->\s*(?!\(\s*\))(?:Result|[A-Za-z0-9_]*(?:Outcome|Resolution|Failure|Preservation))\b/,
    "reset release with pending residue must return retained authority instead of silently succeeding",
  );
  const clearFailure = implementationBlock(library, "RootClearFailure");
  const clearRetry = functionBlocks(clearFailure).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /retry/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  const clearFailurePreserve = functionBlocks(clearFailure).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:acknowledge|preserve)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    clearRetry && clearFailurePreserve,
    "failed root clear must expose consuming retry and explicit preserve exits",
  );
  assert.match(
    clearRetry.source.slice(0, clearRetry.source.indexOf("{")),
    /->\s*RootClearOutcome\b/,
    "failed root clear retry must return its must-use clear outcome",
  );
  assert.match(
    uniqueReachableFunctions(library, clearRetry.source),
    /\.clear_root\s*\(/,
    "failed root clear retry must re-enter the retained reset authority clear",
  );
  const clearFailurePreserveFlow = uniqueReachableFunctions(
    library,
    clearFailurePreserve.source,
  );
  assert.match(
    clearFailurePreserveFlow,
    /\.acknowledge_preserved[a-z0-9_]*\s*\(/,
    "failed root clear preservation must delegate through retained reset authority",
  );
  assert.match(
    clearFailurePreserve.source.slice(
      0,
      clearFailurePreserve.source.indexOf("{"),
    ),
    /->\s*(?:Result\s*<|[A-Za-z0-9_]+(?:Outcome|Resolution|Failure)\b)/,
    "failed root clear preservation must return a must-use retained outcome",
  );
  assert.match(
    clearFailurePreserve.source,
    /\.authority\s*\.\s*take\s*\(\s*\)/,
    "failed clear preservation must take its linear authority for settlement",
  );
  assert.match(
    clearFailurePreserve.source,
    /Err\s*\(\s*(?:mut\s+)?authority\s*\)[\s\S]{0,260}?self\.authority\s*=\s*Some\s*\(\s*authority\s*\)[\s\S]{0,200}?Err\s*\(\s*self\s*\)|self\.authority\s*=\s*Some\s*\(\s*authority\s*\)[\s\S]{0,200}?(?:Failed|PreservedUnverified)\s*\(\s*self\s*\)/,
    "failed preserve admission must return the failure with its exact reset authority restored",
  );
  const clearPreserveOutcome = clearFailurePreserve.source
    .slice(0, clearFailurePreserve.source.indexOf("{"))
    .match(/->\s*([A-Za-z0-9_]+(?:Outcome|Resolution|Failure))\b/)?.[1];
  if (clearPreserveOutcome) {
    const outcomeKind = library.match(
      new RegExp(`(?:pub\\s+)?(enum|struct)\\s+${clearPreserveOutcome}\\b`),
    )?.[1];
    assert.ok(outcomeKind);
    assertMustUse(library, outcomeKind, clearPreserveOutcome);
  }
  const resetDrop = traitImplementationBlock(
    library,
    "Drop",
    "RootResetAuthority",
  );
  assert.doesNotMatch(
    `${resetAuthority}\n${resetDrop}`,
    /mem::forget\s*\(|ManuallyDrop/,
    "pending reset authority cannot detach its retained session",
  );
  const unresolvedDropGuard = resetDrop.match(
    /self\.session\.is_some\s*\(\s*\)/,
  )?.[0];
  const unresolvedDropAbort = resetDrop.match(
    /std::process::abort\s*\(\s*\)/,
  )?.[0];
  assert.ok(
    unresolvedDropGuard && unresolvedDropAbort,
    "dropping any unresolved reset authority must fail-stop",
  );
  assertOrdered(
    resetDrop,
    unresolvedDropGuard,
    unresolvedDropAbort,
    "unresolved reset-session proof before fail-stop",
  );
  assert.doesNotMatch(
    resetDrop,
    /has_[a-z_]*reset[a-z_]*pending[a-z_]*\s*\(/,
    "reset-authority Drop cannot fail-stop only for one pending-effect subtype",
  );
  assert.doesNotMatch(
    resetDrop,
    /release_effect\s*\(|\.session\.take\s*\(/,
    "RootResetAuthority drop cannot claim settlement or bypass its unresolved guard",
  );
  for (const terminal of [
    resetClear,
    acknowledgeResetPreserved,
    resetRelease,
  ]) {
    assert.match(
      terminal.source.slice(0, terminal.source.indexOf("{")),
      /\bself\b/,
      `${terminal.name} must consume reset authority`,
    );
  }
  assert.match(
    resetClear.source,
    /RootClearReceipt\s*\{[\s\S]*?authority:\s*Some\s*\(\s*self\s*\)/,
    "successful root clear must transfer the exact reset authority into its receipt",
  );
  assert.doesNotMatch(
    resetClear.source,
    /\.session\.take\s*\(|\.revoke\s*\(|drop\s*\(\s*self\.session/,
    "successful root clear cannot release its lease before receipt settlement",
  );
  for (const terminal of [acknowledgeResetPreserved, resetRelease]) {
    assert.match(
      terminal.source,
      /\.session\.take\s*\(\s*\)/,
      `${terminal.name} must remove the settled session before Drop runs`,
    );
  }

  const createRootDirectory = functionBlock(
    unix,
    "create_and_publish_root_directory",
  );
  const rootMkdir = createRootDirectory.match(/mkdirat\s*\(/)?.[0];
  const rootOpen = createRootDirectory.match(/openat\s*\(/)?.[0];
  assert.ok(
    rootMkdir && rootOpen,
    "Unix root creation needs mkdir then retained open",
  );
  const rootOpenFailure = matchArmBlocks(
    createRootDirectory.slice(createRootDirectory.indexOf(rootOpen)),
    /Err\s*\([^)]*\)/,
  ).find(({ body }) => /RootDirectoryCreationError/.test(body));
  assert.ok(
    rootOpenFailure,
    "Unix root mkdir needs an explicit post-mkdir open-failure arm",
  );
  assert.match(
    rootOpenFailure.body,
    new RegExp(
      `${preserveMarker.source}|RootDirectoryCreationError::Unclassified`,
    ),
    "root mkdir success without a retained handle must stay preserve-only",
  );
  assert.doesNotMatch(
    rootOpenFailure.body,
    /entry_observation|statat\s*\(|directory_identity\s*\(|fstat\s*\(/,
    "root creation cannot promote a post-hoc name observation to created identity",
  );
  assert.doesNotMatch(
    rootOpenFailure.body,
    /RootCreatedBinding\s*\{/,
    "root mkdir/open ambiguity cannot mint an identity-owned created binding",
  );

  const rootConstruction = itemBlock(unix, "struct", "RootConstruction");
  const residueType = rootConstruction.match(
    /Vec<([A-Za-z0-9_]*(?:Unclassified|Residue|Reservation)[A-Za-z0-9_]*)>/,
  )?.[1];
  assert.ok(
    residueType,
    "root construction must retain preserve-only debris separately from created bindings",
  );
  const residueKind = unix.match(
    new RegExp(`\\b(struct|enum)\\s+${escapeRegExp(residueType)}\\b`),
  )?.[1];
  const rootResidue = itemBlock(unix, residueKind, residueType);
  assert.match(rootResidue, /parent:\s*(?:Option<)?DirectoryHandle/);
  assert.match(rootResidue, /name:\s*(?:OsString|LeafName)/);
  assert.doesNotMatch(
    rootResidue,
    /identity:\s*Identity/,
    "a name-only root residue cannot claim physical identity",
  );

  const acquireObligation = implementationBlock(
    library,
    "RootSessionAcquireObligation",
  );
  const acknowledgeRoot = functionBlocks(acquireObligation).find(
    ({ name, source }) =>
      /^pub fn/.test(source) &&
      /(?:acknowledge|preserve)/.test(name) &&
      /(?:residue|preserv|unclassified)/.test(name) &&
      /\bself\b/.test(source.slice(0, source.indexOf("{"))),
  );
  assert.ok(
    acknowledgeRoot,
    "root acquisition must expose terminal acknowledgement of preserve-only debris",
  );
  const rootAcknowledgeFlow = `${acknowledgeRoot.source}\n${functionBlock(unix, "acknowledge_preserved_root_construction")}`;
  assert.match(rootAcknowledgeFlow, /unclassified|preserv/i);
  assert.doesNotMatch(
    rootAcknowledgeFlow,
    /unlinkat\s*\(|cleanup_root_construction\s*\(/,
    "acknowledging name-only root debris must preserve it without claiming cleanup",
  );
  const rootAckReturn = acknowledgeRoot.source
    .slice(0, acknowledgeRoot.source.indexOf("{"))
    .match(
      /->\s*(?:(?:io::)?Result\s*<\s*)?([A-Za-z0-9_]+(?:Outcome|Resolution))\b/,
    )?.[1];
  if (rootAckReturn) {
    assertMustUse(library, "enum", rootAckReturn);
  }
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
    /\bSerialize\b|\bDeserialize\b|\bpersistent_(?:binding|identity)\b|\b(?:StageJournal|PersistedStage|DurableReceipt|StartupStageRecovery|PidStage|StagePid)\b|std::process::id\(|process::id\(/,
    "axial-fs must not persist native identity, stage state, PID sweep authority, or restart truth",
  );
  assert.doesNotMatch(
    `${library}\n${platform}`,
    /fn [a-z_]*(?:startup|restart)[a-z_]*(?:sweep|recover|cleanup)[a-z_]*\s*\(|fn [a-z_]*(?:sweep|recover)[a-z_]*(?:stage|temp|pid)[a-z_]*\s*\(/,
    "B03 owns startup recovery and durable staged-object cleanup",
  );
});

test("P01-B02 never serializes native filesystem identity", async () => {
  const rustSources = await readRustTree("apps", "core");
  const byPath = new Map(rustSources);
  assertAbsent(rustSources, [
    /\bpersistent_filesystem_binding\b/,
    /\bpersistent_identity_binding\b/,
    /\bpersistent_binding\b/,
    /\broot_binding_sha256\b/,
    /\bdirectory_identity_sha256\b/,
  ]);
  for (const path of [
    "core/minecraft/src/managed_component_table.rs",
    "core/minecraft/src/managed_component_publication.rs",
    "core/minecraft/src/managed_component_ancestor_journal.rs",
  ]) {
    assert.match(byPath.get(path), /const FORMAT_VERSION: u16 = 2;/);
  }
  assert.match(
    byPath.get("core/minecraft/src/version_bundle_publication.rs"),
    /axial\.version_bundle_publication\.intent\.v2/,
  );
  const effects = byPath.get("core/minecraft/src/managed_component_effects.rs");
  const transaction = byPath.get(
    "core/minecraft/src/managed_component_effects/managed_component_transaction.rs",
  );
  assert.match(
    effects,
    /restart_recovery_accepts_a_coherently_copied_logical_transaction/,
  );
  assert.match(
    effects,
    /restart_recovery_rejects_a_partial_copied_logical_transaction/,
  );
  assert.match(
    effects,
    /ancestor_recovery_requires_live_identity_for_a_partial_canonical_prefix/,
  );
  assert.match(
    effects,
    /only_live_recovery_cleans_an_exact_unjournaled_ancestor_bucket/,
  );
  assert.match(transaction, /runtime_ancestors:\s*None/);
  assert.match(
    transaction,
    /restart_recovery\s*&&\s*ancestor_plan\.canonical_records\s*!=\s*0/,
  );
  assert.match(
    transaction,
    /published\.runtime_ancestors\.is_none\(\)\s*&&\s*parked_bucket\.is_some\(\)/,
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
        !name.includes("move") &&
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
  assert.doesNotMatch(
    windows,
    /fn\s+[a-z_][a-z0-9_]*\s*<[^>]+>\s*\([^)]*\)\s*->\s*(?:io::)?Result[^\{]+\{[\s\S]{0,800}?GetFileInformationByHandleEx\s*\(/,
    "safe Windows metadata queries cannot accept an arbitrary output type and FILE_INFO_BY_HANDLE_CLASS pairing",
  );
  const directWindowsInfoQueries = windowsFunctions.filter(({ source }) =>
    /GetFileInformationByHandleEx\s*\(/.test(source),
  );
  for (const query of directWindowsInfoQueries) {
    const header = query.source.slice(0, query.source.indexOf("{"));
    assert.doesNotMatch(
      header,
      /fn\s+[a-z_][a-z0-9_]*\s*</,
      `${query.name} cannot bind a native information class to a generic output type`,
    );
    assert.doesNotMatch(
      header,
      /\b(?:class|info_class):\s*[A-Za-z0-9_:]+/,
      `${query.name} cannot accept a caller-selected information class`,
    );
  }
  const typedWindowsQueries = [];
  for (const [infoType, infoClass] of [
    ["FILE_BASIC_INFO", "FileBasicInfo"],
    ["FILE_STANDARD_INFO", "FileStandardInfo"],
    ["FILE_ID_INFO", "FileIdInfo"],
  ]) {
    const typedQuery = windowsFunctions.find(({ source }) => {
      const header = source.slice(0, source.indexOf("{"));
      return (
        new RegExp(
          `->\\s*(?:io::)?Result\\s*<\\s*${escapeRegExp(infoType)}\\s*>`,
        ).test(header) &&
        new RegExp(`\\b${escapeRegExp(infoClass)}\\b`).test(source) &&
        /GetFileInformationByHandleEx\s*\(/.test(source)
      );
    });
    assert.ok(
      typedQuery,
      `Windows ${infoType} needs a fixed safe query with its exact information class`,
    );
    typedWindowsQueries.push(typedQuery);
    assert.match(
      typedQuery.source,
      new RegExp(
        `size_of\\s*::<\\s*${escapeRegExp(infoType)}\\s*>|size_of_val\\s*\\(`,
      ),
      `Windows ${infoType} query must pass its exact output size`,
    );
    assert.match(
      typedQuery.source,
      new RegExp(
        `MaybeUninit\\s*::<\\s*${escapeRegExp(infoType)}\\s*>::uninit\\s*\\(|${escapeRegExp(infoType)}::default\\s*\\(`,
      ),
      `Windows ${infoType} query must use valid typed initialization`,
    );
    for (const otherClass of [
      "FileBasicInfo",
      "FileStandardInfo",
      "FileIdInfo",
    ]) {
      if (otherClass === infoClass) continue;
      assert.doesNotMatch(
        typedQuery.source,
        new RegExp(`\\b${escapeRegExp(otherClass)}\\b`),
        `Windows ${infoType} query cannot select ${otherClass}`,
      );
    }
  }
  const typedQueryNames = new Set(typedWindowsQueries.map(({ name }) => name));
  for (const query of directWindowsInfoQueries) {
    const directoryEnumeration =
      /FILE_ID_BOTH_DIR_INFO/.test(query.source) &&
      /(?:DirectoryEntries|Vec\s*<)/.test(
        query.source.slice(0, query.source.indexOf("{")),
      );
    const exactLeafNameQuery =
      query.name === "opened_directory_leaf_name" &&
      /FileNameInfo/.test(query.source) &&
      /FileNameInformation/.test(query.source) &&
      /checked_add\(name_bytes\)/.test(query.source);
    assert.ok(
      typedQueryNames.has(query.name) ||
        directoryEnumeration ||
        exactLeafNameQuery,
      `${query.name} must be a fixed typed metadata query or the bounded directory enumerator`,
    );
  }
  for (const typedQuery of typedWindowsQueries) {
    assert.match(
      windows.replace(typedQuery.source, ""),
      new RegExp(`\\b${escapeRegExp(typedQuery.name)}\\s*\\(`),
      `${typedQuery.name} must be consumed by Windows metadata logic rather than merely satisfying the contract`,
    );
  }
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

test("P01-B02 proves the retained directory object is unlinked before removal success", async () => {
  const platform = await read("core/fs/src/platform.rs");
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );

  for (const [platformName, source, linkProof] of [
    ["Unix", unix, /(?:st_nlink|link_count|links)\s*==\s*0/],
    [
      "Windows",
      windows,
      /(?:NumberOfLinks|number_of_links|link_count|links)\s*==\s*0/,
    ],
  ]) {
    for (const operationName of [
      "remove_parked_directory",
      "settle_removed_directory",
    ]) {
      const operation = functionBlock(source, operationName);
      const operationHeader = operation.slice(0, operation.indexOf("{"));
      const cleanupParameter = operationHeader.match(
        /\b([a-z_][a-z0-9_]*):\s*&(?:mut\s+)?DirectoryCleanupHandle\b/,
      )?.[1];
      const expectedParameter = operationHeader.match(
        /\b([a-z_][a-z0-9_]*):\s*Identity\b/,
      )?.[1];
      assert.ok(
        cleanupParameter && expectedParameter,
        `${platformName} ${operationName} must receive retained cleanup authority and its expected identity`,
      );

      const proof = reachableFunctionBlocks(source, operation).find(
        ({ source: block }) => {
          const header = block.slice(0, block.indexOf("{"));
          const retained = header.match(
            /\b([a-z_][a-z0-9_]*):\s*&(?:mut\s+)?(?:DirectoryCleanupHandle|DirectoryHandle|File|OwnedFd)\b/,
          )?.[1];
          const expected = header.match(
            /\b([a-z_][a-z0-9_]*):\s*Identity\b/,
          )?.[1];
          if (!retained || !expected || !linkProof.test(block)) return false;
          const observesRetained = new RegExp(
            `(?:directory_identity|object_identity|fstat|query)\\s*\\([^)]*\\b${escapeRegExp(retained)}\\b`,
          ).test(block);
          const checksExpected = new RegExp(
            `(?:==|!=)[^;\\n]{0,160}\\b${escapeRegExp(expected)}\\b|\\b${escapeRegExp(expected)}\\b[^;\\n]{0,160}(?:==|!=)`,
          ).test(block);
          const positiveConjunction = new RegExp(
            `Ok\\s*\\([\\s\\S]{0,320}?(?:(?:identity|id)[a-z0-9_().?]*\\s*==\\s*${escapeRegExp(expected)}|${escapeRegExp(expected)}\\s*==\\s*[^&|)]*(?:identity|id))[\\s\\S]{0,240}?&&[\\s\\S]{0,240}?${linkProof.source}|Ok\\s*\\([\\s\\S]{0,320}?${linkProof.source}[\\s\\S]{0,240}?&&[\\s\\S]{0,240}?(?:(?:identity|id)[a-z0-9_().?]*\\s*==\\s*${escapeRegExp(expected)}|${escapeRegExp(expected)}\\s*==\\s*[^&|)]*(?:identity|id))`,
            "i",
          ).test(block);
          const identityRefusal = conditionalBlocks(block).some(
            ({ condition, body }) =>
              new RegExp(
                `(?:(?:identity|id)[^;\\n]{0,160}?!=\\s*${escapeRegExp(expected)}|${escapeRegExp(expected)}\\s*!=[^;\\n]{0,160}?(?:identity|id))`,
                "i",
              ).test(condition) && /return\s+Err\s*\(/.test(body),
          );
          const positiveLinkReturn = new RegExp(
            `Ok\\s*\\([^)]{0,120}?${linkProof.source}\\s*\\)`,
          ).test(block);
          return (
            observesRetained &&
            checksExpected &&
            (positiveConjunction || (identityRefusal && positiveLinkReturn))
          );
        },
      );
      assert.ok(
        proof,
        `${platformName} ${operationName} needs one reachable retained-handle identity plus native link-zero proof`,
      );

      const proofMarker =
        proof.name === operationName
          ? operation.match(linkProof)?.[0]
          : operation.match(
              new RegExp(
                `\\b${escapeRegExp(proof.name)}\\s*\\([^)]*\\b${escapeRegExp(cleanupParameter)}\\b(?:\\.(?:0|observation|handle))?[^)]*\\b${escapeRegExp(expectedParameter)}\\b[^)]*\\)`,
              ),
            )?.[0];
      const absence = operation.match(/BindingState::Absent/)?.[0];
      const success = operation.match(/Ok\s*\(\s*\(\s*\)\s*\)/)?.[0];
      const combinedRefusal = conditionalBlocks(operation).find(
        ({ condition, body }) =>
          /BindingState::Absent/.test(condition) &&
          new RegExp(`!\\s*${escapeRegExp(proof.name)}\\s*\\(`).test(
            condition,
          ) &&
          /return\s+Err\s*\(/.test(body),
      );
      assert.ok(
        proofMarker && absence && success && combinedRefusal,
        `${platformName} ${operationName} must combine name absence with retained-object removal proof before success`,
      );
      assertOrdered(
        operation,
        absence,
        success,
        `${platformName} ${operationName} namespace absence before success`,
      );
      assert.ok(
        operation.lastIndexOf(proofMarker) < operation.lastIndexOf(success),
        `${platformName} ${operationName} retained-object proof must precede success`,
      );
      if (operationName === "remove_parked_directory") {
        const deleteEffect = operation.match(
          /unlinkat\s*\(|set_delete\s*\(/,
        )?.[0];
        assert.ok(
          deleteEffect &&
            operation.lastIndexOf(deleteEffect) <
              operation.lastIndexOf(proofMarker),
          `${platformName} directory removal must prove link-zero after the delete effect`,
        );
      }
    }
  }
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

test("P01-B02 explicitly acknowledges preserved exact files", async () => {
  const library = await read("core/fs/src/lib.rs");
  assertMustUse(library, "struct", "FileParkPreservationError");
  assertLinear(library, "FileParkPreservationError");
  const preservationError = itemBlock(
    library,
    "struct",
    "FileParkPreservationError",
  );
  assert.match(preservationError, /error:\s*io::Error/);
  assert.match(preservationError, /parked:\s*ParkedFile/);
  assert.match(
    uniqueMethodBlock(library, "FileParkPreservationError", "error"),
    /pub fn error\s*\(\s*&self\s*\)\s*->\s*&io::Error/,
  );
  assert.match(
    uniqueMethodBlock(library, "FileParkPreservationError", "into_parked"),
    /pub fn into_parked\s*\(\s*self\s*\)\s*->\s*ParkedFile[\s\S]*\bself\.parked\b/,
  );

  const acknowledge = uniqueMethodBlock(
    library,
    "ParkedFile",
    "acknowledge_preserved",
  );
  assert.match(
    acknowledge.slice(0, acknowledge.indexOf("{")),
    /pub fn acknowledge_preserved\s*\(\s*mut self\s*\)\s*->\s*Result\s*<\s*\(\s*\)\s*,\s*FileParkPreservationError\s*>/,
  );
  const checkoutCall = callBlocks(
    acknowledge,
    /self\s*\.\s*checkout_current\s*\(/,
  )[0];
  const checkoutFailure = matchArmBlocks(
    acknowledge,
    /Err\s*\(\s*error\s*\)/,
  )[0];
  const disarmCall = callBlocks(acknowledge, /guard\s*\.\s*disarm\s*\(/)[0];
  assert.ok(checkoutCall && checkoutFailure && disarmCall);
  assert.deepEqual(callArguments(checkoutCall.source), []);
  assert.match(
    checkoutFailure.body,
    /return\s+Err\s*\(\s*FileParkPreservationError\s*\{[\s\S]{0,200}\berror\s*,[\s\S]{0,120}\bparked:\s*self\b/,
    "every checkout failure must return the still-armed ParkedFile",
  );
  assert.doesNotMatch(
    acknowledge.slice(0, disarmCall.index),
    /\?|\.armed\s*=\s*false|\.disarm\s*\(/,
    "acknowledgement cannot implicitly discard or pre-disarm its retained authority",
  );
  assert.deepEqual(callArguments(disarmCall.source), [
    "&mut self.token",
    "&operation",
  ]);
  assert.ok(
    checkoutCall.index < acknowledge.indexOf(checkoutFailure.body) &&
      acknowledge.indexOf(checkoutFailure.body) < disarmCall.index,
    "typed checkout refusal must precede the sole successful disarm",
  );

  const checkout = uniqueMethodBlock(library, "ParkedFile", "checkout_current");
  assert.match(
    checkout.slice(0, checkout.indexOf("{")),
    /&self[\s\S]*->\s*io::Result\s*<\s*\(\s*CapabilityOperation\s*,\s*FileParkRecordGuard\s*\)\s*>/,
  );
  const verified = checkout.indexOf("!self.verified");
  const enterCall = callBlocks(checkout, /enter_file_park\s*\(/)[0];
  const takeCall = callBlocks(checkout, /take_file_park\s*\(/)[0];
  const validateCheckedOutCall = callBlocks(
    checkout,
    /self\s*\.\s*validate_checked_out\s*\(/,
  )[0];
  assert.ok(
    verified !== -1 && enterCall && takeCall && validateCheckedOutCall,
    "current park checkout needs verified receipt, token admission, guard checkout, and exact validation",
  );
  assert.deepEqual(callArguments(enterCall.source), ["&self.token"]);
  assert.deepEqual(callArguments(takeCall.source), [
    "&operation",
    "&self.token",
  ]);
  assert.deepEqual(callArguments(validateCheckedOutCall.source), [
    "&operation",
    "guard.record()",
  ]);
  assert.ok(
    verified < enterCall.index &&
      enterCall.index < takeCall.index &&
      takeCall.index < validateCheckedOutCall.index,
    "current park checkout validations are out of order",
  );
  const finalValidation = conditionalBlocks(checkout).find(({ condition }) =>
    /self\.validate_checked_out\s*\(/.test(condition),
  );
  assert.ok(finalValidation, "missing checked-out park refusal");
  assertOrdered(
    finalValidation.body,
    "drop(guard)",
    "return Err(error)",
    "failed current validation must reinsert its checked-out record",
  );
  assert.doesNotMatch(
    checkout,
    /\.disarm\s*\(|\.armed\s*=\s*false/,
    "current validation cannot settle parked-file ownership",
  );

  const checkedOutValidation = uniqueMethodBlock(
    library,
    "ParkedFile",
    "validate_checked_out",
  );
  assert.match(checkedOutValidation, /FileParkRegistryPhase::Live/);
  assert.match(
    checkedOutValidation,
    /platform::parked_file_receipt_fields\s*\(/,
  );
  assert.match(checkedOutValidation, /platform::file_binding_state\s*\(/);
  assertCountAtLeast(
    checkedOutValidation,
    /self\.parent\.validate\s*\(\s*operation\s*\)/,
    2,
    "checked-out validation must bracket exact receipt and binding checks",
  );
  for (const field of ["identity", "size", "stamp", "name", "original_name"]) {
    assert.match(
      checkedOutValidation,
      new RegExp(`\\brecord\\.${escapeRegExp(field)}\\b`),
      `checked-out preservation must bind the exact ${field}`,
    );
  }

  const guardDisarm = uniqueMethodBlock(
    library,
    "FileParkRecordGuard",
    "disarm",
  );
  const ownerRemoval = guardDisarm.match(/park_owners\s*\.\s*remove\s*\(/)?.[0];
  const effectRelease = guardDisarm.match(/release_effect\s*\(/)?.[0];
  const tokenDisarm = guardDisarm.match(/token\s*\.\s*armed\s*=\s*false/)?.[0];
  const acknowledgementFlow = `${acknowledge}\n${checkout}\n${checkedOutValidation}`;
  assert.doesNotMatch(
    acknowledgementFlow,
    /platform::(?:park_file_no_replace|remove_parked_file|restore_parked_file|settle_removed_file|settle_restored_file|rename[a-z_]*|unlink[a-z_]*)\s*\(|std::fs|tokio::fs/,
    "preservation acknowledgement must not mutate the retained namespace",
  );
  assert.ok(
    ownerRemoval && effectRelease && tokenDisarm,
    "successful preservation must remove ownership, release its effect, and disarm",
  );
  assert.match(
    guardDisarm,
    /Some\s*\(\s*ParkRegistryOwner::File\s*\(\s*self\.id\s*\)\s*\)/,
    "preservation may remove only its exact file-park owner",
  );
  assertOrdered(
    guardDisarm,
    ownerRemoval,
    effectRelease,
    "preservation owner removal before effect release",
  );
  assertOrdered(
    guardDisarm,
    effectRelease,
    tokenDisarm,
    "preservation effect release before token disarm",
  );

  const successTest = functionBlock(
    library,
    "preserved_file_acknowledgement_leaves_the_leaf_and_clears_ownership",
  );
  assert.match(successTest, /\.acknowledge_preserved\s*\(\s*\)/);
  assert.match(successTest, /outstanding_effects\s*,\s*0/);
  assert.match(successTest, /file_parks\s*\.\s*is_empty\s*\(\s*\)/);
  assert.match(successTest, /park_owners\s*\.\s*is_empty\s*\(\s*\)/);
  assert.match(successTest, /record\.bin[^\n]*\.exists\s*\(\s*\)/);
  assert.match(successTest, /std::fs::read[\s\S]{0,160}record\.preserved/);

  const reinsertionTest = functionBlock(
    library,
    "preserved_file_acknowledgement_reinserts_mismatched_park_ownership",
  );
  assertOrdered(
    reinsertionTest,
    "record.size",
    ".acknowledge_preserved()",
    "registered-record mismatch before refused preservation acknowledgement",
  );
  assert.match(reinsertionTest, /\.into_parked\s*\(\s*\)/);
  assert.match(reinsertionTest, /token\.armed/);
  assert.match(reinsertionTest, /outstanding_effects\s*,\s*1/);
  assert.match(reinsertionTest, /file_parks_checked_out\s*,\s*0/);
  assert.match(reinsertionTest, /file_parks\.len\s*\(\s*\)\s*,\s*1/);
  assert.match(reinsertionTest, /park_owners\.len\s*\(\s*\)\s*,\s*1/);

  const unixFailureTest = functionBlock(
    library,
    "preserved_file_acknowledgement_returns_mutated_park_ownership",
  );
  assert.match(
    library,
    /#\[cfg\(unix\)\]\s*#\[test\]\s*fn preserved_file_acknowledgement_returns_mutated_park_ownership\s*\(/,
    "live file mutation is a Unix-only check because the Windows cleanup handle denies writes",
  );
  assertOrdered(
    unixFailureTest,
    "mutated-payload",
    ".acknowledge_preserved()",
    "park mutation before refused preservation acknowledgement",
  );
  assert.match(unixFailureTest, /\.into_parked\s*\(\s*\)/);
  assert.match(unixFailureTest, /token\.armed/);
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
    const reservationEntry = functionBlock(library, reservationName);
    const reservation = reachableFunctionBlocks(library, reservationEntry).find(
      ({ source }) =>
        new RegExp(`\\.${escapeRegExp(reserveEffect.name)}\\s*\\(`).test(
          source,
        ) && /\.insert\s*\(/.test(source),
    )?.source;
    assert.ok(reservation, `${label} needs one reachable reservation owner`);
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
  const beginDrainFlow = uniqueReachableFunctions(library, beginDrain);
  const drainSettlement = uniqueReachableFunctions(
    library,
    functionBlock(library, "try_finish_terminal_drain"),
  );
  assert.match(
    beginDrainFlow,
    new RegExp(
      `\\b${escapeRegExp(effectField)}\\b\\s*!=\\s*[a-z_]*registered[a-z_]*effects`,
    ),
    "terminal admission must revalidate exact shared-effect accounting after broker quiescence",
  );
  assert.match(
    drainSettlement,
    /let\s+[a-z_]*expected[a-z_]*(?:effect|outstanding)[a-z_]*\s*=\s*if[\s\S]{0,200}?(?:AUTHORITY_RESETTING|Resetting)[\s\S]{0,180}?reset[a-z_]*(?:count|effects)[\s\S]{0,100}?else\s*\{\s*0\s*\}/,
    "reset publication may retain only its exactly counted managed reset-pending effects",
  );
  assert.match(
    drainSettlement,
    new RegExp(
      `\\b${escapeRegExp(effectField)}\\b\\s*!=\\s*[a-z_]*expected[a-z_]*(?:effect|outstanding)[a-z_]*`,
    ),
    "revocation requires zero effects while reset requires its exact managed-pending count",
  );
});

test("P01-B02 retries abandoned create cleanup from its applied-delete phase", async () => {
  const library = await read("core/fs/src/lib.rs");
  const authority = implementationBlock(library, "CapabilityAuthority");

  for (const {
    label,
    recordPattern,
    cleanupPattern,
    settleOperation,
    removeOperation,
  } of [
    {
      label: "staged-file create",
      recordPattern:
        /struct ([A-Za-z0-9_]*StageCreate[A-Za-z0-9_]*(?:Record|Reservation)[A-Za-z0-9_]*)\s*\{/,
      cleanupPattern: /(?:cleanup|settle)[a-z_]*stage[a-z_]*create/,
      settleOperation: "settle_removed_file",
      removeOperation: "remove_parked_file",
    },
    {
      label: "directory create",
      recordPattern:
        /struct ([A-Za-z0-9_]*DirectoryCreate[A-Za-z0-9_]*(?:Record|Reservation)[A-Za-z0-9_]*)\s*\{/,
      cleanupPattern: /(?:cleanup|settle)[a-z_]*directory[a-z_]*create/,
      settleOperation: "settle_removed_directory",
      removeOperation: "remove_parked_directory",
    },
  ]) {
    const recordName = library.match(recordPattern)?.[1];
    assert.ok(recordName, `${label} needs a typed effect record`);
    const record = itemBlock(library, "struct", recordName);
    const phaseName = record.match(/(?:phase|state):\s*([A-Za-z0-9_]+)\b/)?.[1];
    assert.ok(phaseName, `${label} needs typed cleanup phase state`);
    const phase = itemBlock(library, "enum", phaseName);
    const abandonedVariant = phase.match(/\bAbandoned\b/)?.[0];
    const attemptedVariant = phase.match(
      /\b(?:CleanupAttempted|DeleteApplied|DeletionAttempted|RemovalAttempted|AppliedDelete|RemovedUnsynced)\b/,
    )?.[0];
    assert.ok(
      abandonedVariant && attemptedVariant,
      `${label} must distinguish first cleanup from a possibly applied delete`,
    );

    const cleanup = functionBlocks(authority).find(({ name }) =>
      cleanupPattern.test(name),
    );
    assert.ok(cleanup, `${label} needs terminal abandoned-create cleanup`);
    const settlementCall = `platform::${settleOperation}`;
    const removalCall = `platform::${removeOperation}`;
    assert.match(
      cleanup.source,
      new RegExp(
        `${escapeRegExp(attemptedVariant)}[\\s\\S]*${escapeRegExp(settlementCall)}`,
      ),
      `${label} cleanup-attempted phase must reach typed settlement`,
    );
    const settlementIndex = cleanup.source.indexOf(settlementCall);
    const attemptedGuardIndex = cleanup.source.lastIndexOf(
      attemptedVariant,
      settlementIndex,
    );
    const fallbackIndex = cleanup.source.indexOf(removalCall, settlementIndex);
    const attemptedFlow =
      attemptedGuardIndex !== -1 && fallbackIndex !== -1
        ? cleanup.source.slice(
            attemptedGuardIndex,
            fallbackIndex + removalCall.length,
          )
        : undefined;
    assert.ok(
      attemptedFlow,
      `${label} attempted cleanup must settle first and remove only as a fallback`,
    );
    assertOrdered(
      attemptedFlow,
      settlementCall,
      removalCall,
      `${label} applied-delete settlement before remove fallback`,
    );
    assert.match(
      attemptedFlow,
      /\belse\b|\.or_else\s*\(/,
      `${label} remove must be only the fallback after attempted settlement`,
    );
    assert.match(
      cleanup.source,
      new RegExp(
        `\\b${escapeRegExp(abandonedVariant)}\\b[\\s\\S]*${escapeRegExp(removalCall)}`,
      ),
      `${label} first abandoned cleanup must perform the initial remove`,
    );
    const attemptedAssignment = cleanup.source.match(
      new RegExp(
        `(?:phase|state)\\s*=\\s*(?:${escapeRegExp(phaseName)}::)?${escapeRegExp(attemptedVariant)}`,
      ),
    )?.[0];
    const registryReinsert = cleanup.source.match(/\.insert\s*\(/)?.[0];
    const release = cleanup.source.match(/\.release_effect\s*\(/)?.[0];
    assert.ok(
      attemptedAssignment && registryReinsert && release,
      `${label} ambiguous cleanup must retain retry phase and proof-gated permit release`,
    );
    assert.ok(
      cleanup.source.indexOf(attemptedAssignment) <
        cleanup.source.lastIndexOf(registryReinsert),
      `${label} cleanup-attempted state must precede unresolved record reinsertion`,
    );
    assert.ok(
      cleanup.source.lastIndexOf(settlementCall) <
        cleanup.source.lastIndexOf(release) &&
        cleanup.source.lastIndexOf(removalCall) <
          cleanup.source.lastIndexOf(release),
      `${label} cannot release its shared effect before removal settlement proof`,
    );
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
    const operation = functionBlocks(library).find(
      ({ source }) =>
        new RegExp(`->\\s*${outcome}\\b`).test(source) &&
        /\.enter\s*\(\)/.test(source) &&
        effectExpression.test(source),
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
    const reservationEntry = functionBlock(library, reservationName);
    const reservationFlow = reachableFunctionBlocks(
      library,
      reservationEntry,
    ).find(({ source }) =>
      new RegExp(
        `\\b${kind === "file" ? escapeRegExp(fileRegistryName) : escapeRegExp(directoryRegistryName)}\\s*\\.\\s*insert\\s*\\(`,
      ).test(source),
    )?.source;
    assert.ok(
      reservationFlow,
      `${kind} park needs one atomic registration owner`,
    );
    const liveReservation = reservationFlow.match(
      /AUTHORITY_LIVE|\bLive\b/,
    )?.[0];
    const registryInsert = reservationFlow.match(
      new RegExp(
        `\\b${kind === "file" ? escapeRegExp(fileRegistryName) : escapeRegExp(directoryRegistryName)}\\s*\\.\\s*insert\\s*\\(`,
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

  const recovery = [...library.matchAll(/(?:^|\n)struct ([A-Za-z0-9_]+)\s*\{/g)]
    .map((match) => ({
      name: match[1],
      source: itemBlock(library, "struct", match[1]),
    }))
    .find(
      ({ source }) =>
        /(?:Vec|Option|Box)<ParkedFile>/.test(source) &&
        /(?:Vec|Option|Box)<ParkedDirectory>/.test(source),
    );
  assert.ok(recovery, "root drain must expose typed terminal recovery");
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
  const quiescingAssignment = beginDrain.match(
    /\b(?:[a-z_]+\.)?(?:phase|state)\s*=\s*(?:AUTHORITY_QUIESCING|Quiescing)\b/,
  )?.[0];
  assert.ok(
    quiescingAssignment,
    "terminal start needs an explicit QUIESCING admission phase",
  );
  assertOrdered(
    beginDrain,
    activeRefusal,
    quiescingAssignment,
    "active-operation refusal before QUIESCING",
  );
  assertOrdered(
    beginDrain,
    quiescingAssignment,
    drainingAssignment,
    "QUIESCING before DRAINING",
  );
  assert.match(
    beginDrain,
    /TerminalQuiescingRollback\s*::\s*new\s*\(/,
    "terminal admission must arm rollback immediately after entering QUIESCING",
  );
  assert.match(
    library,
    /impl\s+Drop\s+for\s+TerminalQuiescingRollback[^\{]*\{[\s\S]{0,500}?restore_live_after_quiescing\s*\(/,
    "every post-QUIESCING refusal must restore LIVE through the rollback guard",
  );
  const terminalStartFlow = uniqueReachableFunctions(library, beforeDraining);
  const startRefusals = conditionalBlocks(terminalStartFlow).filter(({ body }) =>
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
    "terminal recovery state cannot detach from its typed drain owner",
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
  const rootCapability = functionBlock(rootSessionImplementation, "root");
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
  const rootSessionDropFlow = uniqueReachableFunctions(library, rootSessionDrop);
  assert.doesNotMatch(
    rootSessionDrop,
    /\.wait(?:_while)?\(|while\s+[^\{]*(?:active|in_flight|operations)|mem::forget\s*\(|ManuallyDrop/,
    "RootSession drop cannot wait for, detach, or forget live authority",
  );
  assert.doesNotMatch(
    rootSessionDrop,
    /AUTHORITY_RESETTING|AUTHORITY_REVOKED|\bResetting\b|\bRevoked\b/,
    "RootSession drop cannot claim terminal settlement",
  );
  const dropLock = rootSessionDropFlow.match(
    /\.operations\s*\.\s*lock\s*\(\s*\)/,
  )?.[0];
  const dropDraining = rootSessionDropFlow.match(
    /(?:phase|state)\s*=\s*(?:AUTHORITY_DRAINING|[A-Za-z0-9_]+::Draining)/,
  )?.[0];
  assert.ok(
    dropLock && dropDraining,
    "RootSession drop must lock the gate and close LIVE ingress",
  );
  assertOrdered(
    rootSessionDropFlow,
    dropLock,
    dropDraining,
    "drop gate lock before ingress closure",
  );
  const poisonedDrop = matchArmBlocks(
    rootSessionDropFlow,
    /Err\s*\([^)]*\)/,
  ).find(
    ({ body }) => /std::process::abort\s*\(\s*\)/.test(body),
  );
  assert.ok(
    poisonedDrop,
    "an unprovable poisoned RootSession gate must fail-stop",
  );
  const operationState = itemBlock(library, "struct", "OperationState");
  const activeField = operationState.match(
    /([a-z_]*(?:active|in_flight|operations)[a-z_]*):\s*usize\b/,
  )?.[1];
  const effectField = operationState.match(
    /([a-z_]*(?:outstanding|retained)[a-z_]*effects[a-z_]*):\s*usize\b/,
  )?.[1];
  const registryFields = [
    ...operationState.matchAll(
      /\b([a-z_][a-z0-9_]*):\s*(?:HashMap|BTreeMap)<|\b([a-z_][a-z0-9_]*):\s*Vec</g,
    ),
  ].map((match) => match[1] ?? match[2]);
  const checkedOutFields = [
    ...operationState.matchAll(
      /\b([a-z_][a-z0-9_]*checked_out[a-z0-9_]*):\s*usize\b/g,
    ),
  ].map((match) => match[1]);
  assert.ok(
    activeField &&
      effectField &&
      registryFields.length > 0 &&
      checkedOutFields.length > 0,
    "RootSession fail-stop proof needs the complete operation state shape",
  );
  for (const field of [activeField, effectField, ...checkedOutFields]) {
    assert.ok(
      conditionalBlocks(rootSessionDrop).some(
        ({ condition, body }) =>
          new RegExp(`\\b${escapeRegExp(field)}\\b\\s*(?:!=|>)\\s*0`).test(
            condition,
          ) && /std::process::abort\s*\(\s*\)/.test(body),
      ),
      `RootSession drop must abort while ${field} is nonzero`,
    );
  }
  for (const field of registryFields.filter(
    (field) => field !== "effect_owner_handles",
  )) {
    assert.ok(
      conditionalBlocks(rootSessionDrop).some(
        ({ condition, body }) =>
          new RegExp(
            `!\\s*(?:state|guard|operations)\\.${escapeRegExp(field)}\\.is_empty\\s*\\(\\s*\\)`,
          ).test(condition) && /std::process::abort\s*\(\s*\)/.test(body),
      ),
      `RootSession drop must abort while ${field} retains an effect`,
    );
  }
  assert.match(
    rootSessionDropFlow,
    /effect_owner_handles[\s\S]*?retain\([\s\S]*?strong_count\(\)\s*>\s*0/,
    "terminal drain must discard dead weak effect-owner handles",
  );
  assert.match(
    rootSessionDropFlow,
    /has_external_owner[\s\S]*?strong_count\(\)\s*>\s*authority_owned[\s\S]*?if has_external_owner[\s\S]*?return Err\(/,
    "terminal drain must refuse every live external effect-owner handle",
  );
  const finalDropAbort = rootSessionDrop.lastIndexOf("std::process::abort");
  assert.ok(
    finalDropAbort > rootSessionDrop.indexOf(dropDraining),
    "RootSession drop may release only after closing ingress and proving complete quiescence",
  );
  assert.doesNotMatch(
    rootSessionDrop.slice(rootSessionDrop.indexOf(dropLock), finalDropAbort),
    /drop\s*\(\s*(?:state|guard|operations)\s*\)/,
    "drop must retain the same gate lock through its fail-stop proof",
  );
  for (const carrier of [
    "RootRevokeDrain",
    "RootRevokeRecovery",
    "RootRevokeStartFailure",
    "RootRevokeDrainFailure",
    "ResetDrainAuthority",
    "ResetDrainRecovery",
    "ResetStartFailure",
    "ResetDrainFailure",
    "RootClearFailure",
  ]) {
    const customDrop = new RegExp(`impl\\s+Drop\\s+for\\s+${carrier}\\b`).test(
      library,
    )
      ? traitImplementationBlock(library, "Drop", carrier)
      : "";
    assert.doesNotMatch(
      customDrop,
      /mem::forget\s*\(|ManuallyDrop/,
      `${carrier} drop must transitively retain fail-stop session ownership`,
    );
  }
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
  const resetImageValidation = resetClear.source.match(
    /platform::validate_process_image_outside_root\s*\(|\b([a-z_]*(?:validate|revalidate)[a-z_]*(?:process_image|image_ancestry|reset_safety)[a-z_]*)\s*\(/,
  )?.[0];
  const destructiveClear = resetClear.source.match(
    new RegExp(`platform::${escapeRegExp(platformClearName)}\\s*\\(`),
  )?.[0];
  assert.ok(
    resetImageValidation && destructiveClear,
    "retained reset authority must revalidate process-image ancestry immediately before clear",
  );
  assert.match(
    uniqueReachableFunctions(library, resetClear.source),
    /platform::validate_process_image_outside_root\s*\(/,
    "the final reset-safety call must reach native process-image ancestry validation",
  );
  assertOrdered(
    resetClear.source,
    resetImageValidation,
    destructiveClear,
    "reset process-image revalidation before destructive clear",
  );
  const validationToClear = resetClear.source.slice(
    resetClear.source.lastIndexOf(resetImageValidation),
    resetClear.source.indexOf(destructiveClear) + destructiveClear.length,
  );
  assert.doesNotMatch(
    validationToClear.replace(resetImageValidation, ""),
    /\.await\b|(?:sleep|yield_now|spawn|enter_[a-z_]*|try_settle)\s*\(/,
    "reset clear cannot admit an interposed workflow after its final process-image proof",
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
    const finalConditionEnd = Math.max(
      finalProofSource.indexOf(completeCondition.source) +
        completeCondition.source.length,
      finalProofSource.indexOf(emptyCondition.source) +
        emptyCondition.source.length,
    );
    const finalBindingProof = finalProofSource.slice(finalConditionEnd);
    assert.match(
      finalBindingProof,
      new RegExp(
        `\\bvalidate_root\\s*\\(\\s*${escapeRegExp(proofRootParameter)}\\s*\\)(?:\\s*\\?|\\s*\\}\\s*$)`,
      ),
      `${platformName} final empty listing must be followed by exact configured-root binding validation`,
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
      const proofLeaseParameter = finalProofSource
        .slice(0, finalProofSource.indexOf("{"))
        .match(/\b([a-z_][a-z0-9_]*):\s*&LeaseHandle\b/)?.[1];
      assert.ok(
        proofLeaseParameter,
        "Windows final root proof must retain its exact lease authority",
      );
      assert.match(
        finalBindingProof,
        new RegExp(
          `\\bvalidate_lease\\s*\\(\\s*${escapeRegExp(proofLeaseParameter)}\\s*\\)\\s*\\?`,
        ),
        "Windows final root proof must revalidate the retained lease after enumeration",
      );
      assert.match(
        finalBindingProof,
        new RegExp(
          `file_binding_state\\s*\\([^;]*\\b${escapeRegExp(proofRootParameter)}\\b[^;]*\\b${escapeRegExp(proofLeaseParameter)}\\.identity\\b[^;]*\\)[\\s\\S]{0,100}?BindingState::Exact`,
        ),
        "Windows final root proof must revalidate the exact lease binding after enumeration",
      );
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
  const admissionFlow = uniqueReachableFunctions(library, admission.source);
  assert.match(admissionFlow, /\.is_absolute\(\)/);
  assert.match(admissionFlow, /Arc::downgrade\(&self\.authority\)/);
  assert.doesNotMatch(
    admissionFlow,
    /Arc::new\(\s*CapabilityAuthority|RootSession::acquire/,
    "external roots cannot mint an independent authority or lease",
  );
  assert.match(
    admissionFlow,
    /AbsoluteDirectoryGuard[\s\S]*?from_absolute_handle\(/,
    "external root admission must retain its absolute ancestry guard",
  );

  const platformAdmission = admissionFlow.match(
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

test("P01-B02 fail-stops unresolved acquisition and root-clear authority", async () => {
  const library = await read("core/fs/src/lib.rs");
  for (const carrier of ["RootSessionAcquireObligation", "RootClearFailure"]) {
    const drop = traitImplementationBlock(library, "Drop", carrier);
    assert.doesNotMatch(drop, /mem::forget\s*\(|ManuallyDrop/);
    assert.ok(
      conditionalBlocks(drop).some(
        ({ condition, body }) =>
          /(?:\.is_some\s*\(\s*\)|\barmed\b)/.test(condition) &&
          /std::process::abort\s*\(\s*\)/.test(body),
      ),
      `${carrier} must fail-stop while it retains unresolved authority`,
    );
  }
});

test("P01-B02 reset failures retain explicit cancellation exits", async () => {
  const library = await read("core/fs/src/lib.rs");
  for (const carrier of ["ResetStartFailure", "ResetDrainFailure"]) {
    const drop = traitImplementationBlock(library, "Drop", carrier);
    assert.ok(
      conditionalBlocks(drop).some(
        ({ condition, body }) =>
          /(?:\.is_some\s*\(\s*\)|\barmed\b)/.test(condition) &&
          /std::process::abort\s*\(\s*\)/.test(body),
      ),
      `${carrier} must fail-stop while it retains reset authority`,
    );
  }

  const startCancel = functionBlock(
    implementationBlock(library, "ResetStartFailure"),
    "cancel_reset",
  );
  assert.match(
    startCancel.slice(0, startCancel.indexOf("{")),
    /pub fn cancel_reset\s*\(\s*(?:mut\s+)?self\s*\)\s*->\s*RootSession\b/,
    "reset refusal must return its still-LIVE RootSession explicitly",
  );
  assert.match(startCancel, /\.take\s*\(\s*\)/);

  const drainCancel = functionBlock(
    implementationBlock(library, "ResetDrainFailure"),
    "cancel_reset",
  );
  assert.match(
    drainCancel.slice(0, drainCancel.indexOf("{")),
    /pub fn cancel_reset\s*\(\s*(?:mut\s+)?self\s*\)\s*->\s*RootRevokeOutcome\b/,
    "failed DRAINING reset cancellation must continue through revocation",
  );
  assert.match(drainCancel, /\.take\s*\(\s*\)/);
  const drainType = itemBlock(library, "struct", "ResetDrainFailure").match(
    /drain:\s*Option<([A-Za-z0-9_]+)>/,
  )?.[1];
  assert.ok(drainType, "reset drain failure must retain typed drain authority");
  const drainAuthorityCancel = functionBlock(
    implementationBlock(library, drainType),
    "cancel_reset",
  );
  const resetPendingCancellation = functionBlocks(library).find(
    ({ source }) =>
      /DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending/.test(
        source,
      ) &&
      /record\.phase\s*=\s*DirectoryCreateEffectPhase::UnclassifiedAbandoned/.test(
        source,
      ),
  );
  assert.ok(
    resetPendingCancellation,
    "reset cancellation must demote every reset-pending create to abandoned recovery",
  );
  const transitionCall = new RegExp(
    `\\b${escapeRegExp(resetPendingCancellation.name)}\\s*\\(`,
  ).exec(drainAuthorityCancel)?.[0];
  const revokeRecovery = drainAuthorityCancel.match(
    /(?:let\s+[a-z_][a-z0-9_]*\s*=\s*)?RootRevokeDrain\s*\{/,
  )?.[0];
  assert.ok(
    transitionCall && revokeRecovery,
    "DRAINING reset cancellation must invoke reset-pending demotion before revoke recovery",
  );
  assertOrdered(
    drainAuthorityCancel,
    transitionCall,
    revokeRecovery,
    "reset-pending demotion before revoke recovery",
  );
  assert.doesNotMatch(
    `${drainCancel}\n${drainAuthorityCancel}\n${resetPendingCancellation.source}`,
    /(?:phase|state)\s*=\s*(?:AUTHORITY_LIVE|[A-Za-z0-9_]+::Live)\b/,
    "DRAINING reset cancellation cannot republish LIVE authority",
  );
});

test("P01-B02 keeps read completion advisory and drain recovery internal", async () => {
  const library = await read("core/fs/src/lib.rs");
  assertMustUse(library, "struct", "FileReader");
  const readerDropName = library.match(
    /impl\s+Drop\s+for\s+(FileReader(?:<[^>{}]+>)?)\s*\{/,
  )?.[1];
  if (readerDropName) {
    assert.doesNotMatch(
      traitImplementationBlock(library, "Drop", readerDropName),
      /std::process::abort\s*\(\s*\)/,
      "dropping a partial read must release admission without aborting",
    );
  }
  assert.match(library, /(?:^|\n)struct SessionDrainRecoveryState\b/);
  assert.doesNotMatch(
    library,
    /(?:^|\n)\s*pub(?:\([^)]*\))?\s+struct SessionDrainRecoveryState\b/,
    "internal drain bookkeeping must not be a public API type",
  );
});

test("P01-B02 parks exact files at caller-named leaves without a second rename framework", async () => {
  const [library, platform, workspaceManifest, fsManifest] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
    read("Cargo.toml"),
    read("core/fs/Cargo.toml"),
  ]);
  assertPlatformLeafEquivalence({ platform, workspaceManifest, fsManifest });
  const namedPark = uniqueMethodBlock(library, "Directory", "park_file_as");
  const header = namedPark.slice(0, namedPark.indexOf("{"));
  const parkName = header.match(/\b(park_name|destination):\s*LeafName\b/)?.[1];
  assert.match(header, /request:\s*FileParkRequest\b/);
  assert.match(header, /->\s*FileParkOutcome\b/);
  assert.ok(parkName, "named park must consume one validated LeafName");
  const fileReservation = callBlocks(namedPark, /\breserve_file_park\s*\(/)[0];
  const fileMutation = callBlocks(
    namedPark,
    /\bplatform::park_file_no_replace\s*\(/,
  )[0];
  assert.ok(fileReservation && fileMutation);
  const fileReservationArguments = callArguments(fileReservation.source);
  const fileMutationArguments = callArguments(fileMutation.source);
  assert.equal(fileReservationArguments.length, 4);
  assert.equal(fileMutationArguments.length, 6);
  assert.match(
    fileReservationArguments[2],
    new RegExp(`\\b${escapeRegExp(parkName)}\\s*\\.\\s*clone\\s*\\(\\s*\\)`),
  );
  assert.match(
    fileMutationArguments[4],
    new RegExp(
      `\\b${escapeRegExp(parkName)}\\s*\\.\\s*as_os_str\\s*\\(\\s*\\)`,
    ),
  );
  const sameFileName = conditionalBlocks(namedPark).find(({ condition }) =>
    /platform::leaf_names_equal\s*\(/.test(condition),
  );
  assert.ok(
    sameFileName,
    "caller-named file parks must reject the current source leaf",
  );
  const sameFileCall = callBlocks(
    sameFileName.condition,
    /platform::leaf_names_equal\s*\(/,
  )[0];
  assert.deepEqual(callArguments(sameFileCall.source), [
    "request.file.name.as_os_str()",
    `${parkName}.as_os_str()`,
  ]);
  assert.match(
    sameFileName.body,
    /FileParkOutcome::NoEffect\s*\{[\s\S]{0,240}\brequest\s*(?:,|\})/,
    "same-source refusal must return the input FileParkRequest",
  );
  assert.ok(
    namedPark.indexOf(sameFileName.source) < fileReservation.index &&
      fileReservation.index < fileMutation.index,
    "same-source file rejection must precede effect reservation",
  );
  assert.match(namedPark, /take_file_park\s*\(/);
  assert.match(namedPark, /finish_new_file_park\s*\(/);
  assertAtomicParkRegistration({
    library,
    admission: namedPark,
    registry: "file_parks",
    owners: "park_owners",
    keyType: "ParkRegistryKey",
    ownerVariant: "File",
    label: "named file-park",
  });
  assertPersistentParkOwnership({
    library,
    registry: "file_parks",
    owners: "park_owners",
    ownerVariant: "File",
    tokenType: "FileParkRegistryToken",
    takeMethod: "take_file_park",
    guardType: "FileParkRecordGuard",
    label: "file-park",
  });
  assert.doesNotMatch(
    namedPark,
    /random_leaf\s*\(|OsRng|thread_rng|fill_bytes\s*\(/,
    "caller-named park cannot replace its destination with a random leaf",
  );
  const randomPark = uniqueMethodBlock(library, "Directory", "park_file");
  assert.match(randomPark, /random_leaf\s*\(/);
  assert.match(randomPark, /park_file_as\s*\(/);
  assert.doesNotMatch(
    randomPark,
    /reserve_file_park\s*\(|platform::park_file_no_replace\s*\(/,
    "random and caller-named parks must share one typed implementation",
  );
  assert.doesNotMatch(
    library,
    /pub\s+(?:struct|enum)\s+FileRename[A-Za-z0-9_]*\b|pub\s+fn\s+[a-z_]*rename[a-z_]*\s*\([^)]*FileCapability/,
    "named quarantine must reuse the existing typed park state machine",
  );

  const namedDirectoryPark = uniqueMethodBlock(library, "Directory", "park_as");
  assert.match(
    namedDirectoryPark.slice(0, namedDirectoryPark.indexOf("{")),
    /pub fn park_as\s*\(\s*self\s*,\s*park_name:\s*LeafName\s*\)\s*->\s*DirectoryParkOutcome\b/,
  );
  const directoryReservation = callBlocks(
    namedDirectoryPark,
    /\breserve_directory_park\s*\(/,
  )[0];
  const directoryMutation = callBlocks(
    namedDirectoryPark,
    /\bplatform::park_directory_no_replace\s*\(/,
  )[0];
  assert.ok(directoryReservation && directoryMutation);
  const directoryReservationArguments = callArguments(
    directoryReservation.source,
  );
  const directoryMutationArguments = callArguments(directoryMutation.source);
  assert.equal(directoryReservationArguments.length, 6);
  assert.equal(directoryMutationArguments.length, 6);
  assert.match(
    directoryReservationArguments[4],
    /\bpark_name\s*\.\s*clone\s*\(\s*\)/,
  );
  assert.match(
    directoryMutationArguments[4],
    /\bpark_name\s*\.\s*as_os_str\s*\(\s*\)/,
  );
  const sameDirectoryName = conditionalBlocks(namedDirectoryPark).find(
    ({ condition }) => /platform::leaf_names_equal\s*\(/.test(condition),
  );
  assert.ok(
    sameDirectoryName,
    "caller-named directory parks must reject the current source leaf",
  );
  const sameDirectoryCall = callBlocks(
    sameDirectoryName.condition,
    /platform::leaf_names_equal\s*\(/,
  )[0];
  assert.deepEqual(callArguments(sameDirectoryCall.source), [
    "original_name.as_os_str()",
    "park_name.as_os_str()",
  ]);
  assert.match(sameDirectoryName.body, /DirectoryParkOutcome::NoEffect/);
  assert.match(sameDirectoryName.body, /directory:\s*self\b/);
  assert.ok(
    namedDirectoryPark.indexOf(sameDirectoryName.source) <
      directoryReservation.index &&
      directoryReservation.index < directoryMutation.index,
    "same-source directory rejection must precede effect reservation",
  );
  assert.match(namedDirectoryPark, /take_directory_park\s*\(/);
  assert.match(namedDirectoryPark, /DirectoryParkRegistryPhase::Live/);
  assertAtomicParkRegistration({
    library,
    admission: namedDirectoryPark,
    registry: "directory_parks",
    owners: "park_owners",
    keyType: "ParkRegistryKey",
    ownerVariant: "Directory",
    label: "named directory-park",
  });
  assertPersistentParkOwnership({
    library,
    registry: "directory_parks",
    owners: "park_owners",
    ownerVariant: "Directory",
    tokenType: "DirectoryParkRegistryToken",
    takeMethod: "take_directory_park",
    guardType: "DirectoryParkRecordGuard",
    label: "directory-park",
  });
  assert.doesNotMatch(
    namedDirectoryPark,
    /random_leaf\s*\(|OsRng|thread_rng|fill_bytes\s*\(/,
  );
  const randomDirectoryPark = uniqueMethodBlock(library, "Directory", "park");
  assert.match(randomDirectoryPark, /random_leaf\s*\(/);
  assert.match(randomDirectoryPark, /\.park_as\s*\(/);
  assert.doesNotMatch(
    randomDirectoryPark,
    /reserve_directory_park\s*\(|platform::park_directory_no_replace\s*\(/,
    "random and caller-named directory parks must share one typed implementation",
  );
  assert.doesNotMatch(
    library,
    /pub\s+(?:struct|enum)\s+DirectoryRename[A-Za-z0-9_]*\b|pub\s+fn\s+[a-z_]*rename[a-z_]*\s*\([^)]*Directory/,
    "named directory quarantine must reuse the existing park state machine",
  );
});

test("P01-B02 admits exact existing file parks without replaying mutation", async () => {
  const library = await read("core/fs/src/lib.rs");
  const admission = uniqueMethodBlock(
    library,
    "Directory",
    "admit_existing_file_park",
  );
  assert.match(
    admission.slice(0, admission.indexOf("{")),
    /pub fn admit_existing_file_park\s*\(\s*&self\s*,\s*original_name:\s*&LeafName\s*,\s*parked:\s*FileParkRequest\s*,?\s*\)\s*->\s*io::Result\s*<\s*ParkedFile\s*>/,
  );
  const sameSource = callBlocks(
    admission,
    /platform::leaf_names_equal\s*\(/,
  )[0];
  assert.deepEqual(callArguments(sameSource.source), [
    "original_name.as_os_str()",
    "parked.file.name.as_os_str()",
  ]);
  const flow = uniqueReachableFunctions(library, admission);
  assert.match(
    flow,
    /validate_bound_to\s*\(|(?:Arc|Weak)::ptr_eq\s*\(/,
    "file-park admission must prove the same authority and parent",
  );
  assertCountAtLeast(
    flow,
    /file_binding_state\s*\(/,
    2,
    "file-park admission must observe original and parked bindings",
  );
  assert.match(flow, /BindingState::Absent/);
  assert.match(flow, /BindingState::Exact/);
  assert.match(flow, /verify_parked_(?:file|revision)\s*\(/);
  assert.match(flow, /(?:hash_parked_file|sha256)/i);
  assert.match(flow, /expected\.sha256|expected_digest/);
  const requestValidation = uniqueMethodBlock(
    library,
    "FileParkRequest",
    "validate_revision",
  );
  assert.match(
    requestValidation.slice(0, requestValidation.indexOf("{")),
    /operation:\s*&CapabilityOperation/,
  );
  assert.match(
    requestValidation,
    /\.validate_revision_in\s*\(\s*operation\s*,\s*&self\.expected\.revision\s*\)/,
  );
  assert.doesNotMatch(requestValidation, /\.enter\s*\(/);
  assert.equal(admission.match(/\.enter\s*\(/g)?.length ?? 0, 1);
  const admissionRevisions = callBlocks(
    admission,
    /parked\s*\.\s*validate_revision\s*\(/,
  ).filter(
    ({ source }) =>
      callArguments(source).length === 1 &&
      callArguments(source)[0] === "&operation",
  );
  assert.ok(
    admissionRevisions.length >= 3,
    "file admission must reobserve its revision around publication and proof",
  );
  const registrationOwner = assertAtomicParkRegistration({
    library,
    admission,
    registry: "file_parks",
    owners: "park_owners",
    keyType: "ParkRegistryKey",
    ownerVariant: "File",
    label: "file-park",
  });
  const registrationBlock = registrationOwner.source;
  const registration = registrationBlock.match(
    /file_parks\s*\.\s*insert\s*\(/,
  )?.[0];
  assert.ok(registration);
  assert.match(registrationBlock, /FileParkRegistryToken\s*\{/);
  assert.match(registrationBlock, /armed:\s*true/);
  const liveRegistration = callBlocks(
    admission,
    new RegExp(`\\b${escapeRegExp(registrationOwner.name)}\\s*\\(`),
  )[0];
  assert.ok(liveRegistration);
  assert.equal(
    callArguments(liveRegistration.source).at(-1),
    "FileParkRegistryPhase::Live",
  );
  assert.match(
    registrationBlock.slice(registrationBlock.indexOf(registration)),
    /\bphase\s*(?:,|:\s*phase\b)/,
  );
  assertAdmissionRevalidationRollback({
    library,
    admission,
    registrationOwner,
    inlineRegistration: /file_parks\s*\.\s*insert\s*\(/,
    proof: /verify_parked_(?:file|revision)\s*\(/,
    requiredPreProof: /verify_parked_file\s*\(/,
    registry: "file_parks",
    owners: "park_owners",
    ownerVariant: "File",
    tokenType: "FileParkRegistryToken",
    label: "file-park",
  });
  assert.doesNotMatch(
    admission,
    /platform::(?:park_file_no_replace|remove_[a-z_]*|restore_[a-z_]*|rename_[a-z_]*|create_[a-z_]*|write_at|sync_directory)\s*\(/,
    "existing file-park admission must not replay a native effect",
  );
});

test("P01-B02 admits exact existing directory parks without replaying mutation", async () => {
  const library = await read("core/fs/src/lib.rs");
  const admission = uniqueMethodBlock(
    library,
    "Directory",
    "admit_existing_directory_park",
  );
  assert.match(
    admission.slice(0, admission.indexOf("{")),
    /pub fn admit_existing_directory_park\s*\(\s*&self\s*,\s*original_name:\s*&LeafName\s*,\s*parked:\s*Directory\s*,\s*expected:\s*&?DirectoryRevision\s*,?\s*\)\s*->\s*io::Result\s*<\s*ParkedDirectory\s*>/,
  );
  const sameSource = callBlocks(
    admission,
    /platform::leaf_names_equal\s*\(/,
  )[0];
  assert.deepEqual(callArguments(sameSource.source), [
    "original_name.as_os_str()",
    "park_name.as_os_str()",
  ]);
  const flow = uniqueReachableFunctions(library, admission);
  assert.match(
    flow,
    /(?:Arc|Weak)::ptr_eq\s*\(/,
    "directory-park admission must prove the same authority and parent",
  );
  assert.match(flow, /\.parent/);
  assertCountAtLeast(
    flow,
    /directory_binding_state\s*\(/,
    2,
    "directory-park admission must observe original and parked bindings",
  );
  assert.match(flow, /BindingState::Absent/);
  assert.match(flow, /BindingState::Exact/);
  assert.match(flow, /validate_revision_in\s*\(/);
  const directoryValidationIn = uniqueMethodBlock(
    library,
    "Directory",
    "validate_revision_in",
  );
  assert.match(
    directoryValidationIn.slice(0, directoryValidationIn.indexOf("{")),
    /fn\s+validate_revision_in\s*\(\s*&self\s*,\s*operation:\s*&CapabilityOperation\s*,\s*expected:\s*&DirectoryRevision\s*,?\s*\)\s*->\s*io::Result\s*<\s*\(\s*\)\s*>/,
  );
  assert.doesNotMatch(
    directoryValidationIn.slice(0, directoryValidationIn.indexOf("{")),
    /\bpub\b/,
  );
  assert.match(directoryValidationIn, /platform::directory_revision\s*\(/);
  assert.match(directoryValidationIn, /operation\.authority/);
  assert.doesNotMatch(
    directoryValidationIn,
    /\.enter\s*\(|\.authority\s*\(\s*\)/,
  );
  assert.equal(admission.match(/\.enter\s*\(/g)?.length ?? 0, 1);
  assert.doesNotMatch(
    admission,
    /parked\s*\.\s*validate_revision\s*\(\s*expected\s*\)/,
    "directory admission cannot re-enter through the public revision validator",
  );
  const admissionRevisions = callBlocks(
    admission,
    /parked\s*\.\s*validate_revision_in\s*\(/,
  ).filter(({ source }) =>
    /^&operation\s*,\s*expected$/.test(callArguments(source).join(", ")),
  );
  assert.ok(
    admissionRevisions.length >= 3,
    "directory admission must reobserve its revision around publication",
  );
  const registrationOwner = assertAtomicParkRegistration({
    library,
    admission,
    registry: "directory_parks",
    owners: "park_owners",
    keyType: "ParkRegistryKey",
    ownerVariant: "Directory",
    label: "directory-park",
  });
  const registrationBlock = registrationOwner.source;
  const registration = registrationBlock.match(
    /directory_parks\s*\.\s*insert\s*\(/,
  )?.[0];
  assert.ok(registration);
  assert.match(registrationBlock, /DirectoryParkRegistryToken\s*\{/);
  assert.match(registrationBlock, /armed:\s*true/);
  const liveRegistration = callBlocks(
    admission,
    new RegExp(`\\b${escapeRegExp(registrationOwner.name)}\\s*\\(`),
  )[0];
  assert.ok(liveRegistration);
  assert.equal(
    callArguments(liveRegistration.source).at(-1),
    "DirectoryParkRegistryPhase::Live",
  );
  assert.match(
    registrationBlock.slice(registrationBlock.indexOf(registration)),
    /\bphase\s*(?:,|:\s*phase\b)/,
  );
  assertAdmissionRevalidationRollback({
    library,
    admission,
    registrationOwner,
    inlineRegistration: /directory_parks\s*\.\s*insert\s*\(/,
    proof:
      /parked\s*\.\s*validate_revision_in\s*\(\s*&operation\s*,\s*expected\s*\)/,
    registry: "directory_parks",
    owners: "park_owners",
    ownerVariant: "Directory",
    tokenType: "DirectoryParkRegistryToken",
    label: "directory-park",
  });
  assert.doesNotMatch(
    admission,
    /platform::(?:park_directory_no_replace|remove_[a-z_]*|restore_[a-z_]*|rename_[a-z_]*|create_[a-z_]*|write_at|sync_directory)\s*\(/,
    "existing directory-park admission must not replay a native effect",
  );
});

test("P01-B02 exposes exact bounded file revision evidence", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const validateRevision = uniqueMethodBlock(
    library,
    "FileCapability",
    "validate_revision",
  );
  assert.match(
    validateRevision.slice(0, validateRevision.indexOf("{")),
    /&self[\s\S]*?expected:\s*&FileRevision\b[\s\S]*?->\s*io::Result\s*<\s*\(\s*\)\s*>/,
  );
  const validateRevisionIn = uniqueMethodBlock(
    library,
    "FileCapability",
    "validate_revision_in",
  );
  assert.match(
    validateRevisionIn.slice(0, validateRevisionIn.indexOf("{")),
    /fn\s+validate_revision_in\s*\(\s*&self\s*,\s*operation:\s*&CapabilityOperation\s*,\s*expected:\s*&FileRevision\s*,?\s*\)\s*->\s*io::Result\s*<\s*\(\s*\)\s*>/,
  );
  assert.doesNotMatch(
    validateRevisionIn.slice(0, validateRevisionIn.indexOf("{")),
    /\bpub\b/,
  );
  assert.match(validateRevisionIn, /file_receipt_fields\s*\(/);
  assert.match(validateRevisionIn, /operation\.authority/);
  assert.match(validateRevisionIn, /identity/);
  assert.doesNotMatch(validateRevisionIn, /\.enter\s*\(|\.authority\s*\(\s*\)/);
  const publicValidation = callBlocks(
    validateRevision,
    /self\s*\.\s*validate_revision_in\s*\(/,
  )[0];
  assert.ok(publicValidation);
  assert.deepEqual(callArguments(publicValidation.source), [
    "&operation",
    "expected",
  ]);
  assert.equal(validateRevision.match(/\.enter\s*\(/g)?.length ?? 0, 1);
  assertCountAtLeast(
    validateRevision,
    /(?:self|expected)\.validate\s*\(/,
    2,
    "file revision validation must prove binding before and after its receipt check",
  );

  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const unixReceipt = functionBlock(unix, "file_receipt_fields");
  const windowsReceipt = functionBlock(windows, "file_receipt_fields");
  assert.match(
    windows,
    /const\s+WINDOWS_TO_UNIX_EPOCH_TICKS:\s*u64\s*=\s*116_444_736_000_000_000\s*;/,
    "Windows timestamps need the exact checked Unix epoch conversion",
  );
  for (const [target, origin] of [
    ["modified_seconds", /\bstat\.st_mtime\b/],
    ["modified_nanos", /\bstat\.st_mtime_nsec\b/],
    ["changed_seconds", /\bstat\.st_ctime\b/],
    ["changed_nanos", /\bstat\.st_ctime_nsec\b/],
  ]) {
    assertStructFieldDataflow(
      unixReceipt,
      target,
      origin,
      "Unix file revision capture",
    );
  }
  for (const [target, origin] of [
    ["modified", /\bbasic\.LastWriteTime\b/],
    ["changed", /\bbasic\.ChangeTime\b/],
  ]) {
    assertStructFieldDataflow(
      windowsReceipt,
      target,
      origin,
      "Windows file revision capture",
    );
  }
  for (const accessor of ["modified_at_ns", "changed_at_ns"]) {
    const timestamp = uniqueMethodBlock(library, "FileRevision", accessor);
    assert.match(
      timestamp.slice(0, timestamp.indexOf("{")),
      /&self[\s\S]*?->\s*(?:io::Result\s*<\s*u64\s*>|u64)/,
      `${accessor} must expose one canonical nanosecond value`,
    );
    const nativeTimestampName = timestamp.match(
      /platform::([a-z_][a-z0-9_]*)\s*\(/,
    )?.[1];
    assert.ok(
      nativeTimestampName,
      `${accessor} must use the shared platform stamp conversion`,
    );
    for (const [platformName, source] of [
      ["Unix", unix],
      ["Windows", windows],
    ]) {
      const conversion = functionBlock(source, nativeTimestampName);
      const nativeFields =
        platformName === "Unix"
          ? accessor === "modified_at_ns"
            ? ["modified_seconds", "modified_nanos"]
            : ["changed_seconds", "changed_nanos"]
          : accessor === "modified_at_ns"
            ? ["modified"]
            : ["changed"];
      assertReturnedStampConversion(
        conversion,
        nativeFields,
        platformName === "Unix" ? "1_000_000_000" : "100",
        `${platformName} ${accessor}`,
        platformName === "Windows" ? "WINDOWS_TO_UNIX_EPOCH_TICKS" : undefined,
      );
    }
  }

  const rangedRead = uniqueMethodBlock(
    library,
    "FileCapability",
    "read_range_bounded",
  );
  const rangedHeader = rangedRead.slice(0, rangedRead.indexOf("{"));
  assert.match(rangedHeader, /expected:\s*&FileRevision\b/);
  assert.match(rangedHeader, /offset:\s*u64\b/);
  assert.match(rangedHeader, /length:\s*usize\b/);
  assert.match(rangedHeader, /->\s*io::Result\s*<\s*Vec\s*<\s*u8\s*>\s*>/);
  const rangeLimit = library.match(
    /const\s+([A-Z0-9_]*RANGE[A-Z0-9_]*):\s*usize\s*=\s*(?:4\s*\*\s*1024|4096)\s*;/,
  )?.[1];
  assert.ok(rangeLimit, "bounded positional reads need a fixed 4096-byte cap");
  const lengthGuard = conditionalBlocks(rangedRead).find(
    ({ condition, body }) =>
      new RegExp(`length\\s*>\\s*${escapeRegExp(rangeLimit)}`).test(
        condition,
      ) && /return\s+Err\s*\(/.test(body),
  );
  assert.ok(lengthGuard, "oversized range requests must return an error");
  const end = rangedRead.match(
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*offset\s*\.\s*checked_add\s*\([^;]{0,240}\blength[a-z0-9_]*\b[^;]{0,240}\)[^;]{0,300}\.ok_or(?:_else)?\s*\([^;]{0,300}\)\s*\?\s*;/,
  )?.[1];
  assert.ok(
    end,
    "bounded reads must derive one checked end from offset and length",
  );
  const endGuard = conditionalBlocks(rangedRead).find(
    ({ condition, body }) =>
      new RegExp(
        `\\b${escapeRegExp(end)}\\b\\s*>\\s*expected\\s*\\.\\s*size\\b`,
      ).test(condition) && /return\s+Err\s*\(/.test(body),
  );
  assert.ok(
    endGuard,
    "bounded reads must reject a checked end beyond the expected revision size",
  );

  const cursor = rangedRead.match(
    /let\s+mut\s+([a-z_][a-z0-9_]*)\s*=\s*offset\s*;/,
  )?.[1];
  assert.ok(cursor, "bounded reads need one cursor initialized from offset");
  const readLoop = bracedStatementBlocks(rangedRead, /\bwhile\b/).find(
    ({ header }) =>
      new RegExp(
        `\\b${escapeRegExp(cursor)}\\b\\s*<\\s*\\b${escapeRegExp(end)}\\b`,
      ).test(header),
  );
  assert.ok(readLoop, "the checked end must bound the positional read loop");
  const readCall = callBlocks(readLoop.source, /platform::read_at\s*\(/).find(
    ({ source }) =>
      new RegExp(`\\b${escapeRegExp(cursor)}\\b\\s*,?\\s*\\)$`).test(source),
  );
  assert.ok(readCall, "bounded reads must use the shared positional primitive");
  assert.match(
    readLoop.source.slice(
      readCall.index + readCall.source.length,
      readLoop.source.indexOf(";", readCall.index),
    ),
    /^\s*\?/,
    "native positional read failures must propagate",
  );
  const readCount = readLoop.source.match(
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*platform::read_at\s*\(/,
  )?.[1];
  assert.ok(readCount, "bounded reads must retain each physical read count");
  assert.match(
    readLoop.source,
    new RegExp(
      `${escapeRegExp(cursor)}\\s*=\\s*${escapeRegExp(cursor)}\\s*\\.\\s*checked_add\\s*\\([^;]{0,120}\\b${escapeRegExp(readCount)}\\b[^;]{0,120}\\)[^;]{0,300}\\.ok_or(?:_else)?\\s*\\([^;]{0,300}\\)\\s*\\?`,
    ),
    "only the exact physical read count may advance the cursor",
  );
  const zeroRead = conditionalBlocks(readLoop.source).find(
    ({ condition, body }) =>
      new RegExp(`\\b${escapeRegExp(readCount)}\\b\\s*==\\s*0\\b`).test(
        condition,
      ) && /return\s+Err\s*\([\s\S]{0,240}UnexpectedEof/.test(body),
  );
  assert.ok(zeroRead, "a zero physical read before end must be UnexpectedEof");

  const returnedMatch = rangedRead.match(
    /(?:return\s+)?Ok\s*\(\s*([a-z_][a-z0-9_]*)\s*\)\s*;?\s*\}$/,
  );
  const returned = returnedMatch?.[1];
  assert.ok(returned, "bounded reads must return one explicit byte buffer");
  const exactResult = conditionalBlocks(rangedRead).find(
    ({ condition, body, source }) =>
      rangedRead.indexOf(source) > rangedRead.indexOf(readLoop.source) &&
      new RegExp(
        `\\b${escapeRegExp(cursor)}\\b\\s*!=\\s*\\b${escapeRegExp(end)}\\b`,
      ).test(condition) &&
      new RegExp(
        `\\b${escapeRegExp(returned)}\\s*\\.\\s*len\\s*\\(\\s*\\)\\s*!=\\s*length\\b`,
      ).test(condition) &&
      /return\s+Err\s*\([\s\S]{0,240}UnexpectedEof/.test(body),
  );
  assert.ok(
    exactResult,
    "bounded reads must prove the exact cursor and requested result length before return",
  );
  const validations = callBlocks(
    rangedRead,
    /validate_revision_in\s*\(/,
  ).filter(
    ({ index, source }) =>
      /^\s*\?/.test(
        rangedRead.slice(index + source.length, rangedRead.indexOf(";", index)),
      ) &&
      callArguments(source).length === 2 &&
      /^&operation$/.test(callArguments(source)[0]) &&
      /^expected$/.test(callArguments(source)[1]),
  );
  assert.equal(
    rangedRead.match(/\.enter\s*\(/g)?.length ?? 0,
    1,
    "bounded reads must retain one CapabilityOperation",
  );
  assert.doesNotMatch(
    rangedRead,
    /(?<!_)\.\s*validate_revision\s*\(/,
    "bounded reads cannot re-enter through the public revision validator",
  );
  const bindingValidations = callBlocks(
    rangedRead,
    /self\s*\.\s*validate\s*\(/,
  ).filter(
    ({ source }) =>
      callArguments(source).length === 1 &&
      callArguments(source)[0] === "&operation",
  );
  assert.ok(
    bindingValidations.length >= 2,
    "bounded reads must bracket receipt observations with binding validation",
  );
  const loopIndex = rangedRead.indexOf(readLoop.source);
  const exactIndex = rangedRead.indexOf(exactResult.source);
  const returnIndex = returnedMatch.index;
  const allocation = /Vec::with_capacity\s*\(|Vec::new\s*\(|vec!\s*\[/.exec(
    rangedRead,
  );
  assert.ok(
    allocation,
    "bounded reads must allocate one bounded result buffer",
  );
  const firstWork = Math.min(allocation.index, loopIndex + readCall.index);
  assert.ok(
    rangedRead.indexOf(lengthGuard.source) < firstWork &&
      rangedRead.indexOf(endGuard.source) < firstWork,
    "length and revision-size guards must precede allocation and I/O",
  );
  assert.ok(
    validations.some(({ index }) => index < firstWork) &&
      validations.some(
        ({ index }) => index > exactIndex && index < returnIndex,
      ),
    "bounded reads must validate before I/O and after exact completion",
  );
  assert.ok(
    bindingValidations[0].index < validations[0].index &&
      bindingValidations.at(-1).index > validations.at(-1).index,
    "bounded reads must validate binding before and after revision observations",
  );
});

test("P01-B02 keeps directory revisions opaque and identity process-local", async () => {
  const [library, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const declaration = new RegExp(
    `((?:#\\[[^\\]]*\\]\\s*)*)pub\\s+struct\\s+DirectoryRevision\\b`,
  ).exec(library);
  assert.ok(declaration, "missing opaque DirectoryRevision");
  assert.match(declaration[1], /#\[derive\([^\]]*\bEq\b[^\]]*\)\]/);
  assert.doesNotMatch(
    declaration[1],
    /#\[derive\([^\]]*\b(?:Hash|Serialize|Deserialize)\b[^\]]*\)\]/,
  );
  assert.doesNotMatch(library, /impl\s+Hash\s+for\s+DirectoryRevision\b/);
  assert.doesNotMatch(
    library,
    /impl\s+(?:Serialize|Deserialize)\s+for\s+DirectoryRevision\b/,
  );
  const revisionState = itemBlock(library, "struct", "DirectoryRevision");
  assert.doesNotMatch(
    revisionState,
    /\bpub(?:\([^)]*\))?\s+[a-z_][a-z0-9_]*\s*:/,
    "directory revision fields must remain opaque",
  );
  assert.match(revisionState, /DirectoryIdentity/);
  assert.match(
    revisionState,
    /platform::[A-Za-z0-9_]*(?:DirectoryRevision|DirectoryStamp)/,
  );

  const revision = uniqueMethodBlock(library, "Directory", "revision");
  assert.match(
    revision.slice(0, revision.indexOf("{")),
    /&self[\s\S]*?->\s*io::Result\s*<\s*DirectoryRevision\s*>/,
  );
  assertCountAtLeast(
    revision,
    /self\.validate\s*\(/,
    2,
    "directory revision capture must validate its binding before and after",
  );
  const nativeRevisionName = revision.match(
    /platform::([a-z_][a-z0-9_]*(?:revision|receipt|stamp)[a-z0-9_]*)\s*\(/,
  )?.[1];
  assert.ok(
    nativeRevisionName,
    "directory revision must use one native stamp primitive",
  );
  const validateRevision = uniqueMethodBlock(
    library,
    "Directory",
    "validate_revision",
  );
  assert.match(
    validateRevision.slice(0, validateRevision.indexOf("{")),
    /&self[\s\S]*?expected:\s*&DirectoryRevision\b[\s\S]*?->\s*io::Result\s*<\s*\(\s*\)\s*>/,
  );
  const validateRevisionIn = uniqueMethodBlock(
    library,
    "Directory",
    "validate_revision_in",
  );
  assert.match(
    validateRevisionIn.slice(0, validateRevisionIn.indexOf("{")),
    /fn\s+validate_revision_in\s*\(\s*&self\s*,\s*operation:\s*&CapabilityOperation\s*,\s*expected:\s*&DirectoryRevision\s*,?\s*\)\s*->\s*io::Result\s*<\s*\(\s*\)\s*>/,
  );
  assert.doesNotMatch(
    validateRevisionIn.slice(0, validateRevisionIn.indexOf("{")),
    /\bpub\b/,
  );
  assert.match(validateRevisionIn, /DirectoryIdentity|identity/);
  assert.match(
    validateRevisionIn,
    new RegExp(`platform::${escapeRegExp(nativeRevisionName)}\\s*\\(`),
  );
  assert.match(validateRevisionIn, /operation\.authority/);
  assert.doesNotMatch(validateRevisionIn, /\.enter\s*\(|\.authority\s*\(\s*\)/);
  const publicValidation = callBlocks(
    validateRevision,
    /self\s*\.\s*validate_revision_in\s*\(/,
  )[0];
  assert.ok(publicValidation);
  assert.deepEqual(callArguments(publicValidation.source), [
    "&operation",
    "expected",
  ]);
  assert.equal(validateRevision.match(/\.enter\s*\(/g)?.length ?? 0, 1);
  assertCountAtLeast(
    validateRevision,
    /self\.validate\s*\(/,
    2,
    "directory revision validation must prove binding before and after its stamp check",
  );
  const unix = between(
    platform,
    "#[cfg(unix)]\nmod native {",
    "#[cfg(windows)]\nmod native {",
  );
  const windows = platform.slice(
    platform.indexOf("#[cfg(windows)]\nmod native {"),
  );
  const unixRevision = functionBlock(unix, nativeRevisionName);
  assert.match(unixRevision, /fstat\s*\(/);
  for (const [target, origin] of [
    ["modified_seconds", /\bstat\.st_mtime\b/],
    ["modified_nanos", /\bstat\.st_mtime_nsec\b/],
    ["changed_seconds", /\bstat\.st_ctime\b/],
    ["changed_nanos", /\bstat\.st_ctime_nsec\b/],
  ]) {
    assertStructFieldDataflow(
      unixRevision,
      target,
      origin,
      "Unix directory revision capture",
    );
  }
  const windowsRevision = functionBlock(windows, nativeRevisionName);
  assert.match(
    windowsRevision,
    /query_basic\s*\(|FILE_BASIC_INFO|FileBasicInfo/,
  );
  for (const [target, origin] of [
    ["modified", /\bbasic\.LastWriteTime\b/],
    ["changed", /\bbasic\.ChangeTime\b/],
  ]) {
    assertStructFieldDataflow(
      windowsRevision,
      target,
      origin,
      "Windows directory revision capture",
    );
  }
  assert.doesNotMatch(
    library,
    /pub\s+(?:struct|enum)\s+[A-Za-z0-9_]*Fingerprint\b|pub\s+fn\s+[a-z_]*(?:restart|fingerprint|physical_identity|identity[a-z_]*(?:bytes|digest))[a-z_]*\s*\([^)]*\)\s*->\s*(?:\[\s*u8|Vec\s*<\s*u8)/,
    "axial-fs cannot export restart-stable physical identity bytes",
  );
  assert.doesNotMatch(
    `${library}\n${platform}`,
    /\b(?:Serialize|Deserialize)\b|serde::|serde_json::/,
    "process-local revision and park authority cannot be serialized",
  );
  const manifest = await read("core/fs/Cargo.toml");
  assert.doesNotMatch(manifest, /^serde(?:_json)?\s*=/m);
});

test("P01-B02 injects one fixed persisted-state directory bundle off runtime", async () => {
  const [
    configRoot,
    state,
    anchoredRecord,
    benchmarkDrivers,
    performanceOperations,
    journals,
    failureMemory,
    persistedLoad,
    knownGood,
    benchmarkSuites,
    launchReports,
    accounts,
    instanceRegistry,
    configStore,
    userModWitness,
    performanceRules,
    rejectionStreaks,
  ] = await Promise.all([
    read("core/config/src/root.rs"),
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/execution/anchored_record.rs"),
    read("apps/api/src/state/benchmark_suite_drivers.rs"),
    read("apps/api/src/state/performance_operations.rs"),
    read("apps/api/src/state/journals.rs"),
    read("apps/api/src/state/failure_memory.rs"),
    read("apps/api/src/state/persisted_state_load.rs"),
    read("apps/api/src/state/known_good.rs"),
    read("apps/api/src/state/benchmark_suites.rs"),
    read("apps/api/src/state/launch_reports.rs"),
    read("apps/api/src/state/accounts.rs"),
    read("apps/api/src/state/instance_registry.rs"),
    read("apps/api/src/state/config.rs"),
    read("apps/api/src/state/user_mod_witness.rs"),
    read("apps/api/src/state/performance_rules.rs"),
    read("apps/api/src/state/persisted_state_rejection_streaks.rs"),
  ]);

  const directories = itemBlock(
    configRoot,
    "struct",
    "PersistedStateDirectories",
  );
  for (const field of [
    "application_root",
    "operation_journal_parent",
    "known_good",
    "guardian_failure_memory_parent",
    "performance_parent",
    "performance_operations",
    "benchmark_suites",
    "benchmark_suite_drivers",
    "launch_reports",
  ]) {
    assert.match(
      directories,
      new RegExp(`\\b${escapeRegExp(field)}\\s*:\\s*Directory\\b`),
      `persisted-state directory bundle is missing ${field}`,
    );
  }
  assert.equal(
    directories.match(/:\s*Directory\b/g)?.length ?? 0,
    9,
    "persisted-state directory bundle must expose exactly its nine R1 capabilities",
  );
  assert.doesNotMatch(directories, /\b(?:Path|PathBuf|OsString|String)\b/);

  const prepare = uniqueMethodBlock(
    configRoot,
    "AppRootSession",
    "prepare_persisted_state_directories",
  );
  const prepareHeader = prepare.slice(0, prepare.indexOf("{"));
  assert.match(
    prepareHeader,
    /pub fn prepare_persisted_state_directories\s*\(\s*&self\s*\)\s*->\s*io::Result\s*<\s*PersistedStateDirectories\s*>/,
  );
  const prepareFlow = uniqueReachableFunctions(configRoot, prepare);
  for (const fixedPath of [
    /root_directory\s*\(\s*\)/,
    /\[\s*"state"\s*\]/,
    /\[\s*"state"\s*,\s*"known-good"\s*\]/,
    /\[\s*"guardian"\s*\]/,
    /\[\s*"performance"\s*\]/,
    /\[\s*"performance"\s*,\s*"operations"\s*\]/,
    /\[\s*"benchmarks"\s*,\s*"suites"\s*\]/,
    /\[\s*"benchmarks"\s*,\s*"suite-drivers"\s*\]/,
    /\[\s*"benchmarks"\s*,\s*"launch"\s*\]/,
  ]) {
    assert.match(prepareFlow, fixedPath);
  }
  const appRoot = implementationBlock(configRoot, "AppRootSession");
  const publicPreparers = functionBlocks(appRoot).filter(({ source }) => {
    const header = source.slice(0, source.indexOf("{"));
    return /^pub fn prepare/.test(header) && /Directory/.test(header);
  });
  assert.ok(publicPreparers.length > 0, "missing fixed root preparers");
  for (const { name, source } of publicPreparers) {
    const header = source.slice(0, source.indexOf("{"));
    assert.doesNotMatch(
      header,
      /\b(?:Path|PathBuf|LeafName|Iterator|IntoIterator)\b|&\s*\[/,
      `${name} must not expose an arbitrary managed directory chain`,
    );
  }

  const stateStartup = functionBlock(state, "new_with_telemetry_inner");
  assert.equal(
    stateStartup.match(/prepare_persisted_state_directories\s*\(/g)?.length ??
      0,
    1,
    "AppState startup must prepare the fixed persisted-state bundle once",
  );
  for (const getter of [
    "application_root",
    "operation_journal_parent",
    "known_good",
    "guardian_failure_memory_parent",
    "performance_parent",
    "performance_operations",
    "benchmark_suites",
    "benchmark_suite_drivers",
    "launch_reports",
  ]) {
    assert.match(
      stateStartup,
      new RegExp(`\\.${escapeRegExp(getter)}\\s*\\(\\s*\\)`),
      `AppState startup does not inject ${getter}`,
    );
  }

  const adapterConstructor = uniqueMethodBlock(
    anchoredRecord,
    "AnchoredRecordDirectory",
    "from_directory",
  );
  assert.match(
    adapterConstructor.slice(0, adapterConstructor.indexOf("{")),
    /directory:\s*Directory\b/,
  );
  const productionSources = [
    ["anchored adapter", anchoredRecord],
    ["benchmark driver loader", benchmarkDrivers],
    ["performance operation loader", performanceOperations],
    ["journal loader", journals],
    ["failure-memory loader", failureMemory],
    ["persisted-state recovery", persistedLoad],
    ["known-good store", knownGood],
    ["benchmark suite store", benchmarkSuites],
    ["launch report store", launchReports],
    ["account store", accounts],
    ["instance registry", instanceRegistry],
    ["config store", configStore],
    ["user-mod witness store", userModWitness],
    ["performance rules store", performanceRules],
    ["rejection streak store", rejectionStreaks],
  ].map(([label, source]) => [
    label,
    source.split(/\n#\[cfg\(test\)\]\s*\nmod tests\s*\{/)[0],
  ]);
  for (const [label, source] of productionSources) {
    assert.doesNotMatch(
      source,
      /AnchoredRecordDirectory::open\s*\(|\.admit_absolute_directory\s*\(/,
      `${label} must not reopen an absolute persisted-state path`,
    );
  }

  const load = functionBlock(state, "load");
  const blockingStartup = callBlocks(
    load,
    /tokio::task::spawn_blocking\s*\(/,
  ).find(({ source }) => /new_with_telemetry_inner\s*\(/.test(source));
  assert.ok(
    blockingStartup,
    "persisted-state bundle preparation and initial reads must stay off Tokio workers",
  );
});

test("P01-B02 derives complete bounded v3 restart observations from axial-fs", async () => {
  const [
    anchoredRecord,
    benchmarkDrivers,
    performanceOperations,
    userOwnedState,
  ] = await Promise.all([
    read("apps/api/src/execution/anchored_record.rs"),
    read("apps/api/src/state/benchmark_suite_drivers.rs"),
    read("apps/api/src/state/performance_operations.rs"),
    read("apps/api/src/execution/user_owned_state.rs"),
  ]);
  const productionAdapter = anchoredRecord.split(
    /\n#\[cfg\(test\)\]\s*\nmod tests\s*\{/,
  )[0];

  assert.match(
    productionAdapter,
    /struct AnchoredRecordDirectory(?:\s*\([^;]*\bDirectory\b[^;]*\)|\s*\{[^}]*\bDirectory\b)/s,
    "anchored record directories must retain axial-fs Directory",
  );
  const identity = itemBlock(
    productionAdapter,
    "struct",
    "AnchoredRecordIdentity",
  );
  assert.match(identity, /\bAnchoredRecordDirectory\b/);
  assert.match(identity, /\bLeafName\b/);
  assert.match(identity, /\bFileRevision\b/);
  assert.match(identity, /\bFileCapability\b/);

  const names = uniqueMethodBlock(
    productionAdapter,
    "AnchoredRecordDirectory",
    "names_bounded",
  );
  const anchoredDirectoryMethods = implementationBlocks(
    productionAdapter,
    "AnchoredRecordDirectory",
  ).flatMap((implementation) => functionBlocks(implementation));
  assert.ok(
    !anchoredDirectoryMethods.some(({ name }) => name === "names"),
    "the unbounded names compatibility method must be deleted",
  );
  const entries = callBlocks(names, /\.entries\s*\(/)[0];
  assert.ok(entries, "bounded names must use axial-fs enumeration");
  const entriesLimit = callArguments(entries.source)[0] ?? "";
  const entriesLimitLocal = /^([a-z_][a-z0-9_]*)$/.exec(entriesLimit)?.[1];
  assert.ok(
    /max_entries/.test(entriesLimit) ||
      (entriesLimitLocal &&
        new RegExp(
          `\\blet\\s+${escapeRegExp(entriesLimitLocal)}\\s*=\\s*[^;]*\\bmax_entries\\b`,
        ).test(names)),
    "enumeration limit must derive from the caller's finite bound",
  );
  assert.match(names, /DirectoryListingState::(?:Complete|Truncated)/);
  assert.match(names, /Ok\s*\(\s*None\s*\)/);
  assert.match(names, /Ok\s*\(\s*Some\s*\(/);
  assert.doesNotMatch(names, /read_dir\s*\(/);
  const adapterListingBound =
    /\.clamp\s*\([^,]+,\s*(MAX_[A-Z0-9_]*ENTR(?:Y|IES))\s*\)/.exec(names)?.[1];
  assert.ok(
    adapterListingBound,
    "bounded enumeration must clamp to a named finite maximum",
  );
  assertLiteralListingBound(
    productionAdapter,
    adapterListingBound,
    `anchored directory ${adapterListingBound}`,
  );

  for (const [label, source, loaderName] of [
    ["benchmark driver", benchmarkDrivers, "load_persisted_driver_inner"],
    [
      "performance operation",
      performanceOperations,
      "load_persisted_operation_inner",
    ],
  ]) {
    const loader = uniqueReachableFunctions(
      source,
      functionBlock(source, loaderName),
    );
    const boundedListing = callBlocks(loader, /\.names_bounded\s*\(/)[0];
    assert.ok(boundedListing, `${label} startup must use a bounded listing`);
    const listingBound = /MAX_[A-Z0-9_]*ENTR(?:Y|IES)/.exec(
      callArguments(boundedListing.source)[0] ?? "",
    )?.[0];
    assert.ok(
      listingBound,
      `${label} listing needs a named finite entry bound`,
    );
    assertLiteralListingBound(source, listingBound, `${label} ${listingBound}`);
    assert.doesNotMatch(loader, /\.names\s*\(\s*\)/);
    const afterListing = loader.slice(
      boundedListing.index,
      boundedListing.index + 2_400,
    );
    assert.match(
      afterListing,
      /\bNone\b[\s\S]*rejected_record_scan_authoritative\s*=\s*false/,
      `${label} truncation must make the rejection scan non-authoritative`,
    );
  }
  const userOwnedObservation = uniqueReachableFunctions(
    userOwnedState,
    functionBlock(userOwnedState, "observe_blocking"),
  );
  const userOwnedListings = callBlocks(
    userOwnedObservation,
    /\.names_bounded\s*\(/,
  );
  assert.ok(
    userOwnedListings.length > 0,
    "user-owned observation must list files",
  );
  assert.doesNotMatch(
    userOwnedObservation,
    /\.names\s*\(\s*\)|\.entries\s*\(/,
    "user-owned observation must not bypass bounded anchored listing",
  );
  for (const listing of userOwnedListings) {
    const bound = /MAX_[A-Z0-9_]*ENTR(?:Y|IES)/.exec(
      callArguments(listing.source)[0] ?? "",
    )?.[0];
    assert.ok(bound, "user-owned listing needs a named finite entry bound");
    assertLiteralListingBound(userOwnedState, bound, `user-owned ${bound}`);
  }

  const observation = uniqueReachableFunctions(
    productionAdapter,
    uniqueMethodBlock(productionAdapter, "AnchoredRecordDirectory", "read"),
  );
  assertOrdered(observation, "open_file", "revision", "file before revision");
  assertOrdered(
    observation,
    "revision",
    "read_bounded",
    "revision before bounded read",
  );
  assertOrdered(
    observation,
    "read_bounded",
    "validate_revision",
    "bounded read before final revision validation",
  );
  const digest = uniqueMethodBlock(
    productionAdapter,
    "AnchoredRecordDirectory",
    "digest",
  );
  const digestReader = callBlocks(digest, /\.reader\s*\(/)[0];
  assert.ok(digestReader, "digest must stream through axial-fs FileReader");
  assert.match(
    callArguments(digestReader.source)[0] ?? "",
    /\bmax_bytes\b/,
    "digest reader must enforce the caller's byte bound",
  );
  assert.match(
    digest,
    /let\s+mut\s+[a-z_][a-z0-9_]*\s*=\s*\[\s*0(?:_u8)?\s*;\s*[0-9][0-9_]*(?:\s*\*\s*[0-9][0-9_]*)*\s*\]\s*;/,
    "digest must use a fixed-size stack buffer",
  );
  const digestLoop = bracedStatementBlocks(digest, /\bloop\b/)[0]?.source ?? "";
  assert.match(digestLoop, /\.read\s*\(\s*&mut\s+[a-z_][a-z0-9_]*\s*\)/);
  assert.match(digestLoop, /if\s+[a-z_][a-z0-9_]*\s*==\s*0\s*\{\s*break\s*;/);
  const sha256Hasher =
    /let\s+mut\s+([a-z_][a-z0-9_]*)\s*=\s*Sha256::new\s*\(\s*\)/.exec(
      digest,
    )?.[1];
  const sha512Hasher =
    /let\s+mut\s+([a-z_][a-z0-9_]*)\s*=\s*Sha512::new\s*\(\s*\)/.exec(
      digest,
    )?.[1];
  assert.ok(
    sha256Hasher && sha512Hasher,
    "digest needs both streaming hashers",
  );
  for (const [label, hasher] of [
    ["SHA-256", sha256Hasher],
    ["SHA-512", sha512Hasher],
  ]) {
    assert.match(
      digestLoop,
      new RegExp(
        `\\b${escapeRegExp(hasher)}\\.update\\s*\\(\\s*&[a-z_][a-z0-9_]*\\s*\\[\\s*\\.\\.[a-z_][a-z0-9_]*\\s*\\]\\s*\\)`,
      ),
      `digest loop must update ${label} from every bounded chunk`,
    );
  }
  assertOrdered(
    digest,
    ".finish(",
    "validate_revision",
    "reader completion before final revision validation",
  );
  assert.doesNotMatch(
    digest,
    /\.read_bounded\s*\(|\.read_to_end\s*\(|\bVec(?:<|::)|\bvec!\s*\[/,
    "digest cannot allocate a whole-file byte buffer",
  );
  const revalidate = uniqueMethodBlock(
    productionAdapter,
    "AnchoredRecordIdentity",
    "revalidate",
  );
  const revalidateFlow = uniqueReachableFunctions(
    productionAdapter,
    revalidate,
  );
  assert.doesNotMatch(
    revalidateFlow,
    /open_file\s*\(/,
    "retained identity must validate its held file rather than reopen by name",
  );
  assert.match(revalidateFlow, /self\.file\.validate_revision\s*\(/);
  const quarantine = uniqueMethodBlock(
    productionAdapter,
    "AnchoredRecordIdentity",
    "quarantine",
  );
  const quarantineFlow = uniqueReachableFunctions(
    productionAdapter,
    quarantine,
  );
  assert.doesNotMatch(
    quarantineFlow,
    /open_file\s*\(/,
    "quarantine must retain the observed file rather than reopen by name",
  );
  assertOrdered(
    quarantineFlow,
    "ensure_fresh_portable_alias_absent",
    "validate_revision",
    "quarantine must recheck aliases before validating its held file",
  );
  assertOrdered(
    quarantineFlow,
    "validate_revision",
    "park_request",
    "quarantine must validate the held file before minting a park request",
  );
  assert.match(quarantineFlow, /ExpectedFileContent::new\s*\(/);

  assert.match(
    productionAdapter,
    /axial\.persisted-state-restart-record-identity\.v3/,
  );
  assert.doesNotMatch(
    productionAdapter,
    /axial\.persisted-state-restart-record-identity\.v2/,
  );
  const restartMethod = uniqueMethodBlock(
    productionAdapter,
    "AnchoredRecordObservation",
    "into_restart_identity",
  );
  const restartHeader = restartMethod.slice(0, restartMethod.indexOf("{"));
  assert.match(
    restartHeader,
    /context:\s*AnchoredRecordRestartContext\b/,
    "v3 identity must bind the canonical persisted-state store context",
  );
  assert.match(
    restartHeader,
    /canonical_original_(?:name|leaf):\s*&\s*LeafName\b/,
    "v3 identity must consume the canonical original LeafName",
  );
  const restartContext = itemBlock(
    productionAdapter,
    "enum",
    "AnchoredRecordRestartContext",
  );
  assert.match(restartContext, /\bPerformanceOperation\b/);
  assert.match(restartContext, /\bBenchmarkSuiteDriver\b/);
  const restart = uniqueReachableFunctions(productionAdapter, restartMethod);
  assert.match(restart, /\bbytes\b/);
  assert.match(restart, /\b(?:Sha256|sha256)\b/);
  const contextBinding =
    /let\s+([a-z_][a-z0-9_]*)(?:\s*:[^=;]+)?\s*=\s*match\s+context\s*\{/.exec(
      restartMethod,
    )?.[1];
  assert.ok(contextBinding, "v3 identity must bind the matched store context");
  assert.match(
    restartMethod,
    new RegExp(
      `hasher\\.update\\s*\\(\\s*${escapeRegExp(contextBinding)}\\s*\\)`,
    ),
    "v3 identity must hash the matched store context",
  );
  const performanceDomain = /PerformanceOperation\s*=>\s*b"([^"]+)"/.exec(
    restartMethod,
  )?.[1];
  const benchmarkDomain = /BenchmarkSuiteDriver\s*=>\s*b"([^"]+)"/.exec(
    restartMethod,
  )?.[1];
  assert.ok(
    performanceDomain &&
      benchmarkDomain &&
      performanceDomain !== benchmarkDomain,
    "each persisted-state store needs distinct hashed context bytes",
  );
  assert.match(
    restartMethod,
    /update_native_name\s*\([^;]*canonical_original_(?:name|leaf)[^;]*\)/,
    "v3 identity must hash the caller-supplied original leaf",
  );
  const restartSize =
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*identity\.revision\.size\s*\(\s*\)\s*;/.exec(
      restartMethod,
    )?.[1];
  const restartMtime =
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*identity\.revision\.modified_at_ns\s*\(\s*\)\s*\?\s*;/.exec(
      restartMethod,
    )?.[1];
  assert.ok(restartSize && restartMtime, "v3 must bind native size and mtime");
  for (const [label, value] of [
    ["size", restartSize],
    ["mtime", restartMtime],
  ]) {
    assert.match(
      restartMethod,
      new RegExp(
        `hasher\\.update\\s*\\(\\s*${escapeRegExp(value)}\\.to_(?:le|be)_bytes\\s*\\(\\s*\\)\\s*\\)`,
      ),
      `v3 identity must hash native ${label}`,
    );
  }
  const nativeNameEncoders = functionBlocks(productionAdapter).filter(
    ({ name }) => name === "update_native_name",
  );
  const unixNameEncoder = nativeNameEncoders.find(({ source }) =>
    /\.as_bytes\s*\(\s*\)/.test(source),
  )?.source;
  const windowsNameEncoder = nativeNameEncoders.find(({ source }) =>
    /\.encode_wide\s*\(\s*\)/.test(source),
  )?.source;
  assert.ok(
    unixNameEncoder && windowsNameEncoder,
    "missing native leaf encoders",
  );
  for (const [label, encoder] of [
    ["Unix", unixNameEncoder],
    ["Windows", windowsNameEncoder],
  ]) {
    assert.match(
      encoder,
      /hasher\.update\s*\([^;]*\.len\s*\(\s*\)[^;]*\.to_(?:le|be)_bytes\s*\(\s*\)/,
      `${label} leaf encoding must hash its native length`,
    );
    assertCountAtLeast(
      encoder,
      /hasher\.update\s*\(/,
      2,
      `${label} leaf encoding must hash native contents after length`,
    );
  }
  const unixNameBytes =
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*name\.as_bytes\s*\(\s*\)\s*;/.exec(
      unixNameEncoder,
    )?.[1];
  assert.ok(unixNameBytes, "Unix leaf encoder must bind raw native bytes");
  assert.match(
    unixNameEncoder,
    new RegExp(
      `hasher\\.update\\s*\\(\\s*${escapeRegExp(unixNameBytes)}\\s*\\)`,
    ),
    "Unix leaf encoder must hash the bound native bytes",
  );
  const windowsNameUnits =
    /let\s+([a-z_][a-z0-9_]*)\s*=\s*name\.encode_wide\s*\(\s*\)[^;]*;/.exec(
      windowsNameEncoder,
    )?.[1];
  assert.ok(windowsNameUnits, "Windows leaf encoder must bind native units");
  const windowsUnitLoop = bracedStatementBlocks(
    windowsNameEncoder,
    new RegExp(
      `for\\s+[a-z_][a-z0-9_]*\\s+in\\s+${escapeRegExp(windowsNameUnits)}\\b`,
    ),
  )[0];
  assert.ok(
    windowsUnitLoop,
    "Windows leaf encoder must consume every native unit",
  );
  const windowsUnit = /for\s+([a-z_][a-z0-9_]*)\s+in\b/.exec(
    windowsUnitLoop.header,
  )?.[1];
  assert.ok(windowsUnit);
  assert.match(
    windowsUnitLoop.body,
    new RegExp(
      `hasher\\.update\\s*\\(\\s*${escapeRegExp(windowsUnit)}\\.to_(?:le|be)_bytes\\s*\\(\\s*\\)\\s*\\)`,
    ),
    "Windows leaf encoder must hash each bound native unit",
  );
  assertOrdered(
    restartMethod,
    "canonical_original_",
    ".size(",
    "canonical leaf before native size",
  );
  assertOrdered(
    restartMethod,
    ".size(",
    "modified_at_ns",
    "native size before modification time",
  );
  assertOrdered(
    restartMethod,
    "modified_at_ns",
    "hasher.update(bytes)",
    "modification time before complete bytes",
  );
  assert.match(
    restart,
    /Oversized[\s\S]{0,320}(?:return\s+Err|=>\s*Err)/,
    "oversized observations must not mint restart mutation identity",
  );
  assert.doesNotMatch(
    restart,
    /read_range_bounded|\b(?:sample|head|tail|edge)\b|platform::|\b(?:physical|inode|device|volume|file_id|directory_chain|changed_at|created_at|ctime|destination|quarantine)\b/i,
    "v3 identity must exclude samples, physical IDs, destination names, and ctime",
  );

  for (const [source, functionName] of [
    [benchmarkDrivers, "retain_driver_rejected_records"],
    [performanceOperations, "retain_performance_rejected_records"],
  ]) {
    const retention = functionBlock(source, functionName);
    assert.match(
      retention,
      /rejection\s*==\s*PersistedStateRecordRejection::Oversized[\s\S]{0,240}\bcontinue\b/,
      `${functionName} must keep oversized records out of repair eligibility`,
    );
  }
});

test("P01-B02 resumes deterministic persisted-state parks as typed receipts off runtime", async () => {
  const [persistedLoad, persistedRepair, anchoredRecord] = await Promise.all([
    read("apps/api/src/state/persisted_state_load.rs"),
    read("apps/api/src/state/persisted_state_repair.rs"),
    read("apps/api/src/execution/anchored_record.rs"),
  ]);
  assert.doesNotMatch(
    persistedLoad,
    /fn exact_applied_quarantine_is_present\s*\(/,
    "restart recovery must return retained park authority instead of a bool",
  );
  const admission = functionBlock(
    persistedLoad,
    "admit_exact_applied_persisted_state_quarantine",
  );
  const admissionHeader = admission.slice(0, admission.indexOf("{"));
  assert.match(
    admissionHeader,
    /directory:\s*&?\s*(?:AnchoredRecord)?Directory\b/,
  );
  assert.match(
    admissionHeader,
    /io::Result\s*<\s*Option\s*<\s*PersistedStateRejectedRecordQuarantineReceipt\s*>\s*>/,
  );
  assert.doesNotMatch(admissionHeader, /\b(?:AppPaths|Path|PathBuf)\b/);
  const admissionFlow = uniqueReachableFunctions(
    `${persistedLoad}\n${anchoredRecord}`,
    admission,
  );
  for (const required of [
    /persisted_state_repair_quarantine_suffix\s*\(/,
    /anchored_record_quarantine_name\s*\(/,
    /ExpectedFileContent::new\s*\(/,
    /\.park_request\s*\(/,
    /\.admit_existing_file_park\s*\(/,
  ]) {
    assert.match(admissionFlow, required);
  }
  assert.doesNotMatch(
    admissionFlow,
    /AnchoredRecordDirectory::open\s*\(|\b(?:Uuid|OsRng|random_leaf)\b/,
  );
  const restartObservation = callBlocks(
    admissionFlow,
    /\.into_restart_identity\s*\(/,
  )[0];
  const existingPark = callBlocks(
    admissionFlow,
    /\.admit_existing_file_park\s*\(/,
  )[0];
  const digestComparison = conditionalBlocks(admissionFlow).find(
    ({ condition }) =>
      /physical_identity\s*\(/.test(condition) &&
      /digest|restart_identity/i.test(condition),
  );
  assert.ok(
    restartObservation && existingPark && digestComparison,
    "restart admission must compare freshly recomputed v3 identity with its durable attempt",
  );
  const restartArguments = callArguments(restartObservation.source);
  assert.equal(
    restartArguments.length,
    2,
    "restart identity must consume only store context and original leaf",
  );
  assert.match(
    restartArguments[0],
    /restart_context\s*\([^)]*attempt\.store\s*\(\s*\)/,
    "restart identity must bind the durable attempt's store context",
  );
  assert.match(
    restartArguments[1],
    /(?:source|original)[a-z0-9_]*(?:leaf|name)|(?:leaf|name)[a-z0-9_]*(?:source|original)/,
    "restart identity must receive the canonical original leaf",
  );
  assert.doesNotMatch(
    restartArguments[1],
    /destination|quarantine/i,
    "restart identity cannot bind the derived destination name",
  );
  const digestComparisonIndex = admissionFlow.indexOf(digestComparison.source);
  assert.ok(
    restartObservation.index < digestComparisonIndex &&
      digestComparisonIndex < existingPark.index,
    "durable restart identity must match before existing-park authority is admitted",
  );
  assert.match(
    digestComparison.body,
    /return\s+(?:Ok\s*\(\s*None\s*\)|Err\s*\()/,
    "restart digest disagreement must refuse park admission",
  );

  const startup = functionBlock(
    persistedRepair,
    "reconcile_persisted_state_repair_startup",
  );
  const blockingAdmission = callBlocks(
    startup,
    /tokio::task::spawn_blocking\s*\(/,
  ).find(({ source }) =>
    /admit_exact_applied_persisted_state_quarantine\s*\(/.test(source),
  );
  assert.ok(
    blockingAdmission,
    "restart park admission must execute on the blocking pool",
  );
  assertOrdered(
    blockingAdmission.source,
    "admit_exact_applied_persisted_state_quarantine",
    "is_current",
    "restart admission before receipt validation",
  );
  assertOrdered(
    blockingAdmission.source,
    "is_current",
    "acknowledge_preserved",
    "restart receipt validation before preservation acknowledgement",
  );
  assert.ok(
    blockingAdmission.index <
      startup.indexOf("settle_persisted_state_repair_terminal"),
    "restart receipt must settle before its reconstructed terminal",
  );
  const restartEffect = new RegExp(
    `let\\s*\\(\\s*([a-z_][a-z0-9_]*)\\s*,\\s*([a-z_][a-z0-9_]*)\\s*\\)\\s*:\\s*\\(\\s*PersistedStateRepairTerminalOutcome\\s*,\\s*Option\\s*<\\s*AnchoredRecordQuarantinePreservationError\\s*>\\s*\\)\\s*=\\s*[\\s\\S]{0,320}tokio::task::spawn_blocking`,
  ).exec(startup);
  assert.ok(
    restartEffect,
    "restart blocking admission must return outcome and typed preservation authority",
  );
  const [, restartOutcome, restartPreservation] = restartEffect;
  const restartAcknowledgement = blockingAdmission.source.slice(
    blockingAdmission.source.indexOf("acknowledge_preserved"),
  );
  const restartAckFailure = matchArmBlocks(
    restartAcknowledgement,
    /Err\s*\([^)]*\)/,
  )[0];
  assert.ok(
    restartAckFailure,
    "restart preservation acknowledgement needs an explicit failure outcome",
  );
  assert.match(
    restartAckFailure.body,
    /PersistedStateRepairTerminalOutcome::AppliedUnverified/,
    "restart acknowledgement failure must reconstruct AppliedUnverified",
  );
  assert.match(
    restartAckFailure.body,
    /Some\s*\(\s*[a-z_][a-z0-9_]*\s*\)/,
    "restart acknowledgement failure must return its actual typed carrier",
  );
  const restartAckError = /Err\s*\(\s*([a-z_][a-z0-9_]*)\s*\)/.exec(
    restartAckFailure.marker,
  )?.[1];
  assert.ok(
    restartAckError,
    "restart acknowledgement failure must bind its error",
  );
  assert.match(
    restartAckFailure.body,
    new RegExp(`Some\\s*\\(\\s*${escapeRegExp(restartAckError)}\\s*\\)`),
    "restart acknowledgement must retain the exact error returned by axial-fs",
  );
  assert.doesNotMatch(
    restartAckFailure.body,
    /PersistedStateRepairTerminalOutcome::Quarantined/,
    "restart acknowledgement failure cannot claim an exact quarantine",
  );
  const restartTerminalIndex = startup.indexOf(
    "settle_persisted_state_repair_terminal",
  );
  const restartPreservationReturn = conditionalBlocks(startup).find(
    ({ condition, body }) =>
      new RegExp(
        `let\\s+Some\\s*\\([^)]*\\)\\s*=\\s*${escapeRegExp(restartPreservation)}\\b`,
      ).test(condition) &&
      /return\s+Err\s*\(\s*io::Error(?:::(?:other|new))?\s*\(/.test(body),
  );
  assert.ok(
    restartPreservationReturn &&
      restartPreservationReturn.source.includes(restartPreservation),
    "restart must return the retained carrier after terminal settlement",
  );
  const restartPreservationBinding =
    /let\s+Some\s*\(\s*([a-z_][a-z0-9_]*)\s*\)/.exec(
      restartPreservationReturn.condition,
    )?.[1];
  assert.ok(restartPreservationBinding);
  const restartCarrierError = callBlocks(
    restartPreservationReturn.body,
    /io::Error::(?:other|new)\s*\(/,
  )[0];
  assert.ok(
    restartCarrierError,
    "restart refusal must own the typed preservation carrier",
  );
  const restartCarrierArguments = callArguments(restartCarrierError.source);
  assert.equal(
    restartCarrierArguments.at(-1)?.trim(),
    restartPreservationBinding,
    "restart refusal must pass the carrier itself, not flattened text",
  );
  const restartPreservationReturnIndex = startup.indexOf(
    restartPreservationReturn.source,
  );
  assert.ok(
    restartTerminalIndex < restartPreservationReturnIndex,
    "restart must write AppliedUnverified before returning preservation failure",
  );
  const restartTerminalConstruction = callBlocks(
    startup,
    /PersistedStateRepairTerminal::from_attempt\s*\(/,
  )[0];
  assert.ok(restartTerminalConstruction, "restart must reconstruct a terminal");
  assert.match(
    callArguments(restartTerminalConstruction.source)[1] ?? "",
    new RegExp(`\\b${escapeRegExp(restartOutcome)}\\b`),
    "restart terminal must consume the blocking acknowledgement outcome",
  );
});

test("P01-B02 settles live persisted-state parks after durable plan and off Tokio", async () => {
  const [persistedRepair, anchoredRecord] = await Promise.all([
    read("apps/api/src/state/persisted_state_repair.rs"),
    read("apps/api/src/execution/anchored_record.rs"),
  ]);

  const authorize = functionBlock(
    persistedRepair,
    "authorize_persisted_state_rejected_record_quarantine",
  );
  assert.doesNotMatch(
    authorize,
    /\.still_current\s*\(/,
    "Guardian policy authorization must not perform blocking filesystem work",
  );
  const admit = uniqueMethodBlock(
    persistedRepair,
    "AppState",
    "admit_persisted_state_repair",
  );
  const blockingCurrentness = callBlocks(
    admit,
    /tokio::task::spawn_blocking\s*\(/,
  ).find(({ source }) => /\.still_current\s*\(/.test(source));
  assert.ok(
    blockingCurrentness,
    "State admission must revalidate the move-only record off Tokio",
  );

  const execute = uniqueMethodBlock(
    persistedRepair,
    "AppState",
    "execute_persisted_state_repair",
  );
  const plan = execute.indexOf("create_persisted_state_repair_plan");
  const blockingEffect = callBlocks(
    execute,
    /tokio::task::spawn_blocking\s*\(/,
  ).find(({ source }) => /authorization\.quarantine\s*\(/.test(source));
  const terminal = execute.indexOf("settle_persisted_state_repair_terminal");
  const memory = execute.indexOf("settle_persisted_state_repair_memory");
  assert.ok(plan !== -1 && blockingEffect && terminal !== -1 && memory !== -1);
  assert.ok(
    plan < blockingEffect.index &&
      blockingEffect.index < terminal &&
      terminal < memory,
    "persisted-state repair must order durable plan, blocking effect, terminal, and memory",
  );
  const liveEffect = new RegExp(
    `let\\s*\\(\\s*([a-z_][a-z0-9_]*)\\s*,\\s*([a-z_][a-z0-9_]*)\\s*\\)\\s*:\\s*\\(\\s*PersistedStateRepairTerminalOutcome\\s*,\\s*Option\\s*<\\s*AnchoredRecordQuarantinePreservationError\\s*>\\s*\\)\\s*=\\s*[\\s\\S]{0,320}tokio::task::spawn_blocking`,
  ).exec(execute);
  assert.ok(
    liveEffect,
    "blocking repair must return outcome and typed preservation authority",
  );
  const [, liveOutcome, livePreservation] = liveEffect;
  assertOrdered(
    blockingEffect.source,
    "authorization.quarantine",
    "receipt.is_current",
    "park before exact receipt validation",
  );
  assertOrdered(
    blockingEffect.source,
    "receipt.is_current",
    "receipt.acknowledge_preserved",
    "exact validation before consuming preservation acknowledgement",
  );
  const staleReceipt = conditionalBlocks(blockingEffect.source).find(
    ({ condition }) => /!\s*receipt\.is_current\s*\(/.test(condition),
  );
  assert.ok(staleReceipt, "live repair must classify a stale park receipt");
  assert.match(
    staleReceipt.body,
    /AppliedUnverified|AnchoredRecordQuarantinePreservationError/,
  );
  assert.doesNotMatch(
    staleReceipt.body,
    /PersistedStateRepairTerminalOutcome::Quarantined/,
  );
  const liveAcknowledgement = blockingEffect.source.slice(
    blockingEffect.source.indexOf("receipt.acknowledge_preserved"),
  );
  const acknowledged = matchArmBlocks(
    liveAcknowledgement,
    /Ok\s*\(\s*\(\s*\)\s*\)/,
  )[0];
  const acknowledgementFailure = matchArmBlocks(
    liveAcknowledgement,
    /Err\s*\([^)]*\)/,
  )[0];
  assert.ok(
    acknowledged && acknowledgementFailure,
    "live preservation acknowledgement needs explicit success and failure outcomes",
  );
  assert.match(
    acknowledged.body,
    /PersistedStateRepairTerminalOutcome::Quarantined/,
    "only successful consuming acknowledgement may claim Quarantined",
  );
  assert.match(
    acknowledged.body,
    /None\b/,
    "successful acknowledgement cannot fabricate a preservation failure",
  );
  assert.match(
    acknowledgementFailure.body,
    /PersistedStateRepairTerminalOutcome::AppliedUnverified/,
    "failed acknowledgement must settle as applied-unverified",
  );
  const liveAckError = /Err\s*\(\s*([a-z_][a-z0-9_]*)\s*\)/.exec(
    acknowledgementFailure.marker,
  )?.[1];
  assert.ok(liveAckError, "live acknowledgement failure must bind its error");
  assert.match(
    acknowledgementFailure.body,
    new RegExp(`Some\\s*\\(\\s*${escapeRegExp(liveAckError)}\\s*\\)`),
    "blocking repair must return the exact preservation error",
  );
  assert.doesNotMatch(
    acknowledgementFailure.body,
    /PersistedStateRepairTerminalOutcome::Quarantined/,
    "acknowledgement failure must remain applied-unverified or retain typed authority",
  );
  const outsideBlockingEffect =
    execute.slice(0, blockingEffect.index) +
    execute.slice(blockingEffect.index + blockingEffect.source.length);
  assert.doesNotMatch(
    outsideBlockingEffect,
    /authorization\.(?:still_current|quarantine)\s*\(|receipt\.(?:is_current|acknowledge_preserved)\s*\(/,
    "live filesystem settlement must not leak back onto Tokio workers",
  );
  assert.doesNotMatch(execute, /block_in_place\s*\(/);

  const terminalConstruction = callBlocks(
    execute,
    /PersistedStateRepairTerminal::from_attempt\s*\(/,
  )[0];
  assert.ok(terminalConstruction, "live repair must construct one terminal");
  assert.match(
    callArguments(terminalConstruction.source)[1] ?? "",
    new RegExp(`\\b${escapeRegExp(liveOutcome)}\\b`),
    "terminal must consume the outcome returned by the blocking effect",
  );
  const terminalFailureFlow = execute.slice(terminal, memory);
  assert.match(
    terminalFailureFlow,
    new RegExp(
      `PersistedStateRepairExecutionError::Terminal\\s*\\{[\\s\\S]*?\\bpreservation(?:_error|_failure)?\\s*:\\s*${escapeRegExp(livePreservation)}\\b[\\s\\S]*?\\}`,
    ),
    "terminal settlement failure must retain pending preservation authority",
  );
  const acceptedJournalIndex = execute.indexOf(
    "PersistedStateRepairExecutionError::AcceptedJournalPersistence",
  );
  const finalPreservation = conditionalBlocks(execute).find(({ condition }) =>
    new RegExp(
      `let\\s+Some\\s*\\(\\s*[a-z_][a-z0-9_]*\\s*\\)\\s*=\\s*${escapeRegExp(livePreservation)}\\b`,
    ).test(condition),
  );
  assert.ok(
    finalPreservation,
    "live repair must inspect retained preservation authority after settlement",
  );
  const finalPreservationBinding =
    /let\s+Some\s*\(\s*([a-z_][a-z0-9_]*)\s*\)/.exec(
      finalPreservation.condition,
    )?.[1];
  assert.ok(finalPreservationBinding);
  assert.match(
    finalPreservation.body,
    new RegExp(
      `return\\s+Err\\s*\\(\\s*PersistedStateRepairExecutionError::Preservation\\s*\\(\\s*${escapeRegExp(finalPreservationBinding)}\\s*\\)\\s*\\)\\s*;`,
    ),
    "live repair must return the exact retained preservation authority",
  );
  const finalPreservationIndex = execute.indexOf(finalPreservation.source);
  assert.ok(
    memory < finalPreservationIndex &&
      (acceptedJournalIndex === -1 ||
        finalPreservationIndex < acceptedJournalIndex),
    "preservation authority must survive memory settlement and take precedence afterward",
  );
  const memoryFailureFlow = execute.slice(memory, finalPreservationIndex);
  assert.match(
    memoryFailureFlow,
    new RegExp(
      `PersistedStateRepairExecutionError::Memory\\s*\\{[\\s\\S]*?\\bpreservation(?:_error|_failure)?\\s*:\\s*${escapeRegExp(livePreservation)}\\b[\\s\\S]*?\\}`,
    ),
    "memory settlement failure must retain pending preservation authority",
  );

  const preservationError = itemBlock(
    anchoredRecord,
    "enum",
    "AnchoredRecordQuarantinePreservationError",
  );
  assertMustUse(
    anchoredRecord,
    "enum",
    "AnchoredRecordQuarantinePreservationError",
  );
  assert.match(
    preservationError,
    /\bAcknowledgement\s*\{(?=[^}]*\bFileParkPreservationError\b)(?=[^}]*\bAnchoredRecordDirectory\b)[^}]*\}/s,
    "acknowledgement failure must retain its parked-file and root authority",
  );
  assert.match(
    preservationError,
    /\bIndeterminatePark\s*\{(?=[^}]*\bFileParkObligation\b)(?=[^}]*\bArc\s*<\s*AppRootSession\s*>)[^}]*\}/s,
    "indeterminate park must retain its obligation and root authority",
  );
  const executionError = itemBlock(
    persistedRepair,
    "enum",
    "PersistedStateRepairExecutionError",
  );
  assert.match(
    executionError,
    /\bPreservation\s*\(\s*#\[source\]\s*AnchoredRecordQuarantinePreservationError\s*\)/,
    "final preservation failure must retain and source its exact live park authority",
  );
  assert.match(
    executionError,
    /\bTerminal\s*\{(?=[^}]*\bOperationJournalStoreError\b)(?=[^}]*Option\s*<\s*AnchoredRecordQuarantinePreservationError\s*>)[^}]*\}/s,
    "terminal persistence failure must retain optional preservation authority",
  );
  assert.match(
    executionError,
    /\bMemory\s*\{(?=[^}]*\bFailureMemoryStoreError\b)(?=[^}]*Option\s*<\s*AnchoredRecordQuarantinePreservationError\s*>)[^}]*\}/s,
    "memory persistence failure must retain optional preservation authority",
  );
});

test("P01-B02 wires registered artifact proofs and effects through Guardian settlement", async () => {
  const [executionModule, anchoredRecord, artifact, findings, reconciliation, guardian] =
    await Promise.all([
      read("apps/api/src/execution/mod.rs"),
      read("apps/api/src/execution/anchored_record.rs"),
      read("apps/api/src/execution/registered_artifact.rs"),
      read("apps/api/src/state/registered_artifact_findings.rs"),
      read("apps/api/src/state/reconciliation.rs"),
      read("apps/api/src/guardian/artifact_repair.rs"),
    ]);

  assert.match(executionModule, /^pub\(crate\) mod registered_artifact;$/m);
  assert.doesNotMatch(executionModule, /use anchored_record::registered_artifact/);
  assert.doesNotMatch(
    anchoredRecord,
    /(?:#\[path\s*=\s*"registered_artifact\.rs"\]\s*)?pub\(crate\) mod registered_artifact/,
  );

  const mintMutation = uniqueMethodBlock(
    artifact,
    "RegisteredArtifactMutationCapability",
    "mint",
  );
  assert.match(mintMutation, /root_session:\s*Arc<AppRootSession>/);
  const admitRepair = functionBlock(
    findings,
    "admit_registered_artifact_repair_with_recovery_scope",
  );
  assert.match(
    admitRepair,
    /RegisteredArtifactMutationCapability::mint\s*\(\s*Arc::clone\s*\(\s*self\.root_session\s*\(\s*\)\s*\)\s*,\s*physical_path\s*,?\s*\)/,
  );
  assert.doesNotMatch(findings, /mutation\.is_current\s*\(/);

  assert.match(
    artifact,
    /enum\s+RegisteredArtifactPhysicalState\s*\{[\s\S]*?Exact\s*\(\s*RegisteredArtifactObservedExactProof\s*\)/,
  );
  assert.doesNotMatch(artifact, /RegisteredArtifactPhysicalState::Exact\s*\)/);
  assert.doesNotMatch(artifact, /fn\s+verify_exact\s*\(/);
  const classify = functionBlock(artifact, "classify_registered_artifact");
  assert.match(
    classify,
    /RegisteredArtifactPhysicalState::Exact\s*\(\s*RegisteredArtifactObservedExactProof\s*\{/,
  );

  const verifierMint = uniqueMethodBlock(
    reconciliation,
    "RegisteredManagedArtifactComponentCompletion",
    "begin_commit_postcheck",
  );
  assert.match(
    verifierMint,
    /RegisteredArtifactExactVerifier::mint\s*\(\s*std::sync::Arc::clone\s*\(\s*self\.authority\.durable\.state\.root_session\s*\(\s*\)\s*\)/,
  );
  const exactValidation = uniqueMethodBlock(
    artifact,
    "RegisteredArtifactExactVerification",
    "validate",
  );
  assert.match(
    exactValidation.slice(0, exactValidation.indexOf("{")),
    /self\s*,\s*proof:\s*RegisteredArtifactExactProof/,
  );
  const settlePostcheck = uniqueMethodBlock(
    reconciliation,
    "RegisteredManagedArtifactPendingPostcheck",
    "settle",
  );
  assert.match(settlePostcheck, /self\.verification\.validate\s*\(\s*proof\s*\)\.await/);
  assert.doesNotMatch(settlePostcheck, /verification\.matches|\.is_current\s*\(/);

  assert.doesNotMatch(guardian, /\.verify_exact\s*\(|RollbackState::Available/);
  assert.doesNotMatch(guardian, /(?:report|error)\.facts\b(?!\s*\()/);
  assert.match(
    guardian,
    /#\[must_use[^\]]*\][\s\S]{0,120}enum\s+ArtifactFinishDisposition\s*\{[\s\S]*?Complete\s*\(\s*ArtifactCompletionProof\s*\)[\s\S]*?Continue\s*\(\s*Option\s*<\s*ArtifactContinuationCause\s*>\s*\)[\s\S]*?Propagate[\s\S]*?Option\s*<\s*ArtifactPropagationOwner\s*>/,
  );
  const planned = functionBlock(guardian, "execute_planned_artifact_repair");
  assert.match(
    planned,
    /RegisteredArtifactPhysicalState::Exact\s*\(\s*proof\s*\)[\s\S]{0,160}settle_observed_exact\s*\(/,
  );
  assert.match(planned, /report\.facts\s*\(\s*\)/);
  assert.match(planned, /report\.validate\s*\(\s*\)\.await/);
  assertOrdered(
    planned,
    "report.facts()",
    "report.validate().await",
    "borrow mutation facts before consuming its proof",
  );
  assert.match(planned, /error\.facts\s*\(\s*\)/);
  assert.match(planned, /error\.has_unsettled_effect\s*\(\s*\)/);
  assertCountAtLeast(
    planned,
    /artifact_execution_error\s*\(\s*error\s*\)/,
    3,
    "mutation, acknowledgement, and published validation errors remain exact sources",
  );
  assert.match(
    planned,
    /ArtifactContinuationCause::try_no_effect_mutation\s*\(\s*error\s*\)/,
  );

  const quarantine = between(
    planned,
    "let quarantine_checkpoint = if context.quarantines_existing()",
    "if !context.admission.evidence_is_live()",
  );
  assertOrdered(
    quarantine,
    ".record_checkpoint(",
    ".acknowledge_preserved().await",
    "quarantine checkpoint visibility before acknowledgement",
  );
  const pendingQuarantine = quarantine.indexOf("ArtifactPropagationOwner::PendingQuarantine(");
  assert.notEqual(pendingQuarantine, -1);
  assert.match(
    quarantine.slice(Math.max(0, pendingQuarantine - 900), pendingQuarantine + 160),
    /Err\s*\(\s*error\s*\)\s*=>\s*\{[\s\S]*?return\s+finish_artifact_repair\s*\([\s\S]*?PendingQuarantine\s*\(\s*preservation/,
    "hard checkpoint reconciliation failures must terminalize while owning the park receipt",
  );
  assert.match(
    guardian,
    /"record_quarantine_checkpoint"[\s\S]{0,180}"acknowledge_quarantined_artifact"[\s\S]{0,180}"download_artifact_to_temp"/,
    "quarantine acknowledgement failure must be a planned causal step",
  );
  assert.match(
    quarantine,
    /AcceptedFailure\s*\(\s*error\s*\)[\s\S]{0,120}checkpoint_error\s*=\s*Some\s*\(\s*error\s*\)/,
  );

  const finish = functionBlock(guardian, "finish_artifact_repair");
  assert.doesNotMatch(finish, /\.await\s*\?/);
  assert.match(
    finish,
    /ArtifactTerminal::Repaired\s*\{\s*\.\.\s*\}[\s\S]{0,120}ArtifactFinishDisposition::Complete\s*\(\s*_\s*\)[\s\S]*?ArtifactTerminal::Failed\s*\{\s*\.\.\s*\}[\s\S]{0,120}ArtifactFinishDisposition::Continue\s*\(\s*_\s*\)[\s\S]*?ArtifactTerminal::Failed\s*\{\s*\.\.\s*\}[\s\S]{0,120}ArtifactFinishDisposition::Propagate\s*\{\s*\.\.\s*\}/,
    "terminal and disposition combinations must be checked before settlement",
  );
  assertCountAtLeast(
    finish,
    /retained_disposition_error\s*\(/,
    3,
    "terminal, memory, and accepted persistence failures retain the disposition",
  );
  assertOrdered(
    finish,
    "record_artifact_terminal_reconciled(",
    ".commit_terminal_memory(",
    "terminal journal before failure memory",
  );
  const topLevel = functionBlock(
    guardian,
    "execute_registered_guardian_artifact_repair",
  );
  assert.match(
    topLevel,
    /PendingQuarantine\s*\(\s*preservation\s*\)[\s\S]*?preservation\.acknowledge_preserved\s*\(\s*\)\.await/,
  );
  assert.match(
    topLevel,
    /ArtifactFinishDisposition::Propagate\s*\{\s*error\s*,\s*owner\s*\}[\s\S]*?None\s*=>\s*Err\s*\(\s*error\s*\)/,
    "an exact propagated execution error must remain the direct persistence source",
  );
  assertOrdered(
    topLevel,
    "into_failed_continuation(",
    "cause.settle()",
    "no-effect cause survives continuation conversion",
  );

  const continuationCause = functionBlock(guardian, "try_no_effect_mutation");
  assert.match(
    continuationCause,
    /try_no_effect_mutation[\s\S]*?error\.has_unsettled_effect\s*\(\s*\)[\s\S]*?Err\s*\(\s*error\s*\)[\s\S]*?MutationFailure\s*\(\s*error\s*\)/,
  );
  assert.match(topLevel, /proof\.settle\s*\(\s*\)/);
  assert.match(topLevel, /cause\.settle\s*\(\s*\)/);

  const componentRequired = functionBlock(
    reconciliation,
    "registered_component_required_terminal_matches",
  );
  assert.match(
    componentRequired,
    /journal\.rollback\s*==\s*RollbackState::NotApplicable/,
    "rung-two admission must accept Guardian's truthful no-rollback component-required terminal",
  );
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

terminalTest(
  "P01-B02 retains typed move and cleared-root authority in axial-fs",
  async () => {
    const [library, platform] = await Promise.all([
      read("core/fs/src/lib.rs"),
      read("core/fs/src/platform.rs"),
    ]);

    for (const name of ["FileMoveOutcome", "DirectoryMoveOutcome"]) {
      const outcome = itemBlock(library, "enum", name);
      assert.match(outcome, /Applied\s*\(/);
      assert.match(outcome, /NoEffect\s*\{/);
      assert.match(outcome, /AppliedUnverified\s*\(/);
    }
    for (const name of ["FileMoveObligation", "DirectoryMoveObligation"]) {
      const obligation = itemBlock(library, "struct", name);
      assert.match(obligation, /MoveEffectToken/);
      assert.match(obligation, /reported_success:\s*bool/);
      assert.match(
        uniqueMethodBlock(library, name, "reconcile"),
        /settle_[a-z_]*move\s*\([\s\S]*?self\.reported_success/,
      );
    }
    const topology = functionBlock(library, "classify_move_topology");
    assert.match(topology, /!reported_success/);
    assert.match(topology, /MoveTopology::Indeterminate/);
    const operationState = itemBlock(library, "struct", "OperationState");
    assert.match(operationState, /unsettled_moves:\s*usize/);
    assert.match(
      uniqueMethodBlock(library, "OperationState", "reserve_move_effect"),
      /unsettled_moves[\s\S]*?reserve_effect\s*\(/,
    );
    assert.match(
      uniqueMethodBlock(library, "OperationState", "release_move_count"),
      /unsettled_moves\s*-=\s*1/,
    );
    assert.doesNotMatch(
      uniqueMethodBlock(library, "OperationState", "release_move_count"),
      /release_effect\s*\(/,
    );
    const moveSettle = uniqueMethodBlock(library, "MoveEffectToken", "settle");
    assert.match(
      moveSettle,
      /state\.release_move_count\s*\(\s*\)[\s\S]*?state\.release_effect\s*\(\s*operation\s*\)/,
    );
    assert.match(
      functionBlock(library, "begin_terminal_drain"),
      /unsettled_moves[\s\S]*?ErrorKind::WouldBlock/,
    );
    assert.match(
      uniqueMethodBlock(library, "FileCapability", "same_file"),
      /self\.identity\s*==\s*other\.identity/,
    );
    assert.doesNotMatch(
      library,
      /pub\s+fn\s+[a-z_]*(?:physical|native)[a-z_]*identity/,
      "physical identity must remain private to axial-fs",
    );
    assert.equal(
      (
        platform.match(
          /pub\(crate\)\s+fn\s+rename_directory_no_replace\s*\(/g,
        ) ?? []
      ).length,
      2,
      "both native adapters must implement directory no-replace move",
    );
    for (const [renameName, openerName] of [
      ["move_file_no_replace", "open_file_move_deleter"],
      ["rename_directory_no_replace", "open_directory_move_deleter"],
    ]) {
      const implementations = functionBlocks(platform).filter(
        ({ name }) => name === renameName,
      );
      assert.equal(implementations.length, 2);
      assert.match(
        implementations[1].source,
        new RegExp(`${openerName}\\s*\\(`),
        `Windows ${renameName} must acquire exact delete authority`,
      );
      const opener = functionBlock(platform, openerName);
      assert.match(opener.slice(0, opener.indexOf("{")), /->\s*io::Result<File>/);
      assert.match(opener, /nt_open_relative\s*\(/);
      assert.match(opener, /DELETE_ACCESS/);
      assert.match(opener, /FILE_OPEN_REPARSE_POINT/);
      assert.match(opener, /FILE_SHARE_READ\s*\|\s*FILE_SHARE_WRITE\s*\|\s*FILE_SHARE_DELETE/);
      assert.match(opener, /(?:file_identity|object_identity)\s*\([^)]*\)\?\s*!=\s*expected/);
    }
    const fileMoveDeleter = functionBlock(platform, "open_file_move_deleter");
    assert.match(fileMoveDeleter, /FILE_NON_DIRECTORY_FILE/);
    assert.match(
      fileMoveDeleter,
      /FILE_READ_ATTRIBUTES\s*\|\s*DELETE_ACCESS\s*\|\s*SYNCHRONIZE_ACCESS/,
    );
    const directoryMoveDeleter = functionBlock(
      platform,
      "open_directory_move_deleter",
    );
    assert.match(directoryMoveDeleter, /FILE_DIRECTORY_FILE/);
    assert.match(
      directoryMoveDeleter,
      /FILE_READ_ATTRIBUTES\s*\|\s*DELETE_ACCESS\s*\|\s*SYNCHRONIZE_ACCESS/,
    );
    const fileMove = uniqueMethodBlock(library, "FileCapability", "move_no_replace");
    assert.match(fileMove, /platform::move_file_no_replace\s*\(/);
    const sealedPromotion = uniqueMethodBlock(
      library,
      "SealedStagedFile",
      "promote_no_replace",
    );
    assert.match(sealedPromotion, /platform::rename_no_replace\s*\(/);
    assert.doesNotMatch(sealedPromotion, /move_file_no_replace/);

    const clearOutcome = itemBlock(library, "enum", "RootClearOutcome");
    const clearReceipt = itemBlock(library, "struct", "RootClearReceipt");
    assert.match(clearOutcome, /Cleared\s*\(\s*RootClearReceipt\s*\)/);
    assert.match(clearReceipt, /Option\s*<\s*RootResetAuthority\s*>/);
    assert.match(
      uniqueMethodBlock(library, "RootClearReceipt", "release"),
      /authority\.release\s*\(/,
    );
    assert.match(
      library,
      /impl\s+Drop\s+for\s+RootClearReceipt[\s\S]*?process::abort\s*\(/,
    );
    const clearRoot = uniqueMethodBlock(
      library,
      "RootResetAuthority",
      "clear_root",
    );
    assert.match(clearRoot, /RootClearOutcome::Cleared\s*\(\s*RootClearReceipt/);
    assert.doesNotMatch(clearRoot, /self\.revoke\s*\(|drop\s*\(\s*self\.session/);
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
  const quarantineReceipt = itemBlock(
    anchoredRecord,
    "struct",
    "AnchoredRecordQuarantineReceipt",
  );
  assert.match(
    quarantineReceipt,
    /ParkedFile/,
    "the adapter quarantine receipt must retain exact parked-file authority",
  );
  const acknowledgeQuarantine = uniqueMethodBlock(
    anchoredRecord,
    "AnchoredRecordQuarantineReceipt",
    "acknowledge_preserved",
  );
  assert.match(
    acknowledgeQuarantine.slice(0, acknowledgeQuarantine.indexOf("{")),
    /\bself\b/,
    "adapter preservation acknowledgement must consume its receipt",
  );
  assert.match(
    acknowledgeQuarantine,
    /\.acknowledge_preserved\s*\(\s*\)/,
    "the adapter must explicitly delegate parked-file preservation settlement",
  );
  const validateQuarantine = uniqueMethodBlock(
    anchoredRecord,
    "AnchoredRecordQuarantineReceipt",
    "is_current",
  );
  assert.match(
    validateQuarantine,
    /\.validate_current\s*\(\s*\)/,
    "adapter currentness must delegate to the retained parked-file proof",
  );
  assert.doesNotMatch(
    anchoredRecord,
    /impl\s+Drop\s+for\s+AnchoredRecordQuarantineReceipt\b/,
    "dropping an adapter receipt must not auto-acknowledge preservation",
  );
  assert.equal(await exists("core/performance/src/file_identity.rs"), false);
  assert.doesNotMatch(performanceLibrary, /^mod file_identity;$/m);
  assert.doesNotMatch(
    launchReports,
    /AdmittedFileIdentity|admitted_(?:path_snapshot|unix_identity|file_identity)|GetFileInformationByHandleEx|MetadataExt/,
  );
});

terminalTest(
  "P01-B02 performance managed storage has one capability authority",
  async () => {
    const [
      storage,
      manager,
      state,
      mutation,
      installTests,
      managedState,
      apiState,
      performanceRules,
      qualification,
      health,
      architecture,
      namespaceAdr,
    ] =
      await Promise.all([
        read("core/performance/src/storage.rs"),
        read("core/performance/src/install/manager.rs"),
        read("core/performance/src/state/mod.rs"),
        read("core/performance/src/install/mutation.rs"),
        read("core/performance/src/install/tests.rs"),
        read("apps/api/src/state/performance_managed.rs"),
        read("apps/api/src/state/mod.rs"),
        read("apps/api/src/state/performance_rules.rs"),
        read("apps/api/src/application/performance/qualification.rs"),
        read("core/performance/src/health/mod.rs"),
        read("docs/GUARDIAN-ARCHITECTURE.md"),
        read("docs/adr/0004-performance-internal-namespace-ownership.md"),
      ]);

    const managedDirectory = itemBlock(
      storage,
      "struct",
      "ManagedStorageDirectory",
    );
    assert.match(managedDirectory, /directory\s*:\s*Directory\b/);
    assert.doesNotMatch(managedDirectory, /\bPath(?:Buf)?\b/);
    const constructor = uniqueMethodBlock(
      storage,
      "ManagedStorageDirectory",
      "bind_instance_root",
    );
    assert.match(
      constructor,
      /directory\s*:\s*Directory[\s\S]*effects\s*:\s*ManagedInstanceEffectAuthority/,
    );
    assert.match(constructor, /directory\.identity\s*\(\s*\)[\s\S]*effects\.anchor_identity/);
    assert.doesNotMatch(constructor, /\bPath(?:Buf)?\b/);
    assert.doesNotMatch(storage, /ManagedStorageEffectOwner|ManagedStoragePendingEffect|RetainedManagedStorageEffect|ManagedStorageObligation/);
    const effectState = itemBlock(
      storage,
      "struct",
      "ManagedInstanceEffectState",
    );
    assert.match(effectState, /owner\s*:\s*EffectOwner\b/);
    assert.match(
      effectState,
      /continuation\s*:\s*Mutex\s*<\s*Option\s*<\s*ManagedEffectContinuation\s*>\s*>/,
      "serialized State admission permits exactly one typed continuation",
    );
    assert.doesNotMatch(
      effectState,
      /Vec\s*<|HashMap\s*</,
      "the instance owner cannot become another pending-effect broker",
    );
    const settleOwner = uniqueMethodBlock(
      storage,
      "ManagedInstanceEffectAuthority",
      "settle",
    );
    assertOrdered(
      settleOwner,
      ".owner.settle()",
      "claim_continuation()",
      "owner settlement before typed terminal receipt claim",
    );
    assertOrdered(
      settleOwner,
      "claim_continuation()",
      "require_settled()",
      "typed receipt claim before final clean-owner proof",
    );
    const retainEffect = uniqueMethodBlock(
      storage,
      "ManagedInstanceEffectAuthority",
      "retain_with",
    );
    assert.doesNotMatch(retainEffect, /\b(?:loop|while)\b|mem::forget/);
    assert.equal(
      (retainEffect.match(/self\.settle\s*\(\s*\)/g) ?? []).length,
      1,
      "bounded effect-owner backpressure permits one settlement retry",
    );

    const compositionAuthority = itemBlock(
      manager,
      "struct",
      "ManagedCompositionAuthority",
    );
    assert.match(compositionAuthority, /instances_root_directory\s*:\s*Arc\s*<\s*Directory\s*>/);
    assert.match(compositionAuthority, /WeakManagedInstanceEffectAuthority/);
    assert.doesNotMatch(compositionAuthority, /ManagedStorageDirectory|EffectOwner\b/);
    const bindEffects = uniqueMethodBlock(
      mutation,
      "ManagedCompositionAuthority",
      "bind_instance_effect_authority",
    );
    assertOrdered(
      bindEffects,
      "open_instance_directory(identity).await",
      "instance_effect_authorities",
      "physical instance admission before weak-owner reuse",
    );
    assert.equal(
      (bindEffects.match(/require_effect_anchor\s*\(/g) ?? []).length,
      2,
      "both weak-owner reuse races must validate the current exact anchor",
    );
    assert.match(
      installTests,
      /managed_authority_refuses_a_live_owner_for_a_replaced_instance/,
    );
    const recoverAndInspect = uniqueMethodBlock(
      mutation,
      "ManagedCompositionAuthority",
      "recover_and_inspect",
    );
    assertOrdered(
      recoverAndInspect,
      "recovered_inspection(mods)",
      "final_effects.require_settled()",
      "successful recovery must end with final effect-owner truth",
    );
    const managedEntry = itemBlock(
      managedState,
      "struct",
      "ManagedInstanceEntry",
    );
    assert.match(managedEntry, /effects\s*:\s*OnceLock\s*<\s*ManagedInstanceEffectAuthority\s*>/);
    assert.match(managedEntry, /work_gate\s*:\s*Arc\s*<\s*AsyncMutex/);
    assert.match(managedState, /type\s+ManagedEntries\s*=\s*HashMap\s*<\s*String\s*,\s*ManagedEntrySlot\s*>/);
    assert.match(itemBlock(managedState, "struct", "ManagedEntrySlot"), /entry\s*:\s*Weak\s*<\s*ManagedInstanceEntry\s*>[\s\S]*retained\s*:\s*Option\s*<\s*Arc\s*<\s*ManagedInstanceEntry/);
    const ownedWork = uniqueMethodBlock(
      managedState,
      "AppManagedCompositionAdmission",
      "run_owned",
    );
    assert.match(
      ownedWork,
      /work_gate[\s\S]*self\._lifecycle\.clone\s*\(\s*\)[\s\S]*instance_lifecycle\.retained\s*\(\s*\)/,
    );
    assert.doesNotMatch(ownedWork, /read_owned\s*\(/);
    assert.match(
      ownedWork,
      /phase\s*\(\s*\)\s*!=\s*ManagedEntryPhase::Open[\s\S]*reconciliation_required/,
    );
    assert.match(
      ownedWork,
      /tokio::spawn[\s\S]*ManagedOperationLatch::new[\s\S]*tokio::spawn/,
    );
    assert.match(
      managedState,
      /impl\s+Drop\s+for\s+ManagedOperationLatch[\s\S]*publish_entry_phase/,
      "a supervisor-owned RAII guard must latch worker panic or cancellation",
    );
    const publishPhase = functionBlock(managedState, "publish_entry_phase");
    assert.match(publishPhase, /unwrap_or_else\s*\(\s*\|poisoned\|\s*poisoned\.into_inner\s*\(\s*\)\s*\)/);
    assert.doesNotMatch(publishPhase, /\.expect\s*\(/);
    const bindEntry = uniqueMethodBlock(
      managedState,
      "ManagedCompositionOwner",
      "bind_entry_effects",
    );
    assert.match(
      bindEntry,
      /InstanceLifecycleLease[\s\S]*Arc<OwnedRwLockReadGuard[\s\S]*work_gate[\s\S]*tokio::spawn/,
    );
    assert.doesNotMatch(bindEntry, /read_owned\s*\(/);
    const ensureInstalled = uniqueMethodBlock(
      managedState,
      "AppManagedCompositionAdmission",
      "ensure_installed",
    );
    assert.match(ensureInstalled, /self\._lifecycle\.clone\s*\(\s*\)/);
    assert.doesNotMatch(ensureInstalled, /read_owned\s*\(/);
    assert.match(
      ensureInstalled,
      /phase\s*\(\s*\)\s*!=\s*ManagedEntryPhase::Open[\s\S]*reconciliation_required/,
    );
    assert.match(
      managedState,
      /queued_close_does_not_block_work_owned_by_an_existing_admission[\s\S]*queued_close_does_not_block_binding_owned_by_an_existing_admission/,
    );
    assert.match(
      managedState,
      /foreign_instance_lifecycle_authority_is_rejected[\s\S]*latched_admission_refuses_a_second_operation[\s\S]*operation_queued_behind_a_latching_worker_never_starts/,
    );
    const admitManaged = uniqueMethodBlock(
      managedState,
      "ManagedCompositionOwner",
      "admit",
    );
    const retireManaged = uniqueMethodBlock(
      managedState,
      "ManagedCompositionOwner",
      "retire",
    );
    for (const lifecycleEntry of [admitManaged, retireManaged]) {
      assert.match(lifecycleEntry, /instance_lifecycle\.owns\s*\(\s*&instance_lifecycle\.owner\s*\)/);
    }
    assert.doesNotMatch(
      managedState,
      /struct\s+ManagedCompositionAdmission\b|completed_(?:tx|rx)/,
      "State must not retain duplicate admission wrappers or oneshot supervisors",
    );
    for (const method of [
      "inspect_managed_instance",
      "resolve_managed_instance",
    ]) {
      const entry = functionBlock(apiState, method);
      assert.doesNotMatch(entry, /oneshot|tokio::spawn|completed_(?:tx|rx)/);
      assert.match(entry, /admitted[\s\S]*\.(?:inspect|resolve_and_inspect)\s*\(/);
    }
    assert.match(
      managedState,
      /sequential_clean_instances_release_effect_owner_capacity/,
    );
    const closeOwner = uniqueMethodBlock(
      managedState,
      "ManagedCompositionOwner",
      "close",
    );
    assertOrdered(
      closeOwner,
      ".clear()",
      ".store(ManagedOwnerPhase::Closed",
      "entry-held filesystem owners must drop before managed close publishes Closed",
    );
    const performanceStore = itemBlock(
      performanceRules,
      "struct",
      "AppPerformanceStore",
    );
    assertOrdered(
      performanceStore,
      "managed: ManagedCompositionOwner",
      "_root_session: Arc<AppRootSession>",
      "managed filesystem owners must drop before the terminal root session",
    );
    assert.ok(
      qualification.indexOf("bind_instance_effect_authority") >
        qualification.indexOf("#[cfg(test)]"),
      "direct qualification authority access must remain test-only",
    );

    for (const [path, source] of [
      ["core/performance/src/storage.rs", storage],
      ["core/performance/src/state/mod.rs", state],
      ["core/performance/src/install/mutation.rs", mutation],
      ["core/performance/src/health/mod.rs", health],
    ]) {
      const production = path.endsWith("/storage.rs")
        ? source.slice(source.indexOf("impl ManagedStorageDirectory"))
        : source.split("#[cfg(test)]", 1)[0];
      assert.doesNotMatch(
        production,
        /\.(?:path|canonicalize)\s*\(|\b(?:std::|tokio::)?fs::(?:read_dir|symlink_metadata|metadata|write|rename|hard_link|remove_file|remove_dir|remove_dir_all|create_dir|create_dir_all)\s*\(|\bOpenOptions\b|\bFile::(?:open|create)\s*\(/,
        `${path} retains ambient managed-storage access`,
      );
    }

    for (const typeName of [
      "ManagedCompositionAuthority",
      "ManagedInstanceIdentity",
    ]) {
      assert.doesNotMatch(
        itemBlock(manager, "struct", typeName),
        /\bPath(?:Buf)?\b/,
        `${typeName} retains parallel raw-path authority`,
      );
    }
    assert.doesNotMatch(
      uniqueMethodBlock(manager, "PerformanceManager", "claim_managed_authority"),
      /&?Path(?:Buf)?\b/,
    );

    for (const [path, source] of [
      ["docs/GUARDIAN-ARCHITECTURE.md", architecture],
      ["docs/adr/0004-performance-internal-namespace-ownership.md", namespaceAdr],
    ]) {
      assert.doesNotMatch(
        source,
        /hardlink obligation|restart never reconstructs deletion authority|resynchronization of both retained directory capabilities/i,
        `${path} describes a displaced Performance protocol`,
      );
    }
  },
);

terminalTest(
  "P01-B02 rollback retention is bounded and interruption recoverable",
  async () => {
    const [state, mutation] = await Promise.all([
      read("core/performance/src/state/mod.rs"),
      read("core/performance/src/install/mutation.rs"),
    ]);
    assert.match(
      state,
      /const ROLLBACK_RETAINED_MAX_BYTES:\s*u64\s*=\s*MANAGED_ARTIFACT_MAX_BYTES\s*\*\s*2\s*;/,
    );
    assert.match(
      state,
      /const ROLLBACK_TRANSIENT_MAX_BYTES:\s*u64\s*=\s*ROLLBACK_RETAINED_MAX_BYTES\s*\+\s*MANAGED_ARTIFACT_MAX_BYTES\s*\+\s*ROLLBACK_METADATA_MAX_BYTES\s*;/,
    );

    const save = functionBlock(state, "save_rollback_snapshot_target");
    const strictSnapshotValidation = functionBlock(
      state,
      "validate_rollback_snapshot",
    );
    assertOrdered(
      strictSnapshotValidation,
      "validate_rollback_artifact_budget(snapshot)?",
      "validate_state(state)?",
      "persisted aggregate budget before rollback state or source admission",
    );
    assert.doesNotMatch(
      save,
      /validate_rollback_artifact_budget\s*\(/,
      "snapshot creation cannot retain a redundant save-only aggregate check",
    );
    assertOrdered(
      save,
      "create_file_create_new(Path::new(ROLLBACK_METADATA_FILE_NAME)",
      "complete_rollback_candidate(",
      "rollback durable intent before artifact copies",
    );
    assertOrdered(
      save,
      "candidate.sync()?",
      "complete_rollback_candidate(",
      "rollback intent directory sync before artifact copies",
    );

    const discard = functionBlock(
      state,
      "discard_unresumable_rollback_candidate",
    );
    assert.match(discard, /ROLLBACK_CANDIDATE_DELETE_PREFIX/);
    assertOrdered(
      discard,
      "move_child_directory_no_replace(",
      "tmp.sync()?",
      "candidate deletion transition before durable parent sync",
    );
    assertOrdered(
      discard,
      "tmp.sync()?",
      "delete_rollback_directory_receipt(",
      "durable candidate deletion transition before content deletion",
    );

    const prune = functionBlock(state, "delete_snapshot_directory");
    assertOrdered(
      prune,
      "move_child_directory_no_replace(",
      "history.sync()?",
      "canonical snapshot move before durable deletion receipt",
    );
    assertOrdered(
      prune,
      "history.sync()?",
      "delete_snapshot_receipt(",
      "durable deletion receipt before snapshot content deletion",
    );

    const deleteReceipt = functionBlock(
      state,
      "delete_rollback_directory_receipt",
    );
    assertOrdered(
      deleteReceipt,
      "for artifact in &snapshot.artifacts",
      "let metadata = read_bounded_file(",
      "rollback artifacts before metadata deletion",
    );
    assertOrdered(
      deleteReceipt,
      "let metadata = read_bounded_file(",
      "remove_empty_child(parent, receipt_name)",
      "rollback metadata before empty receipt deletion",
    );

    const recovery = functionBlock(state, "reconcile_rollback_metadata");
    assertOrdered(
      recovery,
      "reconcile_deleted_rollback_candidates(",
      'for entry in complete_entries(&tmp, "rollback candidates")?',
      "candidate deletion receipts before candidate resumption",
    );
    assert.match(recovery, /rollback_directory_storage_bytes\s*\(/);
    assert.match(
      itemBlock(state, "struct", "RetainedRollbackSnapshot"),
      /storage_bytes:\s*u64/,
      "retained rollback accounting must carry actual disk bytes",
    );
    assert.doesNotMatch(
      functionBlock(state, "retained_rollback_storage_bytes"),
      /serde_json::to_vec/,
      "retained rollback accounting cannot estimate metadata by reserialization",
    );
    assert.match(
      functionBlock(state, "read_rollback_candidate"),
      /rollback snapshot contains an unexpected entry/,
      "unknown rollback internals must remain fail-closed",
    );
    assert.match(
      functionBlock(mutation, "classify_state_reconciliation_error"),
      /RollbackCandidateUnresumable[\s\S]*ManagedMutationError::definite\s*\(/,
      "a fully discarded unresumable candidate must return a definite failure",
    );

    for (const focusedTest of [
      "persisted_candidate_rejects_aggregate_budget_before_source_copy",
      "changed_source_discards_an_unresumable_partial_candidate",
      "unknown_candidate_entry_fails_closed_and_is_preserved",
    ]) {
      assert.match(
        state,
        new RegExp(`fn\\s+${focusedTest}\\s*\\(`),
        `missing focused rollback test ${focusedTest}`,
      );
    }
    const persistedBudgetTest = functionBlock(
      state,
      "persisted_candidate_rejects_aggregate_budget_before_source_copy",
    );
    assertOrdered(
      persistedBudgetTest,
      "candidate.join(ROLLBACK_METADATA_FILE_NAME)",
      "reconcile_rollback_metadata(storage.directory())",
      "persisted aggregate metadata before recovery admission",
    );
    assert.match(
      persistedBudgetTest,
      /rollback snapshot exceeds the aggregate artifact budget/,
    );
    assert.match(
      persistedBudgetTest,
      /fs::read_dir\s*\(\s*&candidate\s*\)/,
      "aggregate rejection must prove the candidate received no copied source",
    );
    const changedSourceTest = functionBlock(
      state,
      "changed_source_discards_an_unresumable_partial_candidate",
    );
    assert.match(changedSourceTest, /changed-managed-second/);
    assert.match(changedSourceTest, /RollbackCandidateUnresumable/);
    assert.match(changedSourceTest, /assert!\s*\(\s*!candidate\.exists\s*\(\s*\)\s*\)/);
    const unknownEntryTest = functionBlock(
      state,
      "unknown_candidate_entry_fails_closed_and_is_preserved",
    );
    assert.match(unknownEntryTest, /unknown\.bin/);
    assert.match(
      unknownEntryTest,
      /rollback snapshot contains an unexpected entry/,
    );
    assert.match(unknownEntryTest, /read preserved unknown entry/);

    const restore = functionBlock(
      state,
      "restore_rollback_snapshot_classified",
    );
    const compensationStart = restore.indexOf("if let Err(error) = result");
    const compensationEnd = restore.indexOf(
      "return Err(RollbackRestoreError::Indeterminate(error))",
      compensationStart,
    );
    assert.notEqual(compensationStart, -1, "missing rollback compensation branch");
    assert.notEqual(compensationEnd, -1, "missing rollback compensation terminal");
    const compensation = restore.slice(compensationStart, compensationEnd);
    assertOrdered(
      compensation,
      ".settle_pending_effects()",
      "reconcile_state_publication(instance_mods)",
      "pending effect settlement before authoritative state reconciliation",
    );
    assertOrdered(
      compensation,
      "reconcile_state_publication(instance_mods)",
      "load_state_admitted(instance_mods)",
      "state reconciliation before authoritative compensation reload",
    );
    assertOrdered(
      compensation,
      "load_state_admitted(instance_mods)",
      "reconcile_managed_addition_obligations(instance_mods, authoritative.as_ref())",
      "authoritative reload before addition compensation",
    );
    assertOrdered(
      compensation,
      "load_state_admitted(instance_mods)",
      "reconcile_managed_removal_obligations(instance_mods, authoritative.as_ref())",
      "authoritative reload before removal compensation",
    );
    assert.doesNotMatch(
      compensation,
      /current\.as_ref\s*\(\s*\)/,
      "compensation cannot reuse pre-commit state after a failed restore",
    );

    const restoreFault = itemBlock(
      state,
      "enum",
      "RollbackRestoreFaultPoint",
    );
    assert.match(restoreFault, /BeforeStatePublication/);
    assert.match(restoreFault, /AfterStatePublication/);
    const restoreGraph = functionBlock(state, "restore_snapshot_graph");
    assertOrdered(
      restoreGraph,
      "RollbackRestoreFaultPoint::BeforeStatePublication",
      "save_state(instance_mods, state)?",
      "pre-publication rollback fault boundary",
    );
    assertOrdered(
      restoreGraph,
      "save_state(instance_mods, state)?",
      "RollbackRestoreFaultPoint::AfterStatePublication",
      "post-publication rollback fault boundary",
    );
    assertOrdered(
      restoreGraph,
      "RollbackRestoreFaultPoint::AfterStatePublication",
      "reconcile_managed_addition_obligations(instance_mods, snapshot.state())",
      "post-publication fault before cleanup",
    );
    for (const runtimeTest of [
      "rollback_failure_before_state_publication_preserves_old_authority",
      "rollback_failure_after_state_publication_preserves_target_authority",
    ]) {
      const body = functionBlock(state, runtimeTest);
      assert.match(body, /restore_with_fault\s*\(/);
      assert.match(body, /assert_compensated_restore\s*\(/);
    }
    const compensationProof = functionBlock(state, "assert_compensated_restore");
    for (const proof of [
      /load_state_admitted\s*\(/,
      /fs::read\s*\(/,
      /managed_effect_reconciliation_required\s*\(/,
      /preflight_managed_inspection_reconciliation\s*\(/,
      /ManagedInspectionReconciliation::default\s*\(\s*\)/,
      /prove_managed_storage_recovered\s*\(/,
      /assert_no_pending_park_receipts\s*\(/,
    ]) {
      assert.match(
        compensationProof,
        proof,
        `rollback compensation proof is missing ${proof}`,
      );
    }
  },
);

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
  "P01-B02 reset retries are bounded and external data is identity-protected",
  async () => {
    const [configSources, desktopState, fsLibrary, fsPlatform] = await Promise.all([
      readRustTree("core/config/src"),
      read("apps/desktop/src/state/mod.rs"),
      read("core/fs/src/lib.rs"),
      read("core/fs/src/platform.rs"),
    ]);
    const combinedConfig = configSources.map(([, source]) => source).join("\n");
    const appRootSession = implementationBlock(
      combinedConfig,
      "AppRootSession",
    );
    const drainDriver = functionBlocks(appRootSession).find(
      ({ name, source }) =>
        /^pub (?:async )?fn/.test(source) &&
        /reset/.test(name) &&
        /ResetStartOutcome::Failed/.test(source),
    );
    assert.ok(drainDriver, "AppRootSession must own reset drain failure settlement");
    const resetRetry = itemBlock(
      combinedConfig,
      "enum",
      "AppRootResetRetry",
    );
    for (const [variant, carrier] of [
      ["Pending", "ResetDrainAuthority"],
      ["Recovery", "ResetDrainRecovery"],
      ["Failed", "ResetDrainFailure"],
      ["Clear", "RootClearFailure"],
    ]) {
      assert.match(
        resetRetry,
        new RegExp(`${variant}\\s*\\(\\s*${carrier}\\s*\\)`),
        `reset retry slot must retain exact ${variant} authority`,
      );
    }
    assert.doesNotMatch(
      resetRetry,
      /Drain\s*\(\s*ResetDrainFailure\s*\)/,
      "failed drain authority must have an explicit state-specific retry variant",
    );
    assert.match(
      drainDriver.source,
      /ResetStartOutcome::Failed\s*\(failure\)\s*=>\s*\{[\s\S]*?retain_reset_retry\s*\([\s\S]*?AppRootResetRetry::Failed\s*\(failure\)[\s\S]*?return\s+Err\s*\(/,
      "a failed reset drain must return a bounded error while retaining exact retry authority",
    );
    const failedDrainBranch = drainDriver.source.match(
      /ResetStartOutcome::Failed\s*\(failure\)\s*=>\s*\{([\s\S]*?)\n\s*\}/,
    )?.[1];
    assert.ok(failedDrainBranch, "reset drain failure branch must remain explicit");
    assert.doesNotMatch(
      failedDrainBranch,
      /(?:sleep|yield_now)\s*\(|failure\.retry\s*\(/,
      "a permanent reset drain failure cannot be polled forever inside one attempt",
    );
    assert.match(
      drainDriver.source,
      /ResetStartOutcome::Pending\s*\(drain\)[\s\S]{0,160}?probes\s*==\s*RESET_SETTLEMENT_MAX_PROBES[\s\S]{0,300}?AppRootResetRetry::Pending\s*\(drain\)[\s\S]{0,160}?reset_settlement_would_block\s*\(/,
      "exhausted pending reset must retain its exact drain and return WouldBlock",
    );
    assert.match(
      drainDriver.source,
      /ResetStartOutcome::Recovery\s*\{\s*recovery\s*\}[\s\S]{0,160}?probes\s*==\s*RESET_SETTLEMENT_MAX_PROBES[\s\S]{0,300}?AppRootResetRetry::Recovery\s*\(recovery\)[\s\S]{0,160}?reset_settlement_would_block\s*\(/,
      "exhausted reset recovery must retain its exact authority and return WouldBlock",
    );
    const probeCount = Number(
      combinedConfig.match(
        /const\s+RESET_SETTLEMENT_MAX_PROBES\s*:\s*usize\s*=\s*(\d+)\s*;/,
      )?.[1],
    );
    const initialDelay = Number(
      combinedConfig.match(
        /const\s+RESET_SETTLEMENT_INITIAL_DELAY\s*:\s*Duration\s*=\s*Duration::from_millis\s*\(\s*(\d+)\s*\)/,
      )?.[1],
    );
    const maximumDelay = Number(
      combinedConfig.match(
        /const\s+RESET_SETTLEMENT_MAX_DELAY\s*:\s*Duration\s*=\s*Duration::from_millis\s*\(\s*(\d+)\s*\)/,
      )?.[1],
    );
    assert.equal(probeCount, 8, "reset settlement must stop after eight probes");
    assert.equal(initialDelay, 25, "reset settlement backoff must start at 25ms");
    assert.equal(maximumDelay, 250, "reset settlement backoff must cap at 250ms");
    assert.equal(
      Array.from({ length: probeCount }, (_, probe) =>
        Math.min(initialDelay * 2 ** probe, maximumDelay),
      ).reduce((total, delay) => total + delay, 0),
      1375,
      "one unsettled reset attempt must have an approximately 1.4s delay budget",
    );
    const probeDelay = functionBlock(
      combinedConfig,
      "reset_settlement_probe_delay",
    );
    assert.match(probeDelay, /saturating_mul\s*\(\s*multiplier\s*\)/);
    assert.match(probeDelay, /\.min\s*\(\s*RESET_SETTLEMENT_MAX_DELAY\s*\)/);
    assert.match(
      functionBlock(combinedConfig, "reset_settlement_would_block"),
      /io::ErrorKind::WouldBlock/,
    );
    const boundedProbeSteps = drainDriver.source.match(
      /sleep\s*\(\s*reset_settlement_probe_delay\s*\(\s*probes\s*\)\s*\)\.await;\s*probes\s*\+=\s*1/g,
    ) ?? [];
    assert.ok(
      boundedProbeSteps.length >= 3,
      "every pending, recovery, and failed-retry probe must spend bounded backoff budget",
    );
    assert.doesNotMatch(
      drainDriver.source,
      /RESET_SETTLEMENT_PROBE_DELAY|Duration::from_millis\s*\(\s*10\s*\)/,
      "reset settlement cannot retain the former unbounded 100Hz poll loop",
    );
    assert.match(
      combinedConfig,
      /active_reader_refuses_reset_and_restores_the_live_session[\s\S]*?root_directory\s*\(\)[\s\S]*?retry reset preflight/,
      "active reset refusal must prove that the live root session is restored for retry",
    );
    assert.match(
      combinedConfig,
      /unsettled_recovery_is_bounded_and_retries_the_exact_authority[\s\S]*?WouldBlock[\s\S]*?restore exact parked directory[\s\S]*?settled recovery retry/,
      "an identity-disrupted recovery must exhaust its budget, repair, and retry exact authority",
    );
    assert.match(
      combinedConfig,
      /validate_absolute_directory_outside_root\s*\(/,
      "reset config preflight must consume native external-directory containment proof",
    );
    assert.match(
      fsLibrary,
      /pub fn validate_absolute_directory_outside_root\s*\(/,
      "the root session must expose native outside-root validation",
    );
    assert.match(
      fsPlatform,
      /validate_absolute_directory_outside_root[\s\S]*?guard[\s\S]*?bindings[\s\S]*?binding\.identity\s*==\s*root\.identity/,
      "outside-root validation must compare retained ancestry identities with the retained root",
    );
    assert.match(
      desktopState,
      /result\s*==\s*Err\s*\(TerminalFailure::ResetPreflight\)[\s\S]{0,240}?state\.completed\s*=\s*None[\s\S]{0,160}?state\.intent\s*=\s*None/,
      "a pre-effect reset refusal must publish its attempt and release the terminal claim",
    );
  },
);

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
      /(?:yield_now|sleep)\s*\([^;]*\)\s*\.await/,
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
    assert.match(appReset, /state\.config\(\)\.paths\(\)/);
    assert.match(appReset, /state\.config\(\)\.current\(\)/);
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

    const managedFs = await read("core/minecraft/src/managed_fs.rs");
    assert.match(installFlight, /ManagedLibraryOperation/);
    assert.match(managedFs, /install_flights:\s*Mutex<HashMap<PortablePathKey,/);
    assert.match(managedFs, /pub\(crate\) fn install_flight\(/);
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
