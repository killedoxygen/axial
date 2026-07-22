import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

function functionBlock(source, name) {
  const start = source.search(new RegExp(`\\bfn\\s+${name}\\s*\\(`));
  assert.notEqual(start, -1, `missing ${name}`);
  const brace = source.indexOf("{", start);
  let depth = 0;
  for (let index = brace; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    if (source[index] === "}") depth -= 1;
    if (depth === 0) return source.slice(start, index + 1);
  }
  assert.fail(`unterminated ${name}`);
}

test("transient stages reserve one continuous root-owned effect", async () => {
  const [library, transient] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/transient.rs"),
  ]);
  const create = functionBlock(transient, "create_stage");
  assert.ok(
    create.indexOf("TransientEffectToken::reserve") <
      create.indexOf("platform::create_transient_file"),
    "effect authority must be reserved before native creation",
  );
  assert.match(
    transient,
    /struct TransientStage[\s\S]*?token:\s*Option<TransientEffectToken>/,
  );
  assert.match(
    transient,
    /enum TransientCreationState[\s\S]*?Stage\(TransientStage\)[\s\S]*?Reservation/,
  );
  assert.match(
    transient,
    /enum TransientPublicationState[\s\S]*?LinkUncertain[\s\S]*?Linked[\s\S]*?Published[\s\S]*?token:\s*TransientEffectToken/,
  );
  assert.match(
    transient,
    /enum TransientEffectDisposition[\s\S]*?NoEffect[\s\S]*?Published[\s\S]*?Indeterminate/,
  );
  assert.match(
    transient,
    /struct TransientEffectRecord[\s\S]*?directory:\s*Directory[\s\S]*?destination:\s*LeafName[\s\S]*?identity:\s*Option<platform::Identity>/,
  );
  assert.ok(
    create.indexOf("self.directory.validate(&operation)") <
      create.indexOf("platform::create_transient_file"),
    "the destination directory must be revalidated immediately before native creation",
  );
  assert.match(
    transient,
    /enum TransientDiscardState[\s\S]*?Stage\(TransientStage\)[\s\S]*?Registry\(TransientEffectToken\)/,
  );
  assert.match(transient, /impl Drop for TransientEffectToken[\s\S]*?self\.abandon\(\)/);
  assert.doesNotMatch(transient, /process::abort|mem::forget|let _ = platform::/);
  assert.match(library, /transients:\s*HashMap<u64,\s*transient::TransientEffectRecord>/);
  assert.match(
    functionBlock(library, "validate_terminal_registry_state"),
    /transients\.len\(\)[\s\S]*?TransientEffectPhase::Abandoned/,
  );
  assert.match(library, /cleanup_abandoned_transient\(id\)/);
  for (const reservation of [
    "register_stage_record",
    "reserve_stage_create",
    "reserve_directory_create",
    "register_file_park",
    "register_directory_park",
    "prepare_stage_promotion",
  ]) {
    assert.match(functionBlock(library, reservation), /transient_leaf_is_reserved/);
  }
  assert.match(
    transient,
    /fn transient_destination_is_reserved[\s\S]*?state\.unsettled_moves\s*!=\s*0/,
  );
  assert.match(
    library,
    /impl MoveEffectToken[\s\S]*?fn reserve[\s\S]*?!state\.transients\.is_empty\(\)/,
  );
  assert.match(
    functionBlock(library, "register_directory_park"),
    /transient_directory_identity_is_reserved/,
  );
  assert.match(
    functionBlock(library, "open_file"),
    /ensure_leaf_not_transient_reserved/,
  );
  const cleanup = functionBlock(transient, "cleanup_abandoned_transient");
  assert.match(cleanup, /TransientEffectDisposition::NoEffect[\s\S]*?=>\s*Ok/);
  assert.match(
    cleanup,
    /TransientEffectDisposition::Published[\s\S]*?TransientEffectDisposition::Indeterminate[\s\S]*?Some\(identity\)[\s\S]*?validate_terminal_publication/,
  );
  const terminalProof = functionBlock(transient, "validate_terminal_publication");
  assert.match(
    terminalProof,
    /file_binding_state[\s\S]*?BindingState::Exact[\s\S]*?exact_file_link_count[\s\S]*?Some\(1\)[\s\S]*?validate_portable_destination_with_operation/,
  );
  assert.match(terminalProof, /validate\(\)\?;[\s\S]*?sync_directory[\s\S]*?validate\(\)/);
  assert.match(
    functionBlock(transient, "validate_exact_publication"),
    /validate_exact_destination/,
  );
  assert.doesNotMatch(
    cleanup,
    /\.entries\(/,
    "terminal cleanup must not enter a draining authority",
  );
  assert.doesNotMatch(transient, /fn validate_portable_destination\s*\(/);
  assert.doesNotMatch(
    transient,
    /TransientCloseObligation|NativeCleanup|staging_directory|stage_name/,
  );
});

test("unsupported Unix targets retain no unauthenticated recovery authority", async () => {
  const [transient, platform] = await Promise.all([
    read("core/fs/src/transient.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  assert.doesNotMatch(transient, /Sha256|journal/i);
  assert.match(platform, /cfg\(not\(target_os = "linux"\)\)[\s\S]*?fn unsupported_transient/);
  assert.match(platform, /managed transient files require durable namespace authority/);
  assert.doesNotMatch(platform, /TRANSIENT_JOURNAL|transient_journal|HMAC/i);
});

test("native transient publication uses the intended platform primitives", async () => {
  const platform = await read("core/fs/src/platform.rs");
  const windows = platform.slice(platform.indexOf("#[cfg(windows)]"));
  assert.match(platform, /OFlags::TMPFILE\s*\|\s*OFlags::RDWR/);
  assert.match(platform, /\/proc\/self\/fd/);
  assert.match(platform, /rfs::linkat\([\s\S]*?AtFlags::SYMLINK_FOLLOW/);
  assert.doesNotMatch(platform, /AtFlags::EMPTY_PATH|rollback_transient_publication/);
  assert.match(platform, /enum TransientPublicationState[\s\S]*?Unpublished[\s\S]*?Published[\s\S]*?Indeterminate/);
  assert.match(platform, /discard_transient_file[\s\S]*?retained_file_identity[\s\S]*?external link/);
  assert.match(platform, /FinishTransientPublicationError::Retained/);
  assert.match(windows, /enum TransientFile\s*\{\}/);
  assert.match(
    functionBlock(windows, "create_transient_file"),
    /CreateTransientFileError::NoEffect\(unsupported_transient\(\)\)/,
  );
  assert.match(windows, /managed transient files require a documented Windows publication primitive/);
  assert.doesNotMatch(
    platform,
    /FILE_DELETE_ON_CLOSE|FILE_LINK_INFORMATION|FileLinkInformation|TransientCloseObligation|windows-transient-native-proof/,
  );
});
