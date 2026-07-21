import assert from "node:assert/strict";
import { access, readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repository = fileURLToPath(new URL("../../../", import.meta.url));
const contractPhase = process.env.P01_B02_CONTRACT_PHASE ?? "terminal";
const terminalTest = contractPhase === "terminal" ? test : test.skip;

const read = (path) => readFile(join(repository, path), "utf8");

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
  const marker = new RegExp(`(?:pub\\([^)]*\\)\\s+)?fn\\s+${name}\\s*\\(`);
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
  const marker = /(?:pub(?:\([^)]*\))?\s+)?fn\s+([a-zA-Z0-9_]+)\s*\(/g;
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

const assertOrdered = (source, before, after, label) => {
  const beforeIndex = source.indexOf(before);
  const afterIndex = source.indexOf(after);
  assert.notEqual(beforeIndex, -1, `missing ${label} start: ${before}`);
  assert.notEqual(afterIndex, -1, `missing ${label} end: ${after}`);
  assert.ok(beforeIndex < afterIndex, `${label} is out of order`);
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

  for (const outcome of [
    "DirectoryCreateOutcome",
    "FileCreateOutcome",
    "FilePromotionOutcome",
    "FileRemovalOutcome",
    "DirectoryRemovalOutcome",
  ]) {
    const declaration = library.slice(library.indexOf(`pub enum ${outcome}`));
    const body = declaration.slice(0, declaration.indexOf("\n}"));
    assert.match(body, /NoEffect/);
    assert.match(body, /AppliedUnverified/);
  }
  const obligations = new Set();
  for (const match of library.matchAll(
    /pub enum ([A-Za-z0-9_]+Outcome)\s*\{/g,
  )) {
    const end = library.indexOf("\n}", match.index);
    assert.notEqual(end, -1, `unterminated ${match[1]}`);
    const outcome = library.slice(match.index, end + 2);
    if (!outcome.includes("AppliedUnverified")) continue;
    const obligation = outcome.match(
      /AppliedUnverified(?:\(\s*|\s*\{[\s\S]{0,200}?obligation:\s*)([A-Za-z0-9_]+Obligation)\b/,
    );
    assert.ok(
      obligation,
      `${match[1]} must retain an operation-specific applied-unverified obligation`,
    );
    obligations.add(obligation[1]);
  }
  assert.ok(
    obligations.size >= 5,
    "each mutating operation needs an obligation",
  );
  for (const obligation of obligations) {
    assert.match(library, new RegExp(`pub struct ${obligation}\\b`));
    const implementationStart = library.indexOf(`impl ${obligation}`);
    const implementationEnd = library.indexOf("\n}", implementationStart);
    assert.notEqual(implementationStart, -1);
    assert.notEqual(implementationEnd, -1);
    const implementation = library.slice(
      implementationStart,
      implementationEnd + 2,
    );
    assert.match(
      implementation,
      /pub fn (?:reconcile|settle)\((?:mut )?self\)/,
      `${obligation} settlement must consume the retained obligation`,
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
    /openat\([\s\S]*?OsStr::new\("\."\)/.test(source),
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

  assert.match(windows, /struct RootGuard/);
  assert.match(windows, /fn open_or_create_root\(/);
  assert.match(windows, /NtCreateFile\(/);
  assert.match(windows, /InitializeObjectAttributes\(/);
  assert.match(windows, /OBJ_CASE_INSENSITIVE/);
  assert.match(
    windows,
    /InitializeObjectAttributes\([\s\S]*?OBJ_CASE_INSENSITIVE[\s\S]*?parent\.as_raw_handle\(\)\.cast\(\)/,
  );
  assert.match(windows, /parent\.as_raw_handle\(\)\.cast\(\)/);
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
  const windowsFunctions = functionBlocks(windows);
  for (const [removal, objectFlag] of [
    ["remove_file", "FILE_NON_DIRECTORY_FILE"],
    ["remove_empty_directory", "FILE_DIRECTORY_FILE"],
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
    assert.doesNotMatch(
      cleanupOpen.source,
      /FILE_SHARE_DELETE/,
      "the transient cleanup handle must not share delete authority",
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
});

test("P01-B02 enumeration retains opaque cleanup tokens and reports overflow", async () => {
  const library = await read("core/fs/src/lib.rs");
  const entryStart = library.indexOf("pub struct DirectoryEntry");
  const entryEnd = library.indexOf("\n}", entryStart);
  assert.notEqual(entryStart, -1);
  assert.notEqual(entryEnd, -1);
  const entry = library.slice(entryStart, entryEnd + 2);
  assert.match(entry, /(?:name|leaf): (?:OsString|LeafName)/);
  assert.doesNotMatch(entry, /(?:name|leaf): String/);

  const entryImplementation = library.slice(
    library.indexOf("impl DirectoryEntry"),
    library.indexOf("#[derive", library.indexOf("impl DirectoryEntry") + 1),
  );
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
});

test("P01-B02 owns capability-safe park restore and replacement primitives", async () => {
  const library = await read("core/fs/src/lib.rs");
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
  const productionLibrary = library.split("#[cfg(test)]", 1)[0];
  const acquire = functionBlock(library, "acquire");

  const authority = itemBlock(library, "struct", "CapabilityAuthority");
  assert.match(authority, /Mutex</);
  assert.match(authority, /Condvar/);
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
  assert.match(
    itemBlock(library, "struct", "DirectoryInner"),
    /Weak<CapabilityAuthority>/,
  );
  assert.match(
    itemBlock(library, "struct", "FileCapability"),
    /Weak<CapabilityAuthority>/,
  );
  const permitName = library.match(
    /struct ([A-Za-z0-9_]*(?:OperationPermit|OperationGuard)[A-Za-z0-9_]*)\s*\{[\s\S]*?Arc<CapabilityAuthority>/,
  )?.[1];
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
    /(?:active|in_flight|operations)[\s\S]{0,120}?(?:\+=\s*1|(?:checked|saturating)_add\(1\))/,
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
  assert.match(enter, /platform::validate_lease\(&self\.lease\)/);
  assert.match(enter, /platform::validate_root\(&self\.root\)/);
  assert.match(enter, new RegExp(`${permitName}\\s*\\{`));
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
  assert.match(permitDrop, /notify_(?:one|all)\(/);
  assertOrdered(
    permitDrop,
    activeDecrement[0],
    "notify_",
    "permit decrement before drain notification",
  );
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
  const beginReset = functionBlock(library, "begin_reset");
  assert.match(beginReset.split("{")[0], /begin_reset\(self\)/);
  const drain = [
    beginReset,
    ...functionBlocks(productionLibrary).map(({ source }) => source),
  ].find(
    (source) =>
      /AUTHORITY_DRAINING|\bDraining\b/.test(source) &&
      /(?:active|in_flight|operations)/.test(source) &&
      /\.wait(?:_while)?\(/.test(source),
  );
  assert.ok(
    drain,
    "reset must mark DRAINING while holding the gate and wait for active permits",
  );
  const drainingMarker = drain.match(/AUTHORITY_DRAINING|\bDraining\b/)[0];
  const waitMarker = drain.includes(".wait_while(") ? ".wait_while(" : ".wait(";
  assertOrdered(
    drain,
    ".lock()",
    drainingMarker,
    "reset gate lock before DRAINING",
  );
  assertOrdered(
    drain,
    drainingMarker,
    waitMarker,
    "terminal reset transition before active-operation drain",
  );
  assert.match(
    drain,
    /(?:active|in_flight|operations)[\s\S]{0,300}?(?:==|>|!=)\s*0/,
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
    assert.match(bootstrap, /pub fn open_app_root_session\(/);
    assert.match(bootstrap, /Result<AppRootSession/);
    const openSession = functionBlock(bootstrap, "open_app_root_session");
    assert.match(
      openSession,
      /current_exe\(|executable|process_image/,
      "startup must capture executable ancestry as part of root admission",
    );
    assert.match(
      combinedConfig,
      /pub struct AppRootSession[\s\S]{0,2400}?(?:executable|process_image)/,
      "the retained session must own executable ancestry proof",
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
      assert.match(
        binary,
        /Arc::new\([^)]*root_session[^)]*\)|root_session\.clone\(\)|Arc::clone\(&[a-z_]*root_session\)/,
      );
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
      performanceOperations,
      benchmarkDrivers,
    ] = await Promise.all([
      read("apps/desktop/src/commands/mod.rs"),
      read("core/config/src/lib.rs"),
      readRustTree("core/config/src"),
      read("core/config/src/paths/mod.rs"),
      read("core/minecraft/src/loaders/install_flight.rs"),
      read("apps/api/src/state/performance_operations.rs"),
      read("apps/api/src/state/benchmark_suite_drivers.rs"),
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
    ]);
    assert.match(desktopCommands, /begin_reset\(/);
    assert.match(desktopCommands, /relaunch|restart/i);
    const combinedConfig = configSources.map(([, source]) => source).join("\n");
    const executableResetProof = functionBlocks(combinedConfig).find(
      ({ name, source }) =>
        /^pub fn/.test(source) &&
        /reset|executable|process_image/.test(name) &&
        /executable|process_image|DirectoryIdentity/.test(source),
    );
    assert.ok(
      executableResetProof,
      "AppRootSession needs a physical executable-in-root reset refusal",
    );
    const resetFunctions = functionBlocks(desktopCommands)
      .filter(({ name }) => name.includes("reset"))
      .map(({ source }) => source)
      .join("\n");
    assert.match(
      resetFunctions,
      new RegExp(`\\b${executableResetProof.name}\\(`),
      "desktop reset must consult the startup-captured executable proof",
    );

    assert.match(installFlight, /DirectoryIdentity/);
    assert.match(installFlight, /PortablePathKey/);
    assert.doesNotMatch(installFlight, /namespace:\s*PathBuf|canonicalize\(/);
    assert.doesNotMatch(performanceOperations, /\.names\(\)/);
    assert.doesNotMatch(benchmarkDrivers, /\.names\(\)/);
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
