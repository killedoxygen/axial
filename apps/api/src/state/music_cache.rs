use super::{LifecycleAdmissionError, ProducerLease, RequestProducerHandoff};
use axial_config::AppRootSession;
use axial_fs::{
    Directory, DirectoryListingState, EntryKind, FileCapability, LeafName, leaf_names_equivalent,
};
use axial_minecraft::download::{
    CreateOnlyTransferTarget, ManagedTransferAuthority, TransferClient, TransferClientConfig,
    TransferOrigin,
};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::watch;

pub(crate) const MUSIC_TRACK_COUNT: usize = 2;
pub(crate) const MUSIC_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MusicTrackId {
    VaporHalo,
    SublunarHum,
}

impl MusicTrackId {
    fn spec(self) -> &'static MusicTrackSpec {
        MUSIC_TRACKS
            .iter()
            .find(|track| track.id == self)
            .expect("every fixed music track identity has one specification")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MusicTrackSpec {
    pub(crate) id: MusicTrackId,
    pub(crate) file: &'static str,
    source: &'static str,
}

pub(crate) const MUSIC_TRACKS: [MusicTrackSpec; MUSIC_TRACK_COUNT] = [
    MusicTrackSpec {
        id: MusicTrackId::VaporHalo,
        file: "vapor-halo.mp3",
        source: "https://github.com/mateoltd/axial/releases/download/music-v2/vapor-halo.mp3",
    },
    MusicTrackSpec {
        id: MusicTrackId::SublunarHum,
        file: "sublunar-hum.mp3",
        source: "https://github.com/mateoltd/axial/releases/download/music-v2/sublunar-hum.mp3",
    },
];

#[cfg(test)]
pub(crate) struct MusicTestSources {
    pub(crate) vapor_halo: reqwest::Url,
    pub(crate) sublunar_hum: reqwest::Url,
}

#[cfg(test)]
impl MusicTestSources {
    fn into_sources(self) -> MusicSources {
        MusicSources {
            vapor_halo: Some(self.vapor_halo),
            sublunar_hum: Some(self.sublunar_hum),
        }
    }
}

const MUSIC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MUSIC_IDLE_READ_TIMEOUT: Duration = Duration::from_secs(120);
const MUSIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MUSIC_DIRECTORY_ENTRY_LIMIT: usize = 16;

#[derive(Clone)]
pub(crate) struct MusicCacheOwner {
    shared: Arc<MusicCacheShared>,
}

struct MusicCacheShared {
    root_session: Arc<AppRootSession>,
    directory: Mutex<MusicDirectoryState>,
    client: Option<TransferClient>,
    sources: MusicSources,
    flights: MusicFlights,
}

enum MusicDirectoryState {
    Vacant,
    Retained(Directory),
    Released,
}

struct MusicSources {
    vapor_halo: Option<reqwest::Url>,
    sublunar_hum: Option<reqwest::Url>,
}

impl MusicSources {
    fn production() -> Self {
        Self {
            vapor_halo: reqwest::Url::parse(MusicTrackId::VaporHalo.spec().source).ok(),
            sublunar_hum: reqwest::Url::parse(MusicTrackId::SublunarHum.spec().source).ok(),
        }
    }

    fn get(&self, track: MusicTrackId) -> Option<reqwest::Url> {
        match track {
            MusicTrackId::VaporHalo => self.vapor_halo.clone(),
            MusicTrackId::SublunarHum => self.sublunar_hum.clone(),
        }
    }
}

struct MusicFlights {
    vapor_halo: Mutex<MusicTrackFlight>,
    sublunar_hum: Mutex<MusicTrackFlight>,
}

impl MusicFlights {
    fn new() -> Self {
        Self {
            vapor_halo: Mutex::new(MusicTrackFlight::idle()),
            sublunar_hum: Mutex::new(MusicTrackFlight::idle()),
        }
    }

