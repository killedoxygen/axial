import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

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

function functionBlock(source, name) {
  const match = new RegExp(`(?:async\\s+)?fn\\s+${name}(?:<[^>]+>)?\\s*\\(`).exec(source);
  assert.ok(match, `missing function ${name}`);
  return braceBlock(source, match[0]);
}

function productionSource(source) {
  const marker = "#[cfg(test)]\nmod tests";
  const end = source.indexOf(marker);
  assert.notEqual(end, -1, "missing Rust test module marker");
  return source.slice(0, end);
}

test("managed transfer exports distinct move-only outcomes without path APIs", async () => {
  const [moduleSource, downloadModule] = await Promise.all([
    read("core/minecraft/src/download/transient_transfer.rs"),
    read("core/minecraft/src/download/mod.rs"),
  ]);
  const production = productionSource(moduleSource);
  assert.match(downloadModule, /mod transient_transfer;/);
  assert.equal(downloadModule.match(/mod transient_transfer;/g)?.length, 1);
  for (const type of [
    "TransferByteContract",
    "TransferContract",
    "ExpectedTransferDigests",
    "TransferClientConfig",
    "TransferClientConfigError",
    "TransferOrigin",
    "TransferOriginError",
    "PinnedTransferOrigin",
    "PinnedTransferOriginError",
    "TransferTimeoutKind",
    "ManagedTransferAuthority",
    "CreateOnlyTransferTarget",
    "SourceOnlyTransferTarget",
    "VerifiedCreateOnly",
    "VerifiedSource",
    "TransferOutcome",
    "TransferCleanupObligation",
    "TransferTask",
    "TransferTargetCancelOutcome",
    "TransferTargetCancelObligation",
  ]) {
    assert.match(downloadModule, new RegExp(`\\b${type}\\b`), `missing export ${type}`);
  }
  assert.match(
    production,
    /enum TransferByteContract\s*\{[\s\S]*?Exact\(NonZeroU64\)[\s\S]*?AtMost\(NonZeroU64\)[\s\S]*?Below\(NonZeroU64\)/,
  );
  assert.doesNotMatch(production, /\b(?:Path|PathBuf)\b|std::fs|tokio::fs|tempfile|Uuid|process::id/);
  assert.doesNotMatch(production, /\bDirectory\b|\bLeafName\b/);
  assert.doesNotMatch(production, /\b(?:rename|remove_file|create_dir_all)\s*\(/);
  for (const marker of [
    "pub struct CreateOnlyTransferTarget",
    "pub struct SourceOnlyTransferTarget",
  ]) {
    const target = braceBlock(production, marker);
    assert.match(target, /destination:\s*TransientDestination/);
    assert.match(target, /authority:\s*ManagedTransferAuthority/);
    assert.doesNotMatch(target, /directory|leaf/i);
    assert.match(
      production.slice(Math.max(0, production.indexOf(marker) - 100), production.indexOf(marker)),
      /#\[must_use/,
    );
    const targetImpl = braceBlock(production, marker.replace("pub struct ", "impl "));
    assert.match(targetImpl, /pub fn cancel\(self\) -> TransferTargetCancelOutcome/);
    assert.match(targetImpl, /map_target_cancel\(self\.destination\.cancel\(\), self\.authority\)/);
  }
  const authority = braceBlock(production, "pub struct ManagedTransferAuthority");
  assert.match(authority, /_retained:\s*Arc<dyn Send \+ Sync>/);
  assert.doesNotMatch(production, /impl\s+(?:Clone|Copy)\s+for\s+ManagedTransferAuthority/);
  assert.doesNotMatch(
    production.slice(
      Math.max(0, production.indexOf("pub struct ManagedTransferAuthority") - 100),
      production.indexOf("impl fmt::Debug for ManagedTransferAuthority"),
    ),
    /#\[derive\([^\]]*(?:Clone|Copy)/,
  );
  const retainedAuthority = functionBlock(production, "retained");
  assert.doesNotMatch(retainedAuthority, /pub\s+(?:\([^)]*\)\s+)?fn/);
  assert.match(retainedAuthority, /Arc::clone\(&self\._retained\)/);
  const targetCancel = functionBlock(production, "map_target_cancel");
  assert.match(targetCancel, /TransientDestinationCancelOutcome::Cancelled/);
  assert.match(targetCancel, /TransferTargetCancelOutcome::Cancelled\(authority\)/);
  assert.match(targetCancel, /TransferTargetCancelOutcome::Pending/);
  assert.match(targetCancel, /authority,[\s\S]*?obligation:\s*Some\(obligation\)/);
  const sourceImpl = braceBlock(production, "impl VerifiedSource");
  assert.match(sourceImpl, /pub fn report\(&self\)/);
  assert.match(sourceImpl, /pub fn discard\(self\)/);
  assert.doesNotMatch(sourceImpl, /publish|inner|sealed\s*\(/);
  const createOnlyImpl = braceBlock(production, "impl VerifiedCreateOnly");
  assert.match(createOnlyImpl, /pub fn publish_create_new\(self\)/);
  assert.doesNotMatch(createOnlyImpl, /publish_create_new\(self,/);
  assert.match(createOnlyImpl, /singleton destination/);
  assert.match(createOnlyImpl, /cannot prove group atomicity/);
  assert.equal(
    production.match(/TransientPublicationBatch::new\s*\(/g)?.length,
    1,
    "managed transfer must be the only singleton batch wrapper in this module",
  );
  assert.match(
    createOnlyImpl,
    /TransientPublicationBatch::new\s*\(\s*vec!\[\s*sealed\s*\]\s*\)/,
  );
  assert.match(
    createOnlyImpl,
    /Err\(failure\)[\s\S]*?failure\.error\(\)\.kind\(\)[\s\S]*?failure\.into_stages\(\)/,
  );
  assert.match(
    createOnlyImpl,
    /map_singleton_publication\(batch\.publish_create_new\(\), report, authority\)/,
  );
  assert.doesNotMatch(production, /sealed\.publish_create_new\s*\(/);
  const publicationMapper = functionBlock(production, "map_singleton_publication");
  assert.match(publicationMapper, /TransientPublicationBatchOutcome::Published\(files\)/);
  assert.match(
    publicationMapper,
    /TransientPublicationBatchOutcome::NoEffect\s*\{\s*error,\s*batch\s*\}[\s\S]*?batch\.into_stages\(\)/,
  );
  assert.match(
    publicationMapper,
    /TransientPublicationBatchOutcome::Partial\s*\{\s*error,\s*members\s*\}[\s\S]*?TransientPublicationMember::Published\(file\)[\s\S]*?TransientPublicationMember::Unpublished\(sealed\)/,
  );
  assert.match(publicationMapper, /TransientPublicationBatchOutcome::Pending\(obligation\)/);
  const singletonExtractor = functionBlock(production, "take_singleton");
  assert.match(singletonExtractor, /values\.len\(\) == 1/);
  assert.match(singletonExtractor, /values\s*\.pop\(\)/);
  assert.doesNotMatch(singletonExtractor, /panic!|format!|try_into/);
  const reconcilePublication = braceBlock(production, "impl TransferPublicationObligation");
  assert.match(
    reconcilePublication,
    /map_singleton_publication\(obligation\.reconcile\(\), self\.report, self\.authority\)/,
  );
  assert.match(production, /impl Read for VerifiedSource/);
  assert.match(production, /impl Seek for VerifiedSource/);
  assert.match(production, /#\[must_use[^\]]*\][\s\S]*?pub struct VerifiedCreateOnly/);
  assert.match(production, /#\[must_use[^\]]*\][\s\S]*?pub struct VerifiedSource/);
});

test("managed transfer reports are bounded redacted observations", async () => {
  const source = await read("core/minecraft/src/download/transient_transfer.rs");
  const production = productionSource(source);
  for (const marker of [
    "pub struct TransferFailureEvent",
    "pub struct TransferFailureReport",
    "pub struct TransferReport",
    "pub struct VerifiedTransferDigests",
  ]) {
    const block = braceBlock(production, marker);
    assert.doesNotMatch(block, /String|Path|Url|reqwest::Error|io::Error\b/);
    assert.doesNotMatch(block, /\bpub\s+[a-z_]+\s*:/, `${marker} fields must stay private`);
  }
  assert.match(production, /const MAX_FAILURE_EVENTS: usize = MAX_ATTEMPTS;/);
  assert.match(
    functionBlock(production, "record_terminal"),
    /self\.events\.len\(\) < MAX_FAILURE_EVENTS/,
  );
  const digestDebug = braceBlock(production, "impl fmt::Debug for VerifiedTransferDigests");
  assert.match(digestDebug, /self\.sha1\.is_some\(\)/);
  assert.match(digestDebug, /self\.sha512\.is_some\(\)/);
  const reportDebug = braceBlock(production, "impl fmt::Debug for TransferReport");
  assert.doesNotMatch(reportDebug, /\.field\("digests",\s*&self\.digests\)/);
  assert.match(reportDebug, /self\.digests\.sha1\.is_some\(\)/);
  assert.doesNotMatch(production, /format!\([^\n]*(?:url|path|reqwest|provider)/i);
});

test("managed transfer client preserves raw provider bytes", async () => {
  const [source, apiManifest] = await Promise.all([
    read("core/minecraft/src/download/transient_transfer.rs"),
    read("apps/api/Cargo.toml"),
  ]);
  const build = functionBlock(source, "build");
  assert.match(build, /reqwest::Client::builder\(\)/);
  assert.doesNotMatch(build, /reqwest::ClientBuilder/);
  assert.match(build, /\.connect_timeout\(config\.connect_timeout\)/);
  assert.match(build, /\.read_timeout\(config\.idle_read_timeout\)/);
  assert.match(build, /\.timeout\(config\.request_timeout\)/);
  assert.match(build, /let pinned_public = !config\.pinned_origins\.is_empty\(\)/);
  assert.match(build, /if pinned_public/);
  assert.match(build, /builder = builder\.no_proxy\(\)/);
  assert.equal(build.match(/\.no_proxy\(\)/g)?.length, 1);
  assert.match(build, /parse_transfer_literal_ip\(&pinned\.origin\.host\)\.is_none\(\)/);
  assert.match(build, /builder\.resolve_to_addrs\(&pinned\.origin\.host, &pinned\.addresses\)/);
  assert.match(build, /\.redirect\(reqwest::redirect::Policy::custom/);
  assert.match(build, /transfer_redirect_admitted\([\s\S]*?pinned_public[\s\S]*?attempt\.previous\(\)[\s\S]*?attempt\.url\(\)/);
  assert.match(build, /attempt\.error\(TransferRedirectPolicyError\)/);
  assert.match(build, /attempt\.follow\(\)/);
  assert.match(build, /\.referer\(false\)/);
  assert.match(build, /\.retry\(reqwest::retry::never\(\)\)/);
  assert.doesNotMatch(build, /Policy::none|Policy::limited/);
  for (const decoder of ["no_gzip", "no_brotli", "no_deflate", "no_zstd"]) {
    assert.match(build, new RegExp(`\\.${decoder}\\(\\)`));
  }
  assert.match(source, /const MAX_REDIRECTS: usize = 8;/);
  assert.match(source, /const MAX_TRANSFER_ORIGINS: usize = 8;/);
  assert.match(source, /const MAX_PINNED_ADDRESSES_PER_ORIGIN: usize = 32;/);
  assert.match(source, /const MAX_CONNECT_TIMEOUT: Duration/);
  assert.match(source, /const MAX_IDLE_READ_TIMEOUT: Duration/);
  assert.match(source, /const MAX_REQUEST_TIMEOUT: Duration/);
  const config = functionBlock(source, "bounded");
  assert.match(config, /validate_timeout\([\s\S]*?TransferTimeoutKind::Connect/);
  assert.match(config, /validate_timeout\([\s\S]*?TransferTimeoutKind::IdleRead/);
  assert.match(config, /validate_timeout\([\s\S]*?TransferTimeoutKind::Request/);
  assert.match(config, /timeout > request_timeout/);
  assert.match(config, /origins\.is_empty\(\)/);
  assert.match(config, /origins\.len\(\) > MAX_TRANSFER_ORIGINS/);
  assert.match(config, /origins\[\.\.index\]\.contains\(origin\)/);
  const pinnedOrigin = functionBlock(source, "public");
  assert.match(pinnedOrigin, /origin\.scheme != TransferOriginScheme::Https/);
  assert.match(pinnedOrigin, /addresses\.is_empty\(\)/);
  assert.match(pinnedOrigin, /addresses\.len\(\) > MAX_PINNED_ADDRESSES_PER_ORIGIN/);
  assert.match(pinnedOrigin, /address\.port\(\) != origin\.port/);
  assert.match(pinnedOrigin, /!is_public_transfer_address\(address\.ip\(\)\)/);
  assert.match(pinnedOrigin, /parse_transfer_literal_ip\(&origin\.host\)/);
  assert.match(pinnedOrigin, /literal_host\.is_some_and\(\|host\| host != address\.ip\(\)\)/);
  assert.match(pinnedOrigin, /addresses\[\.\.index\]\.contains\(address\)/);
  const publicIpv4 = functionBlock(source, "is_public_transfer_ipv4");
  assert.match(publicIpv4, /first == 192 && second == 88 && third == 99/);
  const publicIpv6 = functionBlock(source, "is_public_transfer_ipv6");
  assert.match(publicIpv6, /\(segments\[0\] & 0xe000\) == 0x2000/);
  assert.match(publicIpv6, /segments\[0\] == 0x2001 && segments\[1\] <= 0x01ff/);
  assert.match(publicIpv6, /segments\[0\] == 0x2001 && segments\[1\] == 0x0db8/);
  assert.match(publicIpv6, /segments\[0\] == 0x2002/);
  assert.match(publicIpv6, /segments\[0\] == 0x3fff && \(segments\[1\] & 0xf000\) == 0/);
  const pinnedConfig = functionBlock(source, "bounded_pinned_public");
  assert.match(pinnedConfig, /pinned\.origin\.clone\(\)/);
  assert.match(pinnedConfig, /previous\.origin\.host == pinned\.origin\.host/);
  assert.match(pinnedConfig, /TransferClientConfigError::AmbiguousPinnedHost/);
  const origin = braceBlock(source, "impl TransferOrigin");
  const literalIp = functionBlock(source, "parse_transfer_literal_ip");
  assert.match(literalIp, /host\.strip_prefix\('\['\)/);
  assert.match(literalIp, /host\.strip_suffix\('\]'\)/);
  const fromUrl = functionBlock(origin, "from_url");
  assert.match(fromUrl, /url\.scheme\(\) != "https"/);
  assert.match(fromUrl, /TransferOriginError::UnsupportedScheme/);
  assert.doesNotMatch(fromUrl, /LoopbackHttp|from_loopback_http_for_test/);
  assert.match(
    source,
    /#\[cfg\(any\(test, feature = "test-support"\)\)\]\s*pub fn from_loopback_http_for_test_support/,
  );
  assert.match(
    source,
    /#\[cfg\(any\(test, feature = "test-support"\)\)\]\s*LoopbackHttp/,
  );
  assert.match(
    apiManifest,
    /^axial-minecraft = \{ path = "\.\.\/\.\.\/core\/minecraft" \}$/m,
  );
  assert.match(
    apiManifest,
    /^axial-minecraft = \{ path = "\.\.\/\.\.\/core\/minecraft", features = \["test-support"\] \}$/m,
  );
  const originAdmission = functionBlock(origin, "admits");
  assert.match(originAdmission, /url\.username\(\)\.is_empty\(\)/);
  assert.match(originAdmission, /url\.password\(\)\.is_none\(\)/);
  assert.match(
    originAdmission,
    /TransferOriginScheme::Https => url\.scheme\(\) == "https"/,
  );
  assert.match(originAdmission, /url\.host_str\(\)\.is_some_and\(\|host\| host == &\*self\.host\)/);
  assert.match(originAdmission, /url\.port_or_known_default\(\) == Some\(self\.port\)/);
  const runTransfer = functionBlock(source, "run_transfer");
  assert.match(runTransfer, /if !client\.admits_url\(&url\)/);
  assert.match(runTransfer, /TransferFailureKind::RequestPolicy/);
  const loopbackOrigin = functionBlock(source, "from_loopback_http_for_test_support");
  assert.match(loopbackOrigin, /\.and_then\(parse_transfer_literal_ip\)/);
  assert.match(loopbackOrigin, /address\.is_loopback\(\)/);
  assert.match(loopbackOrigin, /url\.scheme\(\) != "http"/);
  const redirects = functionBlock(source, "transfer_redirect_admitted");
  assert.match(redirects, /previous\.len\(\) >= MAX_REDIRECTS/);
  assert.match(redirects, /previous\.first\(\)/);
  assert.match(redirects, /if pinned_public/);
  assert.match(redirects, /\.find\(\|origin\| origin\.admits\(originating_url\)\)/);
  assert.match(redirects, /\.is_some_and\(\|origin\| origin\.admits\(destination\)\)/);
  assert.match(redirects, /origins\.iter\(\)\.any\(\|origin\| origin\.admits\(destination\)\)/);
  const producer = functionBlock(source, "run_producer");
  assert.match(producer, /headers\(identity_request_headers\(\)\)/);
  assert.match(
    functionBlock(source, "identity_request_headers"),
    /headers\.insert\([\s\S]*?ACCEPT_ENCODING[\s\S]*?from_static\("identity"\)/,
  );
  assert.match(producer, /response_has_identity_encoding/);
  assert.match(producer, /TransferFailureKind::ContentEncodingRejected/);
  assert.match(producer, /provider_status_failure\(response\.status\(\)\)/);
  assert.match(
    functionBlock(source, "provider_status_failure"),
    /status != reqwest::StatusCode::OK/,
  );
  const requestFailure = functionBlock(source, "classify_request_error");
  assert.match(requestFailure, /error\.is_builder\(\)/);
  assert.match(requestFailure, /error\.is_redirect\(\)/);
  assert.match(requestFailure, /TransferFailureKind::RequestPolicy/);
  const encodingTest = functionBlock(
    source,
    "content_encoding_accepts_only_absent_or_identity_values",
  );
  for (const value of ["identity", "IDENTITY", "gzip", "br", "deflate", "zstd"]) {
    assert.match(encodingTest, new RegExp(`"${value}"`));
  }
  assert.match(encodingTest, /"identity, identity"/);
  assert.match(encodingTest, /"identity, gzip"/);
});

test("managed transfer bounds queued payload and gives one blocking writer stage authority", async () => {
  const source = await read("core/minecraft/src/download/transient_transfer.rs");
  const production = productionSource(source);
  assert.match(production, /const FRAME_BYTES: usize = 64 \* 1024;/);
  assert.match(production, /const FRAME_CAPACITY: usize = 8;/);
  const attempt = functionBlock(production, "run_attempt");
  assert.match(attempt, /mpsc::channel\(FRAME_CAPACITY\)/);
  assert.match(attempt, /spawn_blocking/);
  assert.match(attempt, /let writer_exit = writer\.await;/);
  assert.match(attempt, /AssertUnwindSafe\(run_producer/);
  assert.match(attempt, /\.catch_unwind\(\)/);
  assert.match(attempt, /producer_panicked[\s\S]*?merge_panicked_producer/);
  assert.doesNotMatch(attempt, /\.abort\(\)/);
  const producer = functionBlock(production, "run_producer");
  assert.match(producer, /chunk\.chunks\(FRAME_BYTES\)/);
  assert.match(producer, /to_vec\(\)\.into_boxed_slice\(\)/);
  assert.match(producer, /admit_bytes\(contract\.bytes, produced/);
  const writer = functionBlock(production, "run_writer");
  assert.match(writer, /receiver\.blocking_recv\(\)/);
  assert.match(writer, /admit_bytes\(contract\.bytes, written/);
  assert.match(writer, /stage\.write_all\(&frame\)/);
  assert.match(writer, /hashers\.update\(&frame\)/);
  assert.equal(production.match(/stage\.write_all\(/g)?.length, 1);
});

test("managed transfer reserves and readies a stage before provider access", async () => {
  const source = await read("core/minecraft/src/download/transient_transfer.rs");
  const production = productionSource(source);
  const writer = functionBlock(production, "run_writer");
  assert.doesNotMatch(writer, /admit_transient_destination/);
  assert.ok(writer.indexOf("destination.create_stage()") < writer.indexOf("ready.send(())"));
  const producer = functionBlock(production, "run_producer");
  const readinessWait = producer.indexOf("&mut readiness");
  const providerRequest = producer.indexOf(".get(url.clone())");
  assert.ok(readinessWait >= 0 && providerRequest > readinessWait);
  assert.match(producer, /response\.content_length\(\)/);
  assert.match(producer, /ContentLengthContractMismatch/);
  assert.match(producer, /ContentLengthMismatch/);
  const writerFinish = writer.slice(writer.indexOf("WriterMessage::Finish"));
  assert.match(writerFinish, /producer_bytes != written/);
  assert.match(writerFinish, /declared != written/);
  assert.match(writerFinish, /contract\.bytes\.admits_final\(written\)/);
});

test("managed transfer cancellation owns task and writer terminality", async () => {
  const source = await read("core/minecraft/src/download/transient_transfer.rs");
  const production = productionSource(source);
  const producer = functionBlock(production, "run_producer");
  assert.match(producer, /wait_for_writer\([\s\S]*?&mut readiness\)/);
  assert.match(producer, /wait_for_writer\(&mut cancellation, &messages, request\)/);
  assert.match(producer, /wait_for_writer\([\s\S]*?response\.chunk\(\)\)/);
  assert.match(producer, /wait_for_writer\([\s\S]*?messages\.send\(WriterMessage::Frame/);
  assert.match(producer, /wait_for_writer\([\s\S]*?messages\.send\(WriterMessage::Finish/);
  const providerWait = functionBlock(production, "wait_for_writer");
  assert.match(providerWait, /cancellation\.cancelled\(\)/);
  assert.match(providerWait, /result = future => Some\(result\)/);
  assert.match(providerWait, /messages\.closed\(\)/);
  assert.match(functionBlock(production, "run_transfer"), /wait\(tokio::time::sleep\(delay\)\)/);
  const taskDrop = braceBlock(production, "impl<T> Drop for TransferTask<T>");
  assert.match(taskDrop, /self\.cancellation\.cancel\(\)/);
  const taskImpl = braceBlock(production, "impl<T: Send + 'static> TransferTask<T>");
  assert.match(taskImpl, /pub async fn join/);
  assert.match(taskImpl, /\.join[\s\S]*?\.as_mut\(\)[\s\S]*?\.await/);
  assert.match(taskImpl, /\.join[\s\S]*?\.take\(\)/);
  assert.match(taskImpl, /pub async fn cancel_and_join/);
  const attempt = functionBlock(production, "run_attempt");
  assert.ok(attempt.indexOf("drop(messages)") < attempt.indexOf("writer.await"));
  assert.match(attempt, /cancellation\.is_cancelled\(\)[\s\S]*?ProducerExit::Failed\(TransferFailureKind::Cancelled\)/);
  assert.doesNotMatch(production, /JoinHandle::abort|\.abort\(\)|mem::forget/);
  for (const testName of [
    "cancellation_owner_drop_wakes_waiters",
    "transfer_task_drop_cancels_its_supervisor",
    "writer_channel_closure_interrupts_a_pending_provider_wait",
    "source_transfer_verifies_replays_and_discards_without_publication",
    "join_failure_returns_retained_unsettled_authority",
    "polled_join_drop_cancels_supervisor_and_settles_authority",
    "redirect_policy_rejects_cross_origin_and_ninth_redirect",
    "target_cancellation_returns_retained_terminal_authority",
  ]) {
    assert.match(source, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
});

test("managed transfer retains opaque operation authority through settlement", async () => {
  const source = await read("core/minecraft/src/download/transient_transfer.rs");
  const production = productionSource(source);
  for (const marker of [
    "pub struct CreateOnlyTransferTarget",
    "pub struct SourceOnlyTransferTarget",
    "pub struct TransferCleanupObligation",
    "pub struct VerifiedCreateOnly",
    "pub struct VerifiedSource",
    "pub struct TransferPublicationObligation",
    "pub struct VerifiedTransferDiscardObligation",
    "pub struct TransferTask",
  ]) {
    assert.match(braceBlock(production, marker), /authority:\s*ManagedTransferAuthority/);
  }
  for (const marker of [
    "pub enum TransferOutcome",
    "pub enum TransferCleanupResolution",
    "pub enum TransferPublicationOutcome",
    "pub enum VerifiedTransferDiscardOutcome",
  ]) {
    assert.match(braceBlock(production, marker), /authority:\s*ManagedTransferAuthority/);
  }
  const unsettled = braceBlock(production, "pub enum TransferOutcome");
  assert.match(
    unsettled,
    /Unsettled\s*\{[\s\S]*?report:\s*TransferFailureReport,[\s\S]*?authority:\s*ManagedTransferAuthority/,
  );
  const taskJoin = functionBlock(production, "join");
  assert.match(taskJoin, /Err\(_\) => TransferOutcome::Unsettled/);
  assert.match(taskJoin, /authority:\s*self\.authority\.retained\(\)/);
  assert.ok(taskJoin.indexOf(".as_mut()") < taskJoin.indexOf(".await"));
  assert.ok(taskJoin.indexOf(".await") < taskJoin.indexOf(".take()"));
  const transfer = functionBlock(production, "run_transfer");
  assert.match(transfer, /TransferOutcome::Unsettled\s*\{[\s\S]*?authority,/);
  assert.match(transfer, /TransferCleanupObligation\s*\{[\s\S]*?authority,/);
  assert.doesNotMatch(production, /struct\s+\w*(?:Effect|Framework)|enum\s+\w*(?:Effect|Framework)/);
  for (const testName of [
    "source_transfer_verifies_replays_and_discards_without_publication",
    "create_only_transfer_publishes_through_singleton_batch",
    "authenticated_terminal_failure_discards_stage_and_cancels_destination",
    "join_failure_returns_retained_unsettled_authority",
    "polled_join_drop_cancels_supervisor_and_settles_authority",
    "transfer_task_drop_cancels_its_supervisor",
  ]) {
    const behavior = functionBlock(source, testName);
    assert.match(behavior, /authority_drops|drops\.load/);
  }
});

test("managed transfer retries only after terminal discard", async () => {
  const source = await read("core/minecraft/src/download/transient_transfer.rs");
  const production = productionSource(source);
  const transfer = functionBlock(production, "run_transfer");
  const discarded = transfer.indexOf("AttemptOutcome::Discarded {");
  const retry = transfer.indexOf("retry.permits_retry(&failure)");
  const pending = transfer.indexOf("AttemptOutcome::CleanupPending");
  assert.ok(discarded >= 0 && retry > discarded && pending > retry);
  assert.match(
    transfer,
    /AttemptOutcome::Discarded\s*\{[\s\S]*?destination:\s*returned_destination[\s\S]*?destination\s*=\s*returned_destination/,
  );
  assert.doesNotMatch(
    transfer,
    /admit_transient_destination|target\.clone\(\)|destination\.clone\(\)/,
  );
  assert.doesNotMatch(transfer.slice(pending), /permits_retry/);
  const reconcile = braceBlock(production, "impl TransferCleanupObligation");
  assert.match(
    reconcile,
    /TransientStageCreateOutcome::Created\(stage\)[\s\S]*?stage\.discard\(\)/,
  );
  assert.match(reconcile, /TransferCleanupResolution::Pending\(self\)/);
  assert.match(reconcile, /TransientDiscardOutcome::Discarded\(destination\)/);
  assert.match(reconcile, /destination\.cancel\(\)/);
  assert.match(reconcile, /TransferCleanupState::DestinationCancel/);
  const terminal = functionBlock(production, "terminal_failure");
  assert.match(terminal, /destination\.cancel\(\)/);
  assert.match(terminal, /TransientDestinationCancelOutcome::Cancelled/);
  assert.match(terminal, /TransferOutcome::CleanupPending/);
  assert.match(production, /const MAX_ATTEMPTS: usize = 8;/);
  assert.match(production, /const MAX_RETRY_DELAYS: usize = MAX_ATTEMPTS - 1;/);
  assert.match(production, /const MAX_RETRY_DELAY: Duration = Duration::from_secs\(30\);/);
  assert.match(production, /const MAX_RETRY_WINDOW: Duration = Duration::from_secs\(2 \* 60\);/);
  const retryConfig = functionBlock(production, "classified");
  assert.match(retryConfig, /delay\.is_zero\(\)/);
  assert.match(retryConfig, /\*delay > MAX_RETRY_DELAY/);
  assert.match(retryConfig, /\.checked_add\(\*delay\)/);
  assert.match(retryConfig, /\*window <= MAX_RETRY_WINDOW/);
  const retryable = functionBlock(production, "is_policy_retryable");
  assert.match(retryable, /ProviderStatus\(408 \| 425 \| 429 \| 500\.\.=599\)/);
  assert.doesNotMatch(retryable, /ProviderStatus\(_\)/);
  const merge = functionBlock(production, "merge_attempt_outcome");
  assert.equal(merge.match(/select_attempt_failure\(producer, writer_failure\)/g)?.length, 2);
  for (const testName of [
    "cleanup_pending_preserves_the_actual_producer_cause",
    "digest_metadata_is_canonicalized_to_typed_bytes",
    "authenticated_contracts_require_digest_authority",
    "authenticated_terminal_failure_discards_stage_and_cancels_destination",
    "create_only_transfer_publishes_through_singleton_batch",
    "byte_contracts_keep_exact_at_most_and_below_distinct",
    "engine_retry_ceiling_allows_only_documented_transients",
    "provider_requires_exactly_ok_without_a_range_request",
    "pinned_public_config_rejects_ambiguous_hosts",
    "pinned_public_address_filter_rejects_every_special_network_class",
    "pinned_public_origin_rejects_private_mismatched_and_duplicate_addresses",
    "retry_policy_caps_total_attempts_at_eight",
    "requested_digest_combinations_hash_only_requested_algorithms",
    "transfer_client_config_requires_positive_bounded_timeouts",
  ]) {
    assert.match(source, new RegExp(`fn\\s+${testName}\\s*\\(`));
  }
});
