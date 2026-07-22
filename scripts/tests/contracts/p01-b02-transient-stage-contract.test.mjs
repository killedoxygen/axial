import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repository = fileURLToPath(new URL("../../../", import.meta.url));
const read = (path) => readFile(join(repository, path), "utf8");

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

function functionBlock(source, name) {
  const start = source.search(
    new RegExp(`\\bfn\\s+${name}(?:<[^>]*>)?\\s*\\(`),
  );
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

test("transient stages retain one admission-owned root effect", async () => {
  const [library, transient, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/transient.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  const create = functionBlock(transient, "create_stage");
  assert.doesNotMatch(
    create,
    /TransientEffectToken::reserve|validate_portable_destination_with_operation|validate_destination_batch_with_operation/,
  );
  assert.match(create, /self\s*\.token\s*\.take\(\)/);
  assert.match(
    transient,
    /struct TransientStage[\s\S]*?token:\s*Option<TransientEffectToken>/,
  );
  assert.match(
    transient,
    /struct TransientDestination[\s\S]*?token:\s*Option<TransientDestinationToken>/,
  );
  assert.match(
    transient,
    /#\[must_use = "admitted transient destinations retain filesystem effect authority"\][\s\S]*?pub struct TransientDestination/,
  );
  assert.match(
    transient,
    /enum TransientCreationState\s*\{\s*Stage\(TransientStage\),?\s*\}/,
  );
  assert.match(
    transient,
    /struct TransientPublicationBatchObligation[\s\S]*?batch:\s*Option<TransientPublicationBatch>/,
  );
  assert.match(
    transient,
    /enum TransientPublicationMember[\s\S]*?Published\(FileCapability\)[\s\S]*?Unpublished\(TransientStageSealed\)/,
  );
  assert.match(
    transient,
    /struct TransientPublicationTransition[\s\S]*?stage:\s*Option<TransientStageSealed>[\s\S]*?retained:\s*Option<platform::TransientFile>[\s\S]*?token:\s*Option<TransientEffectToken>/,
  );
  assert.match(
    transient,
    /enum TransientEffectDisposition[\s\S]*?NoEffect[\s\S]*?Published[\s\S]*?Indeterminate/,
  );
  assert.match(
    transient,
    /struct TransientEffectRecord[\s\S]*?directory:\s*Directory[\s\S]*?destination:\s*LeafName[\s\S]*?identity:\s*Option<platform::Identity>[\s\S]*?retained:\s*Option<platform::TransientFile>/,
  );
  assert.ok(
    create.indexOf("self.directory.validate(&operation)") <
      create.indexOf("platform::create_transient_file"),
    "the destination directory must be revalidated immediately before native creation",
  );
  assert.match(
    transient,
    /enum TransientDiscardState[\s\S]*?Stage\(TransientStage\)[\s\S]*?ReservationRestore/,
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
  const transientReservation = functionBlock(
    transient,
    "transient_destination_is_reserved",
  );
  assert.match(
    transientReservation,
    /state\.moves\.values\(\)\.any[\s\S]*?move_conflicts_with_transient/,
  );
  assert.match(
    library,
    /impl MoveEffectToken[\s\S]*?fn reserve[\s\S]*?state\.transients\.values\(\)\.any[\s\S]*?move_conflicts_with_transient/,
  );
  const moveConflict = functionBlock(library, "move_conflicts_with_transient");
  assert.match(moveConflict, /movement\.source/);
  assert.match(moveConflict, /movement\.destination/);
  assert.match(moveConflict, /moved_directory[\s\S]*?directory_has_physical_ancestor/);
  assert.doesNotMatch(`${library}\n${transient}`, /unsettled_moves/);
  for (const testName of [
    "move_conflicts_cover_portable_source_and_destination_aliases",
    "directory_moves_conflict_with_descendants_but_not_sibling_trees",
    "move_and_transient_reservations_reject_conflicts_in_either_order",
    "unrelated_sibling_tree_reservations_proceed_together",
    "simultaneous_move_and_transient_reservations_admit_exactly_one",
  ]) {
    assert.match(transient, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
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
    /TransientEffectDisposition::Published[\s\S]*?TransientEffectDisposition::Indeterminate[\s\S]*?Some\(identity\)[\s\S]*?Some\(retained\)[\s\S]*?validate_terminal_publication/,
  );
  const terminalProof = functionBlock(transient, "validate_terminal_publication");
  assert.match(
    terminalProof,
    /transient_file_evidence\(retained\)[\s\S]*?\(identity,\s*1\)[\s\S]*?file_binding_state[\s\S]*?BindingState::Exact[\s\S]*?validate_portable_destination_with_operation/,
  );
  assert.match(terminalProof, /validate\(\)\?;[\s\S]*?sync_directory[\s\S]*?validate\(\)/);
  assert.doesNotMatch(terminalProof, /open_file|exact_file_link_count|try_clone/);
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
  assert.match(platform, /enum VisitCompletion\s*\{[\s\S]*?Complete[\s\S]*?Stopped[\s\S]*?LimitExceeded/);
  assert.match(platform, /fn visit_entries<F>[\s\S]*?ControlFlow/);
  assert.doesNotMatch(platform, /Vec::with_capacity\(limit\)/);
  const entries = functionBlock(platform, "entries");
  assert.match(entries, /visit_entries/);
  assert.match(entries, /ControlFlow::Continue/);
  assert.match(entries, /VisitCompletion::Complete/);
});

test("transient admission batches reservation and one fresh inventory", async () => {
  const [library, transient] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/transient.rs"),
  ]);
  const reserve = functionBlock(transient, "reserve_batch");
  assert.match(reserve, /operations\.lock\(\)/);
  assert.match(reserve, /io::ErrorKind::WouldBlock/);
  assert.match(reserve, /reserve_effects\(plan\.names\.len\(\)\)/);
  assert.equal(
    [...reserve.matchAll(/try_reserve_exact\(plan\.names\.len\(\)\)/g)].length,
    2,
  );
  assert.match(reserve, /for \(offset, record\) in records\.into_iter\(\)\.enumerate\(\)/);
  assert.match(reserve, /state\s*\.transients\s*\.insert\(/);
  const mutation = reserve.indexOf("state.next_transient_id = next_id");
  assert.ok(mutation > reserve.indexOf("transient_destination_is_reserved"));
  assert.ok(mutation > reserve.indexOf("state.transients.contains_key"));
  assert.ok(mutation > reserve.indexOf(".try_reserve(plan.names.len())"));
  assert.ok(mutation > reserve.indexOf("state.reserve_effects(plan.names.len())"));
  assert.ok(reserve.indexOf("state.transients.insert") > mutation);
  assert.doesNotMatch(transient, /fn reserve\s*\([\s\S]*?TransientEffectToken/);

  const batchAdmission = functionBlock(transient, "admit_transient_destinations");
  assert.match(batchAdmission, /DestinationBatchPlan::new/);
  assert.ok(
    batchAdmission.indexOf("try_reserve_exact(plan.names.len())") <
      batchAdmission.indexOf("TransientEffectToken::reserve_batch"),
  );
  assert.match(batchAdmission, /TransientEffectToken::reserve_batch/);
  assert.match(batchAdmission, /validate_destination_batch_with_operation/);
  assert.equal(
    [...batchAdmission.matchAll(/validate_destination_batch_with_operation/g)].length,
    1,
  );
  assert.match(batchAdmission, /TransientEffectToken::settle_no_effect_batch/);
  const settleBatch = functionBlock(transient, "settle_no_effect_batch");
  assert.equal([...settleBatch.matchAll(/operations\.lock\(\)/g)].length, 1);
  assert.match(settleBatch, /!token\.armed/);
  assert.match(settleBatch, /tokens\[\.\.index\]/);
  assert.match(settleBatch, /record\.phase\s*!=\s*TransientEffectPhase::Reserved/);
  assert.match(settleBatch, /record\.disposition\s*!=\s*TransientEffectDisposition::Reserved/);
  assert.match(settleBatch, /record\.retained\.is_some\(\)/);
  assert.match(settleBatch, /checked_sub\(tokens\.len\(\)\)/);
  const remove = settleBatch.indexOf("state.transients.remove");
  const total = settleBatch.indexOf("state.outstanding_effects = outstanding_effects");
  const disarm = settleBatch.indexOf("token.armed = false");
  assert.ok(remove >= 0 && remove < total && total < disarm);
  assert.doesNotMatch(settleBatch, /mark_disposition|settle_with/);
  const singleton = functionBlock(transient, "admit_transient_destination");
  assert.match(singleton, /admit_transient_destinations\(vec!\[name\]\)/);

  const cancel = functionBlock(transient, "cancel");
  assert.match(cancel, /mark_disposition\(TransientEffectDisposition::NoEffect\)/);
  assert.match(cancel, /settle_with\(&operation\)/);
  assert.match(cancel, /TransientDestinationCancelOutcome::Cancelled/);
  assert.match(transient, /struct TransientDestinationCancelObligation[\s\S]*?destination:\s*Option<TransientDestination>/);

  const restore = functionBlock(transient, "restore_discarded_destination");
  assert.match(restore, /mark_disposition_on_drop\(TransientEffectDisposition::NoEffect\)/);
  assert.match(restore, /reset_reserved\(\)/);
  assert.match(restore, /TransientDiscardOutcome::Discarded\(destination\)/);
  const reset = functionBlock(transient, "reset_reserved");
  assert.match(reset, /record\.identity\s*=\s*None/);
  assert.match(reset, /record\.phase\s*=\s*TransientEffectPhase::Reserved/);
  assert.match(reset, /record\.disposition\s*=\s*TransientEffectDisposition::Reserved/);
  const abandon = functionBlock(transient, "abandon_transient_effect");
  assert.match(abandon, /TransientEffectPhase::Reserved[\s\S]*?TransientEffectDisposition::Reserved[\s\S]*?TransientEffectDisposition::NoEffect[\s\S]*?TransientEffectPhase::Abandoned/);

  const inventory = functionBlock(
    transient,
    "validate_mixed_destination_batch_with_operation",
  );
  assert.equal(
    [...inventory.matchAll(/validate_publication_directory/g)].length,
    2,
  );
  assert.equal(
    [...inventory.matchAll(/directory_revision_for_publication/g)].length,
    2,
  );
  assert.match(inventory, /platform::visit_entries/);
  assert.match(inventory, /platform::visit_entries_preallocated/);
  assert.match(inventory, /fill_leaf_name_equivalence_keys/);
  assert.match(inventory, /VisitCompletion::Complete/);
  assert.match(inventory, /VisitCompletion::Stopped/);
  assert.match(inventory, /VisitCompletion::LimitExceeded/);
  assert.match(inventory, /exact\.fill\(false\)/);
  assert.doesNotMatch(inventory, /platform::entries|static|cache|Vec::/i);

  assert.match(library, /fn reserve_effects\(&mut self, count: usize\)/);
  for (const testName of [
    "batch_aliases_are_rejected_before_effect_reservation",
    "external_batch_collision_settles_every_reservation",
    "held_destination_blocks_batch_until_explicit_cancellation",
    "batch_admission_reserves_every_destination_atomically",
    "explicit_destination_cancellation_releases_its_reservation",
    "discarded_stage_reuses_the_exact_destination_reservation",
    "reserved_token_unwind_is_root_cleanable_no_effect",
  ]) {
    assert.match(transient, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
  const discardRetry = functionBlock(
    transient,
    "discarded_stage_reuses_the_exact_destination_reservation",
  );
  const firstDiscard = discardRetry.indexOf("first.discard()");
  const heldConflict = discardRetry.indexOf("admit_transient_destinations");
  const secondCreate = discardRetry.indexOf("destination.create_stage()");
  assert.ok(firstDiscard >= 0 && firstDiscard < heldConflict && heldConflict < secondCreate);
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
  assert.match(platform, /struct TransientFile\s*\{[\s\S]*?proc_path:\s*PathBuf/);
  assert.match(
    functionBlock(platform, "link_transient_file"),
    /retained_file_identity_preallocated[\s\S]*?&transient\.proc_path/,
  );
  assert.match(platform, /rfs::linkat\([\s\S]*?AtFlags::SYMLINK_FOLLOW/);
  assert.doesNotMatch(platform, /AtFlags::EMPTY_PATH|rollback_transient_publication/);
  assert.match(platform, /enum TransientPublicationState[\s\S]*?Unpublished[\s\S]*?Published[\s\S]*?Indeterminate/);
  assert.match(platform, /discard_transient_file[\s\S]*?retained_file_identity[\s\S]*?external link/);
  assert.match(platform, /fn transient_file_evidence[\s\S]*?retained_file_identity/);
  assert.match(platform, /fn into_published_file[\s\S]*?transient\.file/);
  assert.doesNotMatch(platform, /FinishTransientPublicationError|finish_transient_publication/);
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

test("monotonic transient batches classify and transfer exact authority", async () => {
  const [library, transient, platform] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("core/fs/src/transient.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  assert.match(
    transient,
    /struct TransientEffectToken\s*\{[\s\S]*?authority:\s*Arc<CapabilityAuthority>/,
  );
  assert.doesNotMatch(
    transient.slice(
      transient.indexOf("struct TransientEffectToken"),
      transient.indexOf("fn transient_destination_is_reserved"),
    ),
    /Weak<CapabilityAuthority>|\.upgrade\(\)/,
  );
  const transfer = functionBlock(transient, "abandon_with_retained");
  assert.match(transfer, /unwrap_or_else\(\|poisoned\| poisoned\.into_inner\(\)\)/);
  assert.match(transfer, /assert!\([\s\S]*?self\.armed/);
  assert.match(transfer, /assert!\([\s\S]*?record\.retained\.is_none\(\)/);
  assert.match(transfer, /record\.retained\s*=\s*Some\(retained\)/);
  assert.doesNotMatch(transfer, /record\.retained\.replace/);
  assert.match(transfer, /TransientEffectPhase::Abandoned/);
  assert.match(
    functionBlock(transient, "settle_transient_effect"),
    /record\.retained\.is_none\(\)/,
  );
  assert.match(library, /TransientPublicationBatchOutcome, TransientPublicationMember/);
  assert.doesNotMatch(
    `${library}\n${transient}`,
    /TransientPublicationOutcome|TransientPublicationObligation|pub fn publish_create_new[\s\S]*?impl TransientStageSealed/,
  );
  const sealedImpl = transient.slice(
    transient.indexOf("impl TransientStageSealed"),
    transient.indexOf("impl Read for TransientStageSealed"),
  );
  assert.doesNotMatch(sealedImpl, /publish_create_new/);
  assert.doesNotMatch(
    transient,
    /impl\s+(?:Clone|Copy)\s+for\s+TransientPublicationBatch/,
  );
  assert.match(
    transient,
    /bounded monotonic publication group[\s\S]*?not an atomic visibility transaction[\s\S]*?private capability-[\s\S]*?owned directory or generation/,
  );
  assert.match(
    transient,
    /enum TransientPublicationBatchOutcome[\s\S]*?Published\(Vec<FileCapability>\)[\s\S]*?NoEffect[\s\S]*?Partial[\s\S]*?members:\s*Vec<TransientPublicationMember>[\s\S]*?Pending/,
  );
  assert.match(
    transient,
    /enum TransientPublicationMember[\s\S]*?Published\(FileCapability\)[\s\S]*?Unpublished\(TransientStageSealed\)/,
  );
  const productionBatchCallers = [];
  for (const [path, source] of await readRustTree("apps", "core")) {
    const testModule = source.indexOf("#[cfg(test)]\nmod tests");
    const production = testModule === -1 ? source : source.slice(0, testModule);
    const calls = production.match(/TransientPublicationBatch::new\s*\(/g)?.length ?? 0;
    if (calls > 0) productionBatchCallers.push([path, calls]);
  }
  assert.deepEqual(productionBatchCallers, [
    ["core/minecraft/src/download/transient_transfer.rs", 1],
  ]);
  const managedTransfer = await read("core/minecraft/src/download/transient_transfer.rs");
  const managedProduction = managedTransfer.slice(
    0,
    managedTransfer.indexOf("#[cfg(test)]\nmod tests"),
  );
  assert.match(
    managedProduction,
    /TransientPublicationBatch::new\s*\(\s*vec!\[\s*sealed\s*\]\s*\)/,
  );
  const batchImpl = transient.slice(
    transient.indexOf("impl TransientPublicationBatch {"),
    transient.indexOf("fn classify_publication_batch"),
  );
  const createBatch = functionBlock(batchImpl, "new");
  assert.match(createBatch, /stages\.is_empty\(\)/);
  assert.match(createBatch, /MAX_OUTSTANDING_EFFECTS/);
  assert.match(createBatch, /Weak::ptr_eq/);
  assert.match(createBatch, /directory\.inner\.identity/);
  assert.match(createBatch, /DestinationBatchPlan::new/);
  assert.match(createBatch, /stages:\s*Some\(stages\)/);
  assert.equal(
    [...createBatch.matchAll(/try_reserve_exact\(stages\.len\(\)\)/g)].length,
    5,
  );
  assert.equal(
    [...createBatch.matchAll(/try_reserve_exact\(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES\)/g)].length,
    3,
  );
  assert.match(createBatch, /TRANSIENT_DIRECTORY_BUFFER_BYTES/);
  assert.match(createBatch, /inventory_exact\.resize\(stages\.len\(\), false\)/);
  assert.doesNotMatch(createBatch, /replacement_director/);
  assert.match(
    transient,
    /struct TransientStage\s*\{[\s\S]*?destination:\s*Option<TransientDestination>/,
  );
  const discard = functionBlock(transient, "discard");
  assert.match(discard, /self\.take_destination\(\)/);
  assert.doesNotMatch(discard, /\.clone\(\)|std::mem::replace|replacement/);
  assert.match(
    transient,
    /struct TransientDestinationToken[\s\S]*?token:\s*Option<TransientEffectToken>[\s\S]*?impl Drop for TransientDestinationToken[\s\S]*?mark_disposition_on_drop\(TransientEffectDisposition::NoEffect\)/,
  );
  assert.doesNotMatch(transient, /impl Drop for TransientDestination\s*\{/);
  const transitionDrop = transient.slice(
    transient.indexOf("impl Drop for TransientPublicationTransition"),
    transient.indexOf("enum ClassifiedPublicationMember"),
  );
  assert.match(transitionDrop, /retained[\s\S]*?\.take\(\)/);
  assert.match(
    transitionDrop,
    /stage\.stage\.file\.is_none\(\)[\s\S]*?stage\.stage\.file\s*=\s*self\.retained\.take\(\)/,
  );
  assert.match(
    transitionDrop,
    /stage\.stage\.token\.is_none\(\)[\s\S]*?stage\.stage\.token\s*=\s*self\.token\.take\(\)/,
  );
  assert.doesNotMatch(transient, /impl Drop for TransientPublicationBatchObligation/);
  const stageDrop = transient.slice(
    transient.indexOf("impl Drop for TransientStage"),
    transient.indexOf("pub struct TransientStageSealFailure"),
  );
  assert.match(stageDrop, /transient_publication_state_for_publication/);
  assert.match(stageDrop, /discard_transient_file_preallocated/);
  assert.match(stageDrop, /DiscardTransientFileError::Retained[\s\S]*?abandon_with_retained/);
  assert.doesNotMatch(stageDrop, /io::Error::(?:new|other)|Vec::|try_reserve|format!/);
  const publish = functionBlock(transient, "publish_create_new");
  assert.match(publish, /validate_destination_batch_with_operation/);
  assert.match(publish, /DestinationCollisionPolicy::RequireVacant/);
  assert.ok(
    publish.indexOf("validate_destination_batch_with_operation") <
      publish.indexOf("platform::link_transient_file"),
  );
  assert.match(
    publish,
    /Err\(error\),\s*Ok\(platform::TransientPublicationState::Unpublished\)[\s\S]*?if index == 0[\s\S]*?NoEffect/,
  );
  assert.match(
    publish,
    /Err\(error\),\s*_\)[\s\S]*?classify_publication_batch/,
  );
  const firstLink = publish.indexOf("platform::link_transient_file");
  assert.ok(firstLink >= 0);
  assert.doesNotMatch(
    publish.slice(firstLink),
    /Vec::|try_reserve|\.clone\(\)|to_owned|to_vec|format!|leaf_name_equivalence_keys|io::Error::(?:new|other)/,
  );
  const classify = functionBlock(transient, "classify_publication_batch");
  assert.match(classify, /transient_publication_state_for_publication/);
  assert.equal(
    [...classify.matchAll(/validate_classified_publication_members/g)].length,
    4,
  );
  assert.match(
    classify,
    /validate_classified_publication_members[\s\S]*?sync_directory[\s\S]*?validate_mixed_destination_batch_with_operation[\s\S]*?validate_classified_publication_members[\s\S]*?published_count[\s\S]*?settle_classified_batch/,
  );
  assert.match(
    classify,
    /published_count\s*==\s*0[\s\S]*?TransientPublicationBatchOutcome::NoEffect\s*\{\s*error,\s*batch\s*\}/,
  );
  assert.match(
    classify,
    /published_count\s*==\s*batch\.classifications\.len\(\)[\s\S]*?TransientPublicationBatchOutcome::Published[\s\S]*?TransientPublicationBatchOutcome::Partial/,
  );
  assert.doesNotMatch(
    classify,
    /Vec::|try_reserve|\.clone\(\)|to_owned|to_vec|format!|leaf_name_equivalence_keys|io::Error::(?:new|other)/,
  );
  const mixedInventory = functionBlock(
    transient,
    "validate_mixed_destination_batch_with_operation",
  );
  assert.match(mixedInventory, /fill_leaf_name_equivalence_keys/);
  assert.match(
    mixedInventory,
    /DestinationCollisionPolicy::RequireVacant[\s\S]*?AlreadyExists/,
  );
  assert.match(
    mixedInventory,
    /DestinationCollisionPolicy::AllowExternalCollision[\s\S]*?!expected_exact\[target\]/,
  );
  assert.match(mixedInventory, /plan\.targets\.get\(portable_key\.as_slice\(\)\)/);
  assert.match(mixedInventory, /plan\.targets\.get\(native_key\.as_slice\(\)\)/);
  assert.match(
    mixedInventory,
    /!expected_exact\[portable_target\][\s\S]*?!expected_exact\[native_target\][\s\S]*?ControlFlow::Continue/,
  );
  assert.match(mixedInventory, /!expected_exact\[target\][\s\S]*?ControlFlow::Continue/);
  assert.doesNotMatch(
    mixedInventory,
    /(?<!fill_)leaf_name_equivalence_keys\(|Vec::|try_reserve|\.clone\(\)|String|collect/,
  );
  const keyFill = functionBlock(platform, "fill_leaf_name_equivalence_keys");
  assert.match(
    keyFill,
    /portable\.clear\(\)[\s\S]*?native\.clear\(\)[\s\S]*?normalization\.clear\(\)/,
  );
  assert.match(keyFill, /case_fold\(\)/);
  assert.match(keyFill, /decompose_canonical/);
  assert.match(keyFill, /canonical_combining_class/);
  assert.match(keyFill, /normalization\.swap/);
  assert.match(keyFill, /extend_preallocated_key/);
  assert.doesNotMatch(keyFill, /\.nfc\(\)|collect|String|Vec::|\.clone\(\)/);
  const unpublishedProof = functionBlock(transient, "validate_unpublished_destination");
  assert.match(unpublishedProof, /transient_file_evidence_for_publication\(retained\)\?[\s\S]*?\(identity,\s*0\)/);
  assert.match(unpublishedProof, /binding\s*==\s*platform::BindingState::Exact/);
  assert.doesNotMatch(unpublishedProof, /binding\s*!=\s*platform::BindingState::Absent/);
  const settleTokens = functionBlock(transient, "settle_classified_batch");
  assert.equal([...settleTokens.matchAll(/operations\s*\.lock\(\)/g)].length, 1);
  assert.match(settleTokens, /record\.phase\s*!=\s*TransientEffectPhase::Live/);
  assert.match(settleTokens, /record\.disposition\s*!=\s*TransientEffectDisposition::Staged/);
  assert.match(settleTokens, /filter\(\|member\| member\.is_published\(\)\)\.count\(\)/);
  assert.match(settleTokens, /checked_sub\(published\)/);
  assert.match(
    settleTokens,
    /if member\.is_published\(\)[\s\S]*?state\.transients\.remove/,
  );
  assert.match(
    settleTokens,
    /state\s*\.transients\s*\.remove[\s\S]*?member\.token_mut\(\)\.armed\s*=\s*false/,
  );
  assert.match(
    settleTokens,
    /if member\.is_published\(\)[\s\S]*?member\.token_mut\(\)\.armed\s*=\s*false/,
  );
  assert.doesNotMatch(settleTokens, /stale_capability|io::Error::(?:new|other)/);

  const rawVisit = functionBlock(platform, "visit_entries_preallocated");
  assert.match(rawVisit, /rfs::openat/);
  assert.match(rawVisit, /rfs::RawDir::new\(handle, buffer\)/);
  assert.doesNotMatch(rawVisit, /Dir::read_from|Vec::|try_reserve|io::Error::(?:new|other)/);
  const classifiedProof = functionBlock(
    transient,
    "validate_classified_publication_members",
  );
  assert.match(classifiedProof, /directory_buffer\.as_deref_mut\(\)/);
  const reconcileEntry = functionBlock(
    transient,
    "enter_publication_reconciliation",
  );
  assert.match(reconcileEntry, /validate_lease_preallocated/);
  assert.match(reconcileEntry, /validate_root_preallocated/);
  assert.match(reconcileEntry, /validate_publication_directory/);
  assert.doesNotMatch(reconcileEntry, /enter_transient_operation|stale_capability|io::Error::(?:new|other)/);
  for (const functionName of [
    "validate_publication_directory",
    "directory_revision_for_publication",
    "transient_publication_state_for_publication",
    "transient_file_evidence_for_publication",
    "validate_exact_destination_binding",
    "validate_unpublished_destination",
    "validate_classified_publication_members",
  ]) {
    assert.doesNotMatch(
      functionBlock(transient, functionName),
      /Vec::|try_reserve|\.clone\(\)|to_owned|to_vec|format!|io::Error::(?:new|other)/,
      `${functionName} may run after the first Linux publication`,
    );
  }
  for (const functionName of [
    "directory_identity_preallocated",
    "directory_revision_preallocated",
    "validate_absolute_directory_guard_preallocated",
    "validate_root_preallocated",
    "validate_lease_preallocated",
    "transient_publication_state_preallocated",
    "transient_file_evidence_preallocated",
    "discard_transient_file_preallocated",
    "retained_file_identity_preallocated",
    "exact_directory_binding_state_preallocated",
  ]) {
    assert.doesNotMatch(
      functionBlock(platform, functionName),
      /Dir::read_from|Vec::|try_reserve|format!|io::Error::(?:new|other)/,
      `${functionName} must retain the Linux post-publication allocation bound`,
    );
  }
  assert.doesNotMatch(
    transient,
    /TransientPublicationRollback|TransientPublicationBatchState|RolledBack|rollback_publication|remove_exact_transient_publication|settle_publication_batch|settle_published_batch/,
  );
  const transitionImpl = transient.slice(
    transient.indexOf("impl TransientPublicationTransition"),
    transient.indexOf("impl Drop for TransientPublicationTransition"),
  );
  assert.match(
    functionBlock(transitionImpl, "into_file_capability"),
    /take_destination[\s\S]*?TransientDestination[\s\S]*?platform::into_published_file/,
  );
  assert.doesNotMatch(
    transitionImpl,
    /replacement_|output_directory|output_name|\.clone\(\)|try_reserve|Vec::/,
  );
  const extractionUnwind = functionBlock(
    transient,
    "publication_transition_unwind_after_carrier_extraction_retains_root_authority",
  );
  assert.match(extractionUnwind, /catch_unwind/);
  assert.match(extractionUnwind, /assert_retained_transient/);
  assert.match(
    extractionUnwind,
    /session\.revoke\(\)[\s\S]*?RootRevokeOutcome::Revoked/,
  );
  const partialLinkUnwind = functionBlock(
    transient,
    "publication_batch_unwind_after_partial_link_retains_root_authority",
  );
  assert.match(partialLinkUnwind, /catch_unwind/);
  assert.match(
    partialLinkUnwind,
    /assert_retained_transient[\s\S]*?TransientEffectDisposition::Published/,
  );
  assert.match(
    partialLinkUnwind,
    /session\.revoke\(\)[\s\S]*?RootRevokeOutcome::Revoked/,
  );
  assert.match(
    platform,
    /cfg\(all\(test,\s*target_os = "linux"\)\)[\s\S]*?fn exact_file_link_count/,
  );
  assert.doesNotMatch(
    platform.slice(platform.indexOf("#[cfg(windows)]")),
    /fn exact_file_link_count/,
  );
  for (const testName of [
    "grouped_publication_releases_every_file_after_one_terminal_outcome",
    "grouped_partial_publication_preserves_original_member_order",
    "grouped_partial_accepts_stable_unpublished_alias_and_wrong_kind_collisions",
    "grouped_zero_publication_returns_the_intact_no_effect_batch",
    "dropped_mixed_pending_batch_retains_root_cleanable_authority",
  ]) {
    assert.match(transient, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
});

test("sealed transient stages expose one bounded positional reader", async () => {
  const [transient, platform] = await Promise.all([
    read("core/fs/src/transient.rs"),
    read("core/fs/src/platform.rs"),
  ]);
  assert.match(
    transient,
    /struct TransientStageSealed\s*\{[\s\S]*?stage:\s*TransientStage,[\s\S]*?read_position:\s*u64/,
  );
  assert.match(
    functionBlock(transient, "seal"),
    /TransientStageSealed\s*\{[\s\S]*?read_position:\s*0/,
  );
  const readImpl = transient.slice(
    transient.indexOf("impl Read for TransientStageSealed"),
    transient.indexOf("impl Seek for TransientStageSealed"),
  );
  assert.match(readImpl, /checked_sub\(self\.read_position\)/);
  assert.match(readImpl, /u64::try_from\(bytes\.len\(\)\)/);
  assert.match(readImpl, /remaining\.min\(requested\)/);
  assert.match(readImpl, /platform::read_transient_at/);
  assert.match(readImpl, /checked_add\(u64::try_from\(read\)/);
  assert.doesNotMatch(readImpl, /\bas u64\b/);
  const seekImpl = transient.slice(
    transient.indexOf("impl Seek for TransientStageSealed"),
    transient.indexOf("fn validate_linked_publication"),
  );
  assert.match(seekImpl, /SeekFrom::Start/);
  assert.match(seekImpl, /SeekFrom::End/);
  assert.match(seekImpl, /SeekFrom::Current/);
  assert.match(seekImpl, /0\.\.=i128::from\(size\)/);
  assert.match(platform, /fn read_transient_at[\s\S]*?\.read_at\(bytes, offset\)/);
  assert.doesNotMatch(
    `${readImpl}\n${seekImpl}`,
    /open_file|try_clone|PathBuf|\/proc\/self\/fd|tempfile/,
  );
});