    fn get(&self, track: MusicTrackId) -> &Mutex<MusicTrackFlight> {
        match track {
            MusicTrackId::VaporHalo => &self.vapor_halo,
            MusicTrackId::SublunarHum => &self.sublunar_hum,
        }
    }
}

struct MusicTrackFlight {
    next_id: u64,
    state: MusicFlightState,
}

impl MusicTrackFlight {
    fn idle() -> Self {
        Self {
            next_id: 0,
            state: MusicFlightState::Idle,
        }
    }
}

enum MusicFlightState {
    Idle,
    Running {
        id: u64,
        completion: watch::Sender<MusicFlightCompletion>,
    },
    Unsettled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MusicFlightCompletion {
    Running,
    Ready,
    Failed,
    Unsettled,
}

pub(crate) enum MusicFlightClaim {
    Started(watch::Receiver<MusicFlightCompletion>),
    Join(watch::Receiver<MusicFlightCompletion>),
    Unsettled,
}

impl MusicCacheOwner {
    pub(crate) fn new(root_session: Arc<AppRootSession>) -> Self {
        Self::with_client(
            root_session,
            production_music_client(),
            MusicSources::production(),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_test_sources(
        root_session: Arc<AppRootSession>,
        sources: MusicTestSources,
    ) -> Self {
        let sources = sources.into_sources();
        let mut origins = Vec::with_capacity(MUSIC_TRACK_COUNT);
        for source in [&sources.vapor_halo, &sources.sublunar_hum] {
            let source = source
                .as_ref()
                .expect("test music source identity must have a URL");
            let origin = TransferOrigin::from_loopback_http_for_test_support(source)
                .expect("test music source must use IP-literal loopback HTTP");
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        let client = TransferClientConfig::bounded(
            MUSIC_CONNECT_TIMEOUT,
            MUSIC_IDLE_READ_TIMEOUT,
            MUSIC_REQUEST_TIMEOUT,
            origins,
        )
        .expect("test music sources must form a bounded origin set");
        let client = TransferClient::build(client).expect("build test music client");
        Self::with_client(root_session, Some(client), sources)
    }

    fn with_client(
        root_session: Arc<AppRootSession>,
        client: Option<TransferClient>,
        sources: MusicSources,
    ) -> Self {
        Self {
            shared: Arc::new(MusicCacheShared {
                root_session,
                directory: Mutex::new(MusicDirectoryState::Vacant),
                client,
                sources,
                flights: MusicFlights::new(),
            }),
        }
    }

    pub(crate) fn claim_flight(
        &self,
        track: MusicTrackId,
        handoff: &RequestProducerHandoff,
        start: impl FnOnce(MusicCacheOwner, MusicTrackId, u64, ProducerLease),
    ) -> Result<MusicFlightClaim, LifecycleAdmissionError> {
        let mut flight = music_lock(self.shared.flights.get(track));
        match &flight.state {
            MusicFlightState::Running { completion, .. } => {
                return Ok(MusicFlightClaim::Join(completion.subscribe()));
            }
            MusicFlightState::Unsettled => return Ok(MusicFlightClaim::Unsettled),
            MusicFlightState::Idle => {}
        }

        if flight.next_id == u64::MAX {
            flight.state = MusicFlightState::Unsettled;
            return Ok(MusicFlightClaim::Unsettled);
        }
        let producer = handoff.try_claim()?;
        let id = flight.next_id;
        flight.next_id += 1;
        let (completion, receiver) = watch::channel(MusicFlightCompletion::Running);
        flight.state = MusicFlightState::Running { id, completion };
        drop(flight);
        let mut launch = MusicFlightLaunchGuard {
            owner: self.clone(),
            track,
            id,
            armed: true,
        };
        start(self.clone(), track, id, producer);
        launch.armed = false;
        Ok(MusicFlightClaim::Started(receiver))
    }

    pub(crate) fn finish_flight(
        &self,
        track: MusicTrackId,
        id: u64,
        completion: MusicFlightCompletion,
    ) {
        let mut flight = music_lock(self.shared.flights.get(track));
        let MusicFlightState::Running {
            id: current_id,
            completion: sender,
        } = &flight.state
        else {
            return;
        };
        if *current_id != id {
            return;
        }
        let sender = sender.clone();
        flight.state = match completion {
            MusicFlightCompletion::Ready | MusicFlightCompletion::Failed => MusicFlightState::Idle,
            MusicFlightCompletion::Unsettled => MusicFlightState::Unsettled,
            MusicFlightCompletion::Running => {
                panic!("music flight cannot settle back to running")
            }
        };
        sender.send_replace(completion);
    }

    pub(crate) fn client(&self) -> Option<TransferClient> {
        self.shared.client.clone()
    }

    pub(crate) fn source(&self, track: MusicTrackId) -> Option<reqwest::Url> {
        self.shared.sources.get(track)
    }

    pub(crate) fn cached_track(&self, name: &LeafName) -> io::Result<Option<Vec<u8>>> {
        let Some(directory) = self.open_music_directory()? else {
            return Ok(None);
        };
        read_exact_track(&directory, name)
    }

    pub(crate) fn cached_track_is_bounded(&self, name: &LeafName) -> io::Result<bool> {
        let Some(directory) = self.open_music_directory()? else {
            return Ok(false);
        };
        exact_track_is_bounded_result(&directory, name)
    }

    pub(crate) fn prepare_target(
        &self,
        name: LeafName,
    ) -> io::Result<CreateOnlyTransferTarget> {
        self.ensure_directory_available()?;
        let directory = self.shared.root_session.prepare_music_directory()?;
        let directory = self
            .retain_opened_directory(Some(directory))?
            .ok_or_else(music_directory_released)?;
        directory
            .admit_transient_destination(name)
            .map(|destination| {
                CreateOnlyTransferTarget::new(
                    destination,
                    ManagedTransferAuthority::retain(Arc::clone(&self.shared)),
                )
            })
    }

    pub(crate) fn published_is_bounded(
        &self,
        file: FileCapability,
        name: &LeafName,
    ) -> io::Result<bool> {
        let Some(directory) = self.open_music_directory()? else {
            return Ok(false);
        };
        let revision = file.revision()?;
        if revision.size() > MUSIC_MAX_BYTES {
            return Ok(false);
        }
        file.validate_revision(&revision)?;
        require_exact_track_binding(&directory, name)?;
        Ok(true)
    }

    pub(crate) fn status(&self) -> [bool; MUSIC_TRACK_COUNT] {
        let Ok(directory) = self.open_music_directory() else {
            return [false; MUSIC_TRACK_COUNT];
        };
        let Some(directory) = directory else {
            return [false; MUSIC_TRACK_COUNT];
        };
        std::array::from_fn(|index| {
            let name = LeafName::new(MUSIC_TRACKS[index].file)
                .expect("fixed music track leaf is valid");
            exact_track_is_bounded(&directory, &name)
        })
    }

    pub(super) fn release_directory_after_producer_drain(&self) {
        *music_lock(&self.shared.directory) = MusicDirectoryState::Released;
    }

    fn open_music_directory(&self) -> io::Result<Option<Directory>> {
        {
            let directory = music_lock(&self.shared.directory);
            match &*directory {
                MusicDirectoryState::Retained(directory) => return Ok(Some(directory.clone())),
                MusicDirectoryState::Released => return Err(music_directory_released()),
                MusicDirectoryState::Vacant => {}
            }
        }
        let directory = self.shared.root_session.open_music_directory()?;
        self.retain_opened_directory(directory)
    }

    fn ensure_directory_available(&self) -> io::Result<()> {
        if matches!(
            &*music_lock(&self.shared.directory),
            MusicDirectoryState::Released
        ) {
            return Err(music_directory_released());
        }
        Ok(())
    }

    fn retain_opened_directory(
        &self,
        opened: Option<Directory>,
    ) -> io::Result<Option<Directory>> {
        let mut state = music_lock(&self.shared.directory);
        match &*state {
            MusicDirectoryState::Retained(directory) => Ok(Some(directory.clone())),
            MusicDirectoryState::Released => Err(music_directory_released()),
            MusicDirectoryState::Vacant => {
                let Some(directory) = opened else {
                    return Ok(None);
                };
                *state = MusicDirectoryState::Retained(directory.clone());
                Ok(Some(directory))
            }
        }
    }
}

struct MusicFlightLaunchGuard {
    owner: MusicCacheOwner,
    track: MusicTrackId,
    id: u64,
    armed: bool,
}

impl Drop for MusicFlightLaunchGuard {
    fn drop(&mut self) {
        if self.armed {
            self.owner
                .finish_flight(self.track, self.id, MusicFlightCompletion::Unsettled);
        }
    }
}

fn read_exact_track(directory: &Directory, name: &LeafName) -> io::Result<Option<Vec<u8>>> {
    let Some(file) = open_exact_track(directory, name)? else {
        return Ok(None);
    };
    let bytes = file.read_bounded(MUSIC_MAX_BYTES)?;
    require_exact_track_binding(directory, name)?;
    Ok(Some(bytes))
}

fn exact_track_is_bounded(directory: &Directory, name: &LeafName) -> bool {
    exact_track_is_bounded_result(directory, name).unwrap_or(false)
}

fn exact_track_is_bounded_result(directory: &Directory, name: &LeafName) -> io::Result<bool> {
    let Some(file) = open_exact_track(directory, name)? else {
        return Ok(false);
    };
    let revision = file.revision()?;
    if revision.size() > MUSIC_MAX_BYTES {
        return Ok(false);
    }
    file.validate_revision(&revision)?;
    require_exact_track_binding(directory, name)?;
    Ok(true)
}

fn open_exact_track(directory: &Directory, name: &LeafName) -> io::Result<Option<FileCapability>> {
    if !exact_track_binding(directory, name)? {
        return Ok(None);
    }
    directory.open_file(name).map(Some)
}

fn require_exact_track_binding(directory: &Directory, name: &LeafName) -> io::Result<()> {
    exact_track_binding(directory, name)?.then_some(()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "exact music track binding disappeared",
        )
    })
}

fn exact_track_binding(directory: &Directory, name: &LeafName) -> io::Result<bool> {
    let listing = directory.entries(MUSIC_DIRECTORY_ENTRY_LIMIT)?;
    if listing.state() != DirectoryListingState::Complete {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut equivalent = listing
        .entries()
        .iter()
        .filter(|entry| leaf_names_equivalent(entry.name(), name.as_os_str()));
    let Some(entry) = equivalent.next() else {
        return Ok(false);
    };
    if equivalent.next().is_some()
        || entry.name() != name.as_os_str()
        || entry.kind() != EntryKind::File
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(true)
}

fn music_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn music_directory_released() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "music cache directory authority was released",
    )
}

fn production_music_client() -> Option<TransferClient> {
    let github = reqwest::Url::parse("https://github.com/").ok()?;
    let release_assets =
        reqwest::Url::parse("https://release-assets.githubusercontent.com/").ok()?;
    let origins = vec![
        TransferOrigin::from_url(&github).ok()?,
        TransferOrigin::from_url(&release_assets).ok()?,
    ];
    let config = TransferClientConfig::bounded(
        MUSIC_CONNECT_TIMEOUT,
        MUSIC_IDLE_READ_TIMEOUT,
        MUSIC_REQUEST_TIMEOUT,
        origins,
    )
    .ok()?;
    TransferClient::build(config).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppLifecycle, RequestLease};
    use axial_config::AppPaths;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Semaphore;

