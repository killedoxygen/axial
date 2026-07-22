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
    /struct TransientDestination[\s\S]*?token:\s*Option<TransientEffectToken>/,
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
    /enum TransientPublicationState[\s\S]*?LinkUncertain[\s\S]*?Linked[\s\S]*?Published\(TransientPublicationTransition\)/,
  );
  assert.match(
    transient,
    /struct TransientPublicationTransition[\s\S]*?retained:\s*Option<platform::TransientFile>[\s\S]*?token:\s*Option<TransientEffectToken>/,
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
  assert.match(batchAdmission, /TransientEffectToken::reserve_batch/);
  assert.match(batchAdmission, /validate_destination_batch_with_operation/);
  assert.equal(
    [...batchAdmission.matchAll(/validate_destination_batch_with_operation/g)].length,
    1,
  );
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
    "validate_destination_batch_with_operation",
  );
  assert.match(inventory, /directory\.validate\(operation\)/);
  assert.equal(
    [...inventory.matchAll(/platform::directory_revision/g)].length,
    2,
  );
  assert.match(inventory, /platform::visit_entries/);
  assert.match(inventory, /leaf_name_equivalence_keys/);
  assert.match(inventory, /VisitCompletion::Complete/);
  assert.match(inventory, /VisitCompletion::Stopped/);
  assert.match(inventory, /VisitCompletion::LimitExceeded/);
  assert.doesNotMatch(inventory, /platform::entries|static|cache/i);

  assert.match(library, /fn reserve_effects\(&mut self, count: usize\)/);
  for (const testName of [
    "batch_aliases_are_rejected_before_effect_reservation",
    "batch_admission_reserves_every_destination_atomically",
    "explicit_destination_cancellation_releases_its_reservation",
    "reserved_token_unwind_is_root_cleanable_no_effect",
  ]) {
    assert.match(transient, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
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

test("published transients retain exact native authority through settlement", async () => {
  const [transient, platform] = await Promise.all([
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
  const transitionDrop = transient.slice(
    transient.indexOf("impl Drop for TransientPublicationTransition"),
    transient.indexOf("fn pending_published"),
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
  assert.doesNotMatch(transient, /impl Drop for TransientPublicationObligation/);
  const stageDrop = transient.slice(
    transient.indexOf("impl Drop for TransientStage"),
    transient.indexOf("pub struct TransientStageSealFailure"),
  );
  assert.match(stageDrop, /DiscardTransientFileError::Retained[\s\S]*?abandon_with_retained/);
  const linked = functionBlock(transient, "settle_linked_stage");
  assert.match(linked, /TransientPublicationTransition::from_linked/);
  assert.ok(
    linked.indexOf("transition.settle") < linked.indexOf("transition.into_file_capability"),
    "the native wrapper may only convert after effect settlement",
  );
  assert.doesNotMatch(linked, /\.file\.take\(\)|\.token\.take\(\)/);
  const transitionImpl = transient.slice(
    transient.indexOf("impl TransientPublicationTransition"),
    transient.indexOf("impl Drop for TransientPublicationTransition"),
  );
  const transitionSettlement = functionBlock(transitionImpl, "settle");
  assert.match(
    transitionSettlement,
    /validate_exact_destination[\s\S]*?sync_directory[\s\S]*?validate_exact_destination[\s\S]*?classify_published[\s\S]*?settle_with/,
  );
  const publicationImpl = transient.slice(
    transient.indexOf("impl TransientPublicationObligation"),
    transient.indexOf("pub enum TransientDiscardOutcome"),
  );
  const reconcile = functionBlock(publicationImpl, "reconcile");
  assert.match(reconcile, /Published\(mut transition\)[\s\S]*?transition\.settle/);
  assert.doesNotMatch(reconcile, /retained\.take\(\)|token\.settle_with/);
  assert.doesNotMatch(reconcile, /platform::open_file|exact_file_link_count|try_clone/);
  assert.match(
    functionBlock(transitionImpl, "into_file_capability"),
    /platform::into_published_file/,
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
  const classificationUnwind = functionBlock(
    transient,
    "publication_transition_unwind_after_classification_retains_root_authority",
  );
  assert.match(classificationUnwind, /catch_unwind/);
  assert.match(
    classificationUnwind,
    /assert_retained_transient[\s\S]*?TransientEffectDisposition::Published/,
  );
  assert.match(
    classificationUnwind,
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
    transient.indexOf("fn settle_linked_stage"),
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
