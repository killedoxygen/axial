import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../../../${path}`, import.meta.url), "utf8");
const exists = async (path) => access(new URL(`../../../${path}`, import.meta.url)).then(
  () => true,
  () => false,
);

test("music cache is AppState-owned and capability-only", async () => {
  const [state, owner, root, paths, application] = await Promise.all([
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/music_cache.rs"),
    read("core/config/src/root.rs"),
    read("core/config/src/paths/mod.rs"),
    read("apps/api/src/application/music.rs"),
  ]);
  const productionApplication = application.split("#[cfg(test)]\nmod tests")[0];

  assert.match(state, /music_cache: MusicCacheOwner/);
  assert.match(state, /MusicCacheOwner::new\(Arc::clone\(&root_session\)\)/);
  assert.match(owner, /root_session: Arc<AppRootSession>/);
  assert.match(owner, /directory: Mutex<MusicDirectoryState>/);
  assert.match(owner, /enum MusicDirectoryState[\s\S]*Vacant[\s\S]*Retained\(Directory\)[\s\S]*Released/);
  assert.match(owner, /flights: MusicFlights/);
  assert.match(owner, /struct MusicSources[\s\S]*vapor_halo:[\s\S]*sublunar_hum:/);
  assert.match(owner, /struct MusicFlights[\s\S]*vapor_halo: Mutex<MusicTrackFlight>[\s\S]*sublunar_hum: Mutex<MusicTrackFlight>/);
  assert.match(owner, /fn get\(&self, track: MusicTrackId\)[\s\S]*MusicTrackId::VaporHalo[\s\S]*MusicTrackId::SublunarHum/);
  assert.match(owner, /production_track_identity_keeps_filename_and_source_together/);
  assert.match(owner, /struct MusicTrackSpec[\s\S]*id: MusicTrackId[\s\S]*file: &'static str[\s\S]*source: &'static str/);
  assert.match(owner, /MusicTestSources[\s\S]*vapor_halo:[\s\S]*sublunar_hum:/);
  assert.doesNotMatch(productionApplication, /const MUSIC_TRACKS/);
  assert.match(root, /pub fn open_music_directory\(&self\)/);
  assert.match(root, /pub fn prepare_music_directory\(&self\)/);
  assert.doesNotMatch(paths, /music_dir/);
  assert.doesNotMatch(
    productionApplication,
    /\b(?:std::fs|tokio::fs|async_fs)::|PathBuf|OnceLock|reqwest::Client::builder/,
  );
});

test("music flights retain lifecycle and transfer ownership", async () => {
  const [owner, application, route, lifecycle, shutdown] = await Promise.all([
    read("apps/api/src/state/music_cache.rs"),
    read("apps/api/src/application/music.rs"),
    read("apps/api/src/routes/music.rs"),
    read("apps/api/src/state/lifecycle.rs"),
    read("apps/api/src/state/shutdown.rs"),
  ]);

  assert.match(owner, /MusicFlightState[\s\S]*Idle[\s\S]*Running[\s\S]*Unsettled/);
  assert.match(owner, /handoff\.try_claim\(\)/);
  assert.match(owner, /drop\(flight\);[\s\S]*MusicFlightLaunchGuard/);
  assert.doesNotMatch(owner, /wrapping_add/);
  assert.match(application, /producer\.wait_for_request_drain_start\(\)/);
  assert.match(application, /pub async fn music_status\([\s\S]*RequestProducerHandoff/);
  assert.match(application, /music_status[\s\S]*handoff\.try_claim\(\)[\s\S]*spawn_joinable/);
  assert.match(application, /producer\.spawn\(async move/);
  assert.match(application, /cancellation_sender\.cancel\(\);[\s\S]*joined\.await/);
  assert.match(application, /TransferOutcome::CleanupPending[\s\S]*obligation\.reconcile\(\)/);
  assert.match(application, /TransferPublicationOutcome::Pending[\s\S]*obligation\.reconcile\(\)/);
  assert.match(application, /VerifiedTransferDiscardOutcome::Pending[\s\S]*obligation\.reconcile\(\)/);
  assert.match(application, /drop\(obligation\);[\s\S]*MusicFlightCompletion::Unsettled/);
  assert.match(application, /impl Drop for MusicFlightGuard/);
  assert.match(application, /spawn_joinable\(async move[\s\S]*spawn_blocking/);
  assert.match(route, /Extension\(handoff\): Extension<RequestProducerHandoff>/);
  assert.match(route, /StatusCode::NOT_FOUND/);
  assert.match(route, /StatusCode::BAD_GATEWAY/);
  assert.match(route, /producer_claim_error_response/);
  assert.match(lifecycle, /active_producers/);
  assert.match(shutdown, /release_directory_after_producer_drain/);
  assert.match(owner, /release_directory_after_producer_drain[\s\S]*MusicDirectoryState::Released/);
  assert.match(owner, /retain_opened_directory[\s\S]*MusicDirectoryState::Released => Err\(music_directory_released\(\)\)/);
  assert.match(owner, /prepare_target[\s\S]*ensure_directory_available\(\)\?[\s\S]*prepare_music_directory\(\)\?/);
});

test("music transfer policy is exact, bounded, and create-only", async () => {
  const [owner, application, transfer] = await Promise.all([
    read("apps/api/src/state/music_cache.rs"),
    read("apps/api/src/application/music.rs"),
    read("core/minecraft/src/download/transient_transfer.rs"),
  ]);

  assert.match(owner, /https:\/\/github\.com\//);
  assert.match(owner, /https:\/\/release-assets\.githubusercontent\.com\//);
  assert.match(owner, /MUSIC_CONNECT_TIMEOUT: Duration = Duration::from_secs\(10\)/);
  assert.match(owner, /MUSIC_IDLE_READ_TIMEOUT: Duration = Duration::from_secs\(120\)/);
  assert.match(owner, /MUSIC_REQUEST_TIMEOUT: Duration = Duration::from_secs\(120\)/);
  assert.match(owner, /MUSIC_MAX_BYTES: u64 = 32 \* 1024 \* 1024/);
  assert.match(owner, /admit_transient_destination\(name\)/);
  assert.match(owner, /from_loopback_http_for_test_support/);
  assert.match(application, /TransferContract::unauthenticated_at_most/);
  assert.match(application, /RetryPolicy::none\(\)/);
  assert.match(application, /start_create_only_transfer/);
  assert.match(application, /TransferPublicationOutcome::NoEffect[\s\S]*settle_no_effect_publication/);
  assert.match(application, /verified\.discard\(\)/);
  assert.match(application, /cached_track_is_bounded/);
  assert.doesNotMatch(
    application,
    /RetryPolicy::classified|reqwest::redirect::Policy::none|Client::new/,
  );
  assert.match(transfer, /provider_status_failure\(response\.status\(\)\)/);
  assert.match(transfer, /status != reqwest::StatusCode::OK/);
});

test("music cutover deletes legacy downloader and vocabulary", async () => {
  const [execution, ownership, guardianModel, guardianFacts, architecture] = await Promise.all([
    read("apps/api/src/execution/mod.rs"),
    read("apps/api/src/state/ownership.rs"),
    read("apps/api/src/guardian/model.rs"),
    read("apps/api/src/guardian/facts.rs"),
    read("docs/ARCHITECTURE.md"),
  ]);

  assert.equal(await exists("apps/api/src/execution/download.rs"), false);
  assert.doesNotMatch(execution, /pub mod download|DownloadTempDiscarded|download_temp_discarded/);
  assert.doesNotMatch(ownership, /MusicCacheFile|music_cache_file/);
  assert.doesNotMatch(guardianModel, /DownloadTempDiscarded|download_temp_discarded/);
  assert.doesNotMatch(guardianFacts, /DownloadTempDiscarded|download_temp_discarded/);
  assert.match(architecture, /Each `AppState` owns one root-scoped music cache owner/);
  assert.match(architecture, /Execution has no generic path-based downloader/);
});