    #[test]
    fn production_track_identity_keeps_filename_and_source_together() {
        let (owner, root) = test_owner("track-identity");
        for track in MUSIC_TRACKS {
            let source = owner.source(track.id).expect("production music source");
            assert_eq!(
                source.path_segments().and_then(Iterator::last),
                Some(track.file),
                "track identity resolved a source for a different filename"
            );
        }
        cleanup(owner, root);
    }

    #[test]
    fn status_does_not_create_the_music_directory() {
        let (owner, root) = test_owner("status-read-only");
        assert_eq!(owner.status(), [false; MUSIC_TRACK_COUNT]);
        assert!(!root.join("music").exists());
        cleanup(owner, root);
    }

    #[test]
    fn release_rejects_an_opened_directory_that_returns_late() {
        let (owner, root) = test_owner("release-race");
        fs::create_dir_all(root.join("music")).expect("create music fixture directory");
        let late = owner
            .shared
            .root_session
            .open_music_directory()
            .expect("open music directory before release");
        assert!(late.is_some());

        owner.release_directory_after_producer_drain();
        assert_eq!(
            owner
                .retain_opened_directory(late)
                .expect_err("late directory must not restore released authority")
                .kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(
            owner
                .open_music_directory()
                .expect_err("released directory must not reopen")
                .kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(owner.status(), [false; MUSIC_TRACK_COUNT]);
        assert!(matches!(
            &*music_lock(&owner.shared.directory),
            MusicDirectoryState::Released
        ));
        cleanup(owner, root);
    }

    #[test]
    fn prepare_after_release_has_no_filesystem_effect() {
        let (owner, root) = test_owner("prepare-after-release");
        owner.release_directory_after_producer_drain();
        let name = LeafName::new(MUSIC_TRACKS[0].file).expect("fixed track name");
        assert_eq!(
            owner
                .prepare_target(name)
                .expect_err("released owner must reject target preparation")
                .kind(),
            io::ErrorKind::NotConnected
        );
        assert!(!root.join("music").exists());
        cleanup(owner, root);
    }

    #[test]
    fn exact_track_size_boundary_is_zero_through_thirty_two_mibibytes() {
        let (owner, root) = test_owner("size-boundary");
        let music = root.join("music");
        fs::create_dir_all(&music).expect("create music fixture directory");
        let track = music.join(MUSIC_TRACKS[0].file);
        let name = LeafName::new(MUSIC_TRACKS[0].file).expect("fixed track name");

        fs::File::create(&track).expect("create empty track");
        assert!(owner.cached_track_is_bounded(&name).expect("inspect empty track"));
        assert_eq!(
            owner.cached_track(&name).expect("read empty track"),
            Some(Vec::new())
        );

        fs::OpenOptions::new()
            .write(true)
            .open(&track)
            .expect("open maximum track")
            .set_len(MUSIC_MAX_BYTES)
            .expect("set maximum track length");
        assert!(
            owner
                .cached_track_is_bounded(&name)
                .expect("inspect maximum track")
        );
        assert_eq!(
            owner
                .cached_track(&name)
                .expect("read maximum track")
                .expect("maximum track is present")
                .len(),
            MUSIC_MAX_BYTES as usize
        );

        fs::OpenOptions::new()
            .write(true)
            .open(&track)
            .expect("open oversized track")
            .set_len(MUSIC_MAX_BYTES + 1)
            .expect("set oversized track length");
        assert!(
            !owner
                .cached_track_is_bounded(&name)
                .expect("inspect oversized track")
        );
        cleanup(owner, root);
    }

    #[test]
    fn portable_alias_and_wrong_kind_are_not_cache_hits() {
        let (owner, root) = test_owner("exact-binding");
        let music = root.join("music");
        fs::create_dir_all(&music).expect("create music fixture directory");
        let name = LeafName::new(MUSIC_TRACKS[0].file).expect("fixed track name");

        fs::write(music.join("VAPOR-HALO.MP3"), b"alias").expect("write alias fixture");
        assert_eq!(
            owner
                .cached_track_is_bounded(&name)
                .expect_err("portable alias must fail closed")
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(music.join("VAPOR-HALO.MP3")).expect("remove alias fixture");
        fs::create_dir(music.join(MUSIC_TRACKS[0].file)).expect("create wrong-kind fixture");
        assert_eq!(
            owner
                .cached_track_is_bounded(&name)
                .expect_err("wrong-kind binding must fail closed")
                .kind(),
            io::ErrorKind::InvalidData
        );
        cleanup(owner, root);
    }

    #[test]
    fn same_track_claims_start_once_and_join_the_live_generation() {
        let (owner, root) = test_owner("same-track-flight");
        let (_lifecycle, _request, handoff) = request_handoff();
        let starts = Arc::new(AtomicUsize::new(0));
        let first_starts = Arc::clone(&starts);
        assert!(matches!(
            owner
                .claim_flight(
                    MusicTrackId::VaporHalo,
                    &handoff,
                    move |_, _, _, producer| {
                        first_starts.fetch_add(1, Ordering::SeqCst);
                        drop(producer);
                    },
                )
                .expect("claim first flight"),
            MusicFlightClaim::Started(_)
        ));
        for _ in 0..8 {
            assert!(matches!(
                owner
                    .claim_flight(
                        MusicTrackId::VaporHalo,
                        &handoff,
                        |_, _, _, _| panic!("join must not launch another producer"),
                    )
                    .expect("join live flight"),
                MusicFlightClaim::Join(_)
            ));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        owner.finish_flight(MusicTrackId::VaporHalo, 0, MusicFlightCompletion::Ready);
        cleanup(owner, root);
    }

    #[test]
    fn separate_tracks_and_owners_start_independently() {
        let (first_owner, first_root) = test_owner("independent-first");
        let (second_owner, second_root) = test_owner("independent-second");
        let (_lifecycle, _request, handoff) = request_handoff();
        let starts = Arc::new(AtomicUsize::new(0));

        for (owner, track) in [
            (first_owner.clone(), MusicTrackId::VaporHalo),
            (first_owner.clone(), MusicTrackId::SublunarHum),
            (second_owner.clone(), MusicTrackId::VaporHalo),
        ] {
            let starts = Arc::clone(&starts);
            assert!(matches!(
                owner
                    .claim_flight(track, &handoff, move |_, _, _, producer| {
                        starts.fetch_add(1, Ordering::SeqCst);
                        drop(producer);
                    })
                    .expect("claim independent flight"),
                MusicFlightClaim::Started(_)
            ));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 3);
        for (owner, track) in [
            (first_owner.clone(), MusicTrackId::VaporHalo),
            (first_owner.clone(), MusicTrackId::SublunarHum),
            (second_owner.clone(), MusicTrackId::VaporHalo),
        ] {
            assert!(matches!(
                owner
                    .claim_flight(
                        track,
                        &handoff,
                        |_, _, _, _| panic!("independent running flight must be joined"),
                    )
                    .expect("join independent running flight"),
                MusicFlightClaim::Join(_)
            ));
            owner.finish_flight(track, 0, MusicFlightCompletion::Ready);
        }
        cleanup(first_owner, first_root);
        cleanup(second_owner, second_root);
    }

    #[test]
    fn ordinary_failure_admits_retry_but_unsettled_refuses_it() {
        let (owner, root) = test_owner("failure-state");
        let (_lifecycle, _request, handoff) = request_handoff();
        assert!(matches!(
            owner
                .claim_flight(MusicTrackId::VaporHalo, &handoff, |_, _, _, _| {})
                .expect("claim first generation"),
            MusicFlightClaim::Started(_)
        ));
        owner.finish_flight(MusicTrackId::VaporHalo, 0, MusicFlightCompletion::Failed);
        assert!(matches!(
            owner
                .claim_flight(MusicTrackId::VaporHalo, &handoff, |_, _, _, _| {})
                .expect("retry ordinary failure"),
            MusicFlightClaim::Started(_)
        ));
        owner.finish_flight(
            MusicTrackId::VaporHalo,
            1,
            MusicFlightCompletion::Unsettled,
        );
        assert!(matches!(
            owner
                .claim_flight(
                    MusicTrackId::VaporHalo,
                    &handoff,
                    |_, _, _, _| panic!("unsettled flight must never restart"),
                )
                .expect("observe unsettled generation"),
            MusicFlightClaim::Unsettled
        ));
        cleanup(owner, root);
    }

    #[test]
    fn launch_panic_latches_the_track_unsettled() {
        let (owner, root) = test_owner("launch-panic");
        let (_lifecycle, _request, handoff) = request_handoff();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = owner.claim_flight(
                MusicTrackId::VaporHalo,
                &handoff,
                |_, _, _, _| panic!("launch failed"),
            );
        }));
        assert!(result.is_err());
        assert!(matches!(
            owner
                .claim_flight(
                    MusicTrackId::VaporHalo,
                    &handoff,
                    |_, _, _, _| panic!("latched flight must never restart"),
                )
                .expect("observe panic latch"),
            MusicFlightClaim::Unsettled
        ));
        cleanup(owner, root);
    }

    #[tokio::test]
    async fn request_drop_does_not_end_the_claimed_producer() {
        let (owner, root) = test_owner("request-disconnect");
        let (lifecycle, request, handoff) = request_handoff();
        let release = Arc::new(Semaphore::new(0));
        let worker_release = Arc::clone(&release);
        let worker_owner = owner.clone();
        let MusicFlightClaim::Started(first_waiter) = owner
            .claim_flight(
                MusicTrackId::VaporHalo,
                &handoff,
                move |_, _, id, producer| {
                    producer.spawn(async move {
                        worker_release
                            .acquire()
                            .await
                            .expect("worker release semaphore open")
                            .forget();
                        worker_owner.finish_flight(
                            MusicTrackId::VaporHalo,
                            id,
                            MusicFlightCompletion::Ready,
                        );
                    });
                },
            )
            .expect("claim disconnect flight")
        else {
            panic!("first request must start the flight")
        };

        drop(first_waiter);
        drop(request);
        let second_request = lifecycle.try_admit_request().expect("admit later waiter");
        let second_handoff = second_request.producer_handoff();
        let MusicFlightClaim::Join(mut second_waiter) = owner
            .claim_flight(
                MusicTrackId::VaporHalo,
                &second_handoff,
                |_, _, _, _| panic!("later waiter must join the retained producer"),
            )
            .expect("join retained producer")
        else {
            panic!("later request must join the running flight")
        };
        drop(second_request);
        let quiescing = lifecycle.clone();
        let mut quiesce = tokio::spawn(async move { quiescing.quiesce().await });
        lifecycle
            .wait_for_shutdown_started()
            .await
            .expect("request drain reaches producer quiescence");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut quiesce)
                .await
                .is_err(),
            "quiescence completed before the disconnected request's producer"
        );
        release.add_permits(1);
        second_waiter
            .changed()
            .await
            .expect("retained producer publishes completion");
        assert_eq!(*second_waiter.borrow(), MusicFlightCompletion::Ready);
        quiesce
            .await
            .expect("join quiesce task")
            .expect("producer drains after release");
        cleanup(owner, root);
    }

    fn test_owner(label: &str) -> (MusicCacheOwner, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "axial-music-cache-{label}-{}-{nonce}",
            std::process::id()
        ));
        let paths = AppPaths::from_root(root.clone()).expect("absolute test app root");
        let root_session = Arc::new(paths.open_root_session().expect("open test root session"));
        (MusicCacheOwner::new(root_session), root)
    }

    fn cleanup(owner: MusicCacheOwner, root: PathBuf) {
        drop(owner);
        fs::remove_dir_all(root).expect("remove music cache fixture");
    }

    fn request_handoff() -> (AppLifecycle, RequestLease, RequestProducerHandoff) {
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit test request");
        let handoff = request.producer_handoff();
        (lifecycle, request, handoff)
    }
}
