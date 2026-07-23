import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const read = (path) => readFile(new URL(path, repository), "utf8");

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

function ordered(source, markers) {
  let previous = -1;
  for (const marker of markers) {
    const index = source.indexOf(marker, previous + 1);
    assert.notEqual(index, -1, `missing ordered marker: ${marker}`);
    assert.ok(index > previous, `marker is out of order: ${marker}`);
    previous = index;
  }
}

test("ordinary content execution enters through one exact State capability", async () => {
  const operation = await read("apps/api/src/application/content/operation.rs");
  const activation = braceBlock(
    operation,
    "async fn activate_content_mutation",
  );
  ordered(activation, [
    "try_acquire_instance_lifecycle",
    "admit_instance_content_mutation",
    "admission.activate()",
    "activated.into_parts()",
  ]);
  assert.doesNotMatch(
    operation,
    /ContentManifest::load|\.game_dir\(|sessions\(\)\.has_active_instance|admit_managed_artifact_mutation/,
  );
  for (const marker of [
    "decode_observed_content_manifest",
    "derive_live_managed_content",
    "plan_managed_content_install",
    "plan_managed_content_uninstall",
    "projection.effect_paths()",
    ".seal(&session)",
  ]) {
    assert.match(
      operation,
      new RegExp(marker.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")),
    );
  }
  const install = braceBlock(operation, "async fn execute_content_install");
  ordered(install, [
    "let resolution = tokio::select!",
    "() = cancellation.cancelled()",
    "resolve_for_execution(",
  ]);
});

test("ordinary transfers are target-first pinned sequential and fully bound", async () => {
  const operation = await read("apps/api/src/application/content/operation.rs");
  const transfers = braceBlock(operation, "async fn execute_transfers");
  ordered(transfers, [
    "loop",
    "transfers.next()",
    "sources.remove(issued.id())",
    "clients.client_for(&url",
    "transfer_cancellation_channel()",
    "issued.start(",
    "let joined = transfer.join()",
    "tokio::select!",
    "record_transfer_settlement(&settlement",
    "settlement.advance()",
    "ManagedContentTransferAdvance::Continue(next)",
    "ManagedContentTransferStep::Complete(complete)",
    "complete.stage()",
  ]);
  assert.match(transfers, /issued\.cancel\(\)/);
  assert.match(transfers, /transfers\.cancel\(\)/);
  assert.match(transfers, /complete\.cancel\(\)/);
  assert.doesNotMatch(
    transfers,
    /start_create_only_transfer|bind_outcome|accept_transfers|cancel_transfers/,
  );
  const clients = braceBlock(operation, "impl ContentTransferClientCache");
  ordered(clients, [
    "TransferOrigin::from_url",
    "self.clients.get(&origin)",
    "pinned_transfer_client(origin.clone()",
    "self.clients.len() < MAX_CONTENT_TRANSFER_CLIENTS",
    "self.clients.insert(origin",
  ]);
  const pinning = braceBlock(operation, "async fn pinned_transfer_client");
  ordered(pinning, [
    "lookup_host",
    "bounded_unique_addresses(resolved)",
    "PinnedTransferOrigin::public",
    "TransferClientConfig::bounded_pinned_public",
  ]);
  const addresses = braceBlock(operation, "fn bounded_unique_addresses");
  ordered(addresses, [
    "seen.insert(address)",
    "unique.len() == MAX_CONTENT_PINNED_ADDRESSES",
    "unique.push(address)",
  ]);
  assert.match(operation, /fn dns_addresses_are_capped_at_the_pinning_bound/);
});

test("content retry and recovery policies stay bounded and distinct", async () => {
  const operation = await read("apps/api/src/application/content/operation.rs");
  assert.match(
    operation,
    /CONTENT_RETRY_DELAYS:[^=]*=\s*\[[\s\S]*?500[\s\S]*?1_500[\s\S]*?4_000/,
  );
  const retry = braceBlock(operation, "fn content_transfer_retryable");
  assert.match(retry, /Network/);
  assert.match(retry, /408 \| 429 \| 500\.\.=599/);
  assert.doesNotMatch(retry, /425/);
  const recovery = braceBlock(operation, "async fn settle_transaction(\n");
  assert.match(recovery, /CONTENT_RECOVERY_RETRY_DELAYS\[retry_index\]/);
  assert.match(recovery, /recovery\.reconcile\(\)/);
  assert.doesNotMatch(recovery, /cancellation\.cancelled|is_cancelled/);
});

test("content operation and progress workers are cancelled and joined in order", async () => {
  const [operation, install] = await Promise.all([
    read("apps/api/src/application/content/operation.rs"),
    read("apps/api/src/application/install.rs"),
  ]);
  const task = braceBlock(operation, "pub(crate) struct ContentOperationTask");
  assert.match(
    task,
    /cancellation:\s*Option<ContentOperationCancellationSender>/,
  );
  assert.match(
    task,
    /task:\s*Option<JoinHandle<Result<\(\), ContentExecutionError>>>/,
  );
  assert.doesNotMatch(operation, /impl\s+Clone\s+for\s+ContentOperationTask/);
  assert.doesNotMatch(
    operation,
    /derive\([^)]*Clone[^)]*\)[\s\S]{0,80}ContentOperationTask/,
  );
  const join = braceBlock(operation, "async fn join_inner");
  ordered(join, [".task", ".as_mut()", ".await", "self.task = None"]);
  assert.doesNotMatch(join, /\.take\(\)/);
  const cancellation = braceBlock(
    operation,
    "impl ContentOperationCancellationSender",
  );
  assert.match(cancellation, /notify_one\(\)/);
  assert.doesNotMatch(cancellation, /notify_waiters\(\)/);
  assert.match(
    operation,
    /async fn cancellation_requested_before_wait_is_observed/,
  );
  const owned = braceBlock(operation, "fn spawn_owned");
  assert.match(owned, /cancellation:\s*None/);
  const modpack = braceBlock(
    operation,
    "pub(crate) fn start_modpack_install_task",
  );
  assert.match(
    modpack,
    /ContentOperationTask::spawn_owned\(producer, async move/,
  );
  assert.doesNotMatch(modpack, /ContentOperationCancellation/);
  ordered(install, [
    "let cancellation = operation.cancellation_sender()",
    "let joined = operation.join()",
    "() = journal_failed.notified()",
    "if let Some(cancellation) = cancellation",
    "cancellation.cancel()",
    "let _ = joined.await",
    "drop(progress_tx)",
    "finish_install_progress_task(progress_task).await",
  ]);
  assert.doesNotMatch(
    install,
    /drop\(content_operation\)|Box::pin\(content_operation\)/,
  );
  assert.doesNotMatch(
    install,
    /content::execute_content_install|content::execute_content_uninstalls/,
  );
});

test("query liveness is explicit while execution remains path-free", async () => {
  const [content, pack, target] = await Promise.all([
    read("apps/api/src/application/content/mod.rs"),
    read("apps/api/src/application/content/pack.rs"),
    read("apps/api/src/application/content/target.rs"),
  ]);
  const snapshot = braceBlock(
    content,
    "pub(super) async fn load_ambient_content_snapshot",
  );
  assert.match(snapshot, /run_blocking_filesystem/);
  const liveness = braceBlock(content, "fn project_ambient_live_content");
  assert.match(liveness, /LiveManagedContent::from_entries/);
  ordered(liveness, [
    "ids.contains(entry.canonical_id())",
    "entry_file_present(game_dir, entry)",
  ]);
  assert.match(
    content,
    /load_ambient_content_snapshot\(game_dir,\s*Some\(candidate_ids\)\)\.await/,
  );
  for (const marker of [
    "pub async fn content_plan",
    "pub async fn instance_content",
    "pub async fn instance_content_updates",
  ]) {
    const query = braceBlock(content, marker);
    assert.match(query, /load_ambient_content_snapshot\(/);
    assert.doesNotMatch(query, /ContentManifest::load|entry_file_present/);
  }
  const cherryPick = braceBlock(
    pack,
    "async fn validate_cherry_pick_dependencies",
  );
  assert.match(cherryPick, /load_ambient_content_snapshot\(/);
  assert.doesNotMatch(cherryPick, /ContentManifest::load|entry_file_present/);
  const resolveTarget = braceBlock(target, "pub struct ResolveTarget");
  assert.match(resolveTarget, /resolution:\s*ResolutionTarget/);
  assert.match(resolveTarget, /game_dir:\s*Option<PathBuf>/);
  assert.doesNotMatch(
    target,
    /impl\s+(?:std::ops::)?Deref\s+for\s+ResolveTarget/,
  );
  assert.match(content, /target\.resolution\(\)/);
});
