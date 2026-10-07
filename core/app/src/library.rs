//! Library admission and physical-root lifetimes.
//!
//! Lock order is a library pin, sorted target leases, sorted shared-artifact
//! leases, then short metadata/file locks. A pin is retained by every escaped
//! file capability; stopping a request never revokes an accepted operation.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axial_fs::{
    AbsoluteDirectoryOutsideRootAdmission, AdmittedRootSession,
    AdmittedRootSessionAcquireObligation, AdmittedRootSessionAcquireOutcome, Directory,
    RootRevokeOutcome, RootSession, RootSessionAcquireObligation, RootSessionAcquireOutcome,
    RootSessionError,
};
use axial_minecraft::managed_path::{ManagedLibraryOperation, ManagedLibraryRoot};
use serde::Deserialize;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::files::ScopedDirectory;

const STARTUP_SELECTION_FILE: &str = "library.json";
const MAX_STARTUP_SELECTION_BYTES: u64 = 16 << 10;

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum StartupSelection {
    Managed {},
    Existing { library_id: Uuid, path: PathBuf },
}

impl StartupSelection {
    fn read(pin: &ApplicationRootPin) -> Result<Self, LibraryError> {
        let name = axial_fs::LeafName::new(STARTUP_SELECTION_FILE).expect("fixed selection file");
        let file = match pin.directory()?.open_file(&name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                pin.revalidate()?;
                return Ok(Self::Managed {});
            }
            Err(error) => return Err(error.into()),
        };
        let revision = file.revision()?;
        if revision.size() > MAX_STARTUP_SELECTION_BYTES {
            return Err(LibraryError::InvalidSelection("file exceeds 16 KiB"));
        }
        file.validate_revision(&revision)?;
        let bytes = file.read_bounded(MAX_STARTUP_SELECTION_BYTES)?;
        file.validate_revision(&revision)?;
        pin.revalidate()?;
        serde_json::from_slice(&bytes)
            .map_err(|_| LibraryError::InvalidSelection("invalid JSON or selection fields"))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GenerationId(u64);

impl GenerationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Metadata identity, never a substitute for an admitted filesystem capability.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LibraryId(Uuid);

impl LibraryId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Default for LibraryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for LibraryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibraryMode {
    Managed,
    Existing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionState {
    Open,
    Changing,
    Closed,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationSnapshot {
    pub generation: GenerationId,
    pub library_id: LibraryId,
    pub mode: LibraryMode,
    pub pins: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibrarySnapshot {
    pub admission: AdmissionState,
    pub current: Option<GenerationSnapshot>,
    pub retiring: Vec<GenerationSnapshot>,
    pub unresolved_admission: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("library admission is closed")]
    Closed,
    #[error("library selection is changing")]
    Changing,
    #[error("the previous library still has retained work or unsettled effects")]
    RetirementPending,
    #[error("library authority is unavailable")]
    Unavailable,
    #[error("saved library selection is invalid: {0}")]
    InvalidSelection(&'static str),
    #[error("library generation changed")]
    StaleGeneration,
    #[error("library generation limit reached")]
    GenerationExhausted,
    #[error("an external library must be outside the application root")]
    InsideApplicationRoot,
    #[error("external library acquisition needs explicit reconciliation")]
    AdmissionUnresolved,
    #[error("a previous game process has not been proven settled")]
    UnsettledLaunch,
    #[error("library pins did not drain before the deadline")]
    DrainTimeout,
    #[error("filesystem authority check failed")]
    Files(#[source] io::Error),
    #[error("external root session could not be acquired")]
    Root(#[source] RootSessionError),
}

impl From<io::Error> for LibraryError {
    fn from(error: io::Error) -> Self {
        Self::Files(error)
    }
}

#[must_use = "root acquisition obligations must be reconciled or preserved"]
#[derive(Debug)]
pub enum LibraryOpenOutcome {
    Ready(LibraryLifecycle),
    NoEffect(RootSessionError),
    Unresolved(RootSessionAcquireObligation),
}

#[derive(Clone)]
pub struct LibraryLifecycle {
    inner: Arc<Inner>,
}

struct Inner {
    application: Arc<ApplicationRoot>,
    state: Mutex<State>,
    changed: Arc<Notify>,
}

struct ApplicationRoot {
    pins: AtomicUsize,
    projection: Option<PathBuf>,
    changed: Arc<Notify>,
    managed: Arc<Mutex<Option<ManagedLibraryRoot>>>,
    retirement: Mutex<Option<RootRevokeOutcome>>,
    session: Mutex<Option<RootSession>>,
}

struct State {
    admission: AdmissionState,
    interrupted_launch: bool,
    revision: u64,
    current: Option<Arc<Generation>>,
    retiring: Vec<Arc<Generation>>,
    unresolved_admission: Option<AdmittedRootSessionAcquireObligation>,
}

struct Generation {
    id: GenerationId,
    library_id: LibraryId,
    mode: LibraryMode,
    pins: AtomicUsize,
    changed: Arc<Notify>,
    managed: Arc<Mutex<Option<ManagedLibraryRoot>>>,
    projection: Option<PathBuf>,
    root: GenerationRoot,
}

enum GenerationRoot {
    Managed(Arc<ApplicationRoot>),
    Existing(Mutex<ExternalState>),
}

enum ExternalState {
    Live(AdmittedRootSession),
    Retiring(RootRevokeOutcome),
    Retired,
}

/// An accepted operation's exact generation. Cloning retains the same root.
pub struct GenerationPin {
    generation: Arc<Generation>,
}

/// The application profile lifetime is independent of the selected payload library.
#[derive(Clone)]
pub struct ApplicationRootPin {
    inner: Arc<ApplicationPinInner>,
}

struct ApplicationPinInner {
    application: Arc<ApplicationRoot>,
}

/// One native dialog/drop selection. It grants only one bounded read of the
/// captured file revision; no path or filesystem capability is exposed.
pub struct NativeFileAdmission {
    file: axial_fs::FileCapability,
    revision: axial_fs::FileRevision,
    max_bytes: u64,
    pin: ApplicationRootPin,
}

impl NativeFileAdmission {
    pub fn read(self) -> io::Result<Vec<u8>> {
        self.pin.revalidate()?;
        self.file.validate_revision(&self.revision)?;
        let bytes = self.file.read_bounded(self.max_bytes)?;
        self.file.validate_revision(&self.revision)?;
        self.pin.revalidate()?;
        Ok(bytes)
    }
}

impl ApplicationRootPin {
    fn retain(application: Arc<ApplicationRoot>) -> Self {
        application
            .pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .expect("application pin count overflow");
        Self {
            inner: Arc::new(ApplicationPinInner { application }),
        }
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.directory().map(|_| ())
    }

    /// The native shell calls this synchronously when it receives an OS file
    /// selection, before dispatching the retained read to a blocking task.
    pub fn admit_native_file(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> io::Result<NativeFileAdmission> {
        if !path.is_absolute() || max_bytes == 0 || max_bytes > (32 << 20) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native file selection or bound is invalid",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "native file has no parent")
        })?;
        let name = axial_fs::LeafName::new(path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "native file has no name")
        })?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "native file name is invalid"))?;
        let session = lock(&self.inner.application.session);
        let session = session.as_ref().ok_or_else(closed_authority)?;
        session.validate_reset_preflight()?;
        let file = session.admit_absolute_directory(parent)?.open_file(&name)?;
        let revision = file.revision()?;
        if revision.size() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "native file exceeds its byte bound",
            ));
        }
        Ok(NativeFileAdmission {
            file,
            revision,
            max_bytes,
            pin: self.clone(),
        })
    }

    pub(crate) fn directory(&self) -> io::Result<Directory> {
        let session = lock(&self.inner.application.session);
        let session = session.as_ref().ok_or_else(closed_authority)?;
        session.validate_reset_preflight()?;
        session.root()
    }

    pub fn read_projection(&self) -> io::Result<PathBuf> {
        let path = self.inner.application.projection.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "application root projection is unavailable",
            )
        })?;
        self.directory()?.validate_absolute_projection(&path)?;
        Ok(path)
    }

    pub(crate) fn runtime_cache(
        &self,
        directory: Directory,
    ) -> io::Result<axial_minecraft::runtime::ManagedRuntimeCache> {
        let path = directory.project_from(&self.directory()?, &self.read_projection()?)?;
        axial_minecraft::runtime::ManagedRuntimeCache::from_directory_retaining(
            directory,
            path,
            Arc::new(self.clone()),
        )
    }
}

impl Drop for ApplicationPinInner {
    fn drop(&mut self) {
        self.application.pins.fetch_sub(1, Ordering::AcqRel);
        self.application.changed.notify_waiters();
    }
}

impl Clone for GenerationPin {
    fn clone(&self) -> Self {
        Self::retain(Arc::clone(&self.generation))
    }
}

impl fmt::Debug for GenerationPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerationPin")
            .field("generation", &self.generation.id)
            .field("library_id", &self.generation.library_id)
            .finish_non_exhaustive()
    }
}

impl GenerationPin {
    fn retain(generation: Arc<Generation>) -> Self {
        // Safe code cannot own usize::MAX live clones. Refuse overflow rather
        // than ever allowing the counter to wrap and grant premature reset.
        generation
            .pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .expect("library pin count overflow");
        Self { generation }
    }

    pub fn generation(&self) -> GenerationId {
        self.generation.id
    }

    pub fn library_id(&self) -> LibraryId {
        self.generation.library_id
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.generation.directory().map(|_| ())
    }

    pub(crate) fn directory(&self) -> io::Result<Directory> {
        self.generation.directory()
    }

    pub fn files(&self) -> io::Result<ScopedDirectory> {
        ScopedDirectory::from_admitted(self.generation.directory()?, self.clone())
    }

    pub fn read_projection(&self) -> io::Result<PathBuf> {
        let path = self.generation.projection.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "library root projection is unavailable",
            )
        })?;
        self.generation
            .directory()?
            .validate_absolute_projection(&path)?;
        Ok(path)
    }

    pub fn managed_library(&self) -> io::Result<ManagedLibraryOperation> {
        self.revalidate()?;
        let mut managed = lock(&self.generation.managed);
        if managed.is_none() {
            *managed = Some(ManagedLibraryRoot::from_directory(
                self.generation.directory()?,
            )?);
        }
        managed
            .as_ref()
            .expect("initialized managed library")
            .try_acquire_retaining(Arc::new(self.clone()))
    }
}

impl Drop for GenerationPin {
    fn drop(&mut self) {
        let previous = self.generation.pins.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        if previous == 1 {
            self.generation.changed.notify_waiters();
        }
    }
}

impl Generation {
    fn directory(&self) -> io::Result<Directory> {
        match &self.root {
            GenerationRoot::Managed(application) => {
                let session = lock(&application.session);
                let session = session.as_ref().ok_or_else(closed_authority)?;
                // Retained handles alone do not establish that the configured
                // root still names the admitted object.
                session.validate_reset_preflight()?;
                session.root()
            }
            GenerationRoot::Existing(external) => match &*lock(external) {
                ExternalState::Live(session) => {
                    let path = self.projection.as_ref().ok_or_else(closed_authority)?;
                    let directory = session.admit_absolute_directory(path)?;
                    if !directory
                        .identity()?
                        .same_filesystem_object(session.identity())
                    {
                        return Err(closed_authority());
                    }
                    Ok(directory)
                }
                _ => Err(closed_authority()),
            },
        }
    }

    fn snapshot(&self) -> GenerationSnapshot {
        GenerationSnapshot {
            generation: self.id,
            library_id: self.library_id,
            mode: self.mode,
            pins: self.pins.load(Ordering::Acquire),
        }
    }

    fn retire(&self) -> bool {
        if self.pins.load(Ordering::Acquire) != 0 {
            return false;
        }
        let GenerationRoot::Existing(external) = &self.root else {
            // The application root has its own lifetime and is drained after
            // every library generation during shutdown/reset.
            return true;
        };
        let mut managed = lock(&self.managed);
        if managed.as_ref().is_some_and(|root| root.settle().is_err()) {
            return false;
        }
        drop(managed.take());
        drop(managed);
        let mut external = lock(external);
        let old = std::mem::replace(&mut *external, ExternalState::Retired);
        let outcome = match old {
            ExternalState::Live(session) => session.revoke(),
            ExternalState::Retiring(RootRevokeOutcome::Pending(drain)) => drain.try_settle(),
            ExternalState::Retiring(RootRevokeOutcome::Refused(failure)) => failure.retry(),
            ExternalState::Retiring(RootRevokeOutcome::Failed(failure)) => failure.retry(),
            ExternalState::Retiring(outcome) => outcome,
            ExternalState::Retired => return true,
        };
        if matches!(outcome, RootRevokeOutcome::Revoked) {
            true
        } else {
            // Recovery is a domain decision. Never remove/restore an abandoned
            // effect merely because its last Rust wrapper was dropped.
            *external = ExternalState::Retiring(outcome);
            false
        }
    }
}

impl fmt::Debug for LibraryLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LibraryLifecycle")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl LibraryLifecycle {
    /// Open an isolated application root. Production composition should use
    /// `from_root_session` with the identity loaded from library metadata.
    pub fn open(path: &Path) -> LibraryOpenOutcome {
        Self::open_with_id(path, LibraryId::new())
    }

    pub fn open_with_id(path: &Path, library_id: LibraryId) -> LibraryOpenOutcome {
        match RootSession::acquire(path) {
            RootSessionAcquireOutcome::Acquired(session) => LibraryOpenOutcome::Ready(
                Self::construct(session, library_id, Some(path.to_path_buf())),
            ),
            RootSessionAcquireOutcome::NoEffect(error) => LibraryOpenOutcome::NoEffect(error),
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                LibraryOpenOutcome::Unresolved(obligation)
            }
        }
    }

    pub fn from_root_session(session: RootSession, managed_library_id: LibraryId) -> Self {
        Self::construct(session, managed_library_id, None)
    }

    pub fn from_root_session_at(
        session: RootSession,
        managed_library_id: LibraryId,
        path: PathBuf,
    ) -> io::Result<Self> {
        session.root()?.validate_absolute_projection(&path)?;
        Ok(Self::construct(session, managed_library_id, Some(path)))
    }

    fn construct(
        session: RootSession,
        managed_library_id: LibraryId,
        projection: Option<PathBuf>,
    ) -> Self {
        let changed = Arc::new(Notify::new());
        let application = Arc::new(ApplicationRoot {
            session: Mutex::new(Some(session)),
            pins: AtomicUsize::new(0),
            projection: projection.clone(),
            changed: Arc::clone(&changed),
            managed: Arc::new(Mutex::new(None)),
            retirement: Mutex::new(None),
        });
        let current = Arc::new(Generation {
            id: GenerationId(1),
            library_id: managed_library_id,
            mode: LibraryMode::Managed,
            root: GenerationRoot::Managed(Arc::clone(&application)),
            pins: AtomicUsize::new(0),
            changed: Arc::clone(&changed),
            managed: Arc::clone(&application.managed),
            projection,
        });
        Self {
            inner: Arc::new(Inner {
                application,
                state: Mutex::new(State {
                    admission: AdmissionState::Open,
                    interrupted_launch: false,
                    revision: 1,
                    current: Some(current),
                    retiring: Vec::new(),
                    unresolved_admission: None,
                }),
                changed,
            }),
        }
    }

    /// Restore the current application's private selection before constructing
    /// library consumers or replaying their recovery. An unavailable external
    /// root leaves application services usable without granting library access.
    pub fn restore_startup_selection(
        &self,
        managed_library_id: LibraryId,
    ) -> Result<(), LibraryError> {
        let mut change = self.begin_switch()?;
        let revision = change.revision;
        {
            let mut state = lock(&self.inner.state);
            if state.admission != AdmissionState::Changing
                || state.revision.checked_add(1) != Some(revision)
            {
                return Err(LibraryError::StaleGeneration);
            }
            if let Some(current) = state.current.take() {
                state.retiring.push(current);
            }
            change.previous_admission = AdmissionState::Unavailable;
        }
        if managed_library_id.0.is_nil() {
            return Err(LibraryError::InvalidSelection("managed identity is nil"));
        }
        let selection = StartupSelection::read(&change._application_pin)?;
        let external = matches!(selection, StartupSelection::Existing { .. });
        let prepared = match selection {
            StartupSelection::Managed {} => change.prepare_managed(managed_library_id),
            StartupSelection::Existing { library_id, path } => {
                if library_id.is_nil() || library_id == managed_library_id.0 {
                    return Err(LibraryError::InvalidSelection(
                        "external identity must be nonnil and distinct from the profile",
                    ));
                }
                if !path.is_absolute()
                    || path.as_os_str().as_encoded_bytes().contains(&0)
                    || path.components().any(|component| {
                        matches!(
                            component,
                            std::path::Component::CurDir | std::path::Component::ParentDir
                        )
                    })
                {
                    return Err(LibraryError::InvalidSelection(
                        "external path must be absolute without relative components",
                    ));
                }
                change.prepare_existing(&path, LibraryId(library_id))
            }
        };
        let result = match prepared {
            Ok(()) => change.commit_after_persistence().map(|_| ()),
            Err(error) => {
                drop(change);
                Err(error)
            }
        };
        if let Err(error) = result {
            let unavailable = match &error {
                LibraryError::Files(error) => error.kind() != io::ErrorKind::InvalidInput,
                LibraryError::Root(_) => true,
                _ => false,
            };
            if !external || !unavailable {
                return Err(error);
            }
            let mut state = lock(&self.inner.state);
            if state.admission != AdmissionState::Unavailable
                || (state.revision != revision && state.revision.checked_add(1) != Some(revision))
            {
                return Err(error);
            }
            if let Some(current) = state.current.take() {
                state.retiring.push(current);
            }
        }
        if !self.collect_retired() {
            return Err(LibraryError::RetirementPending);
        }
        Ok(())
    }

    /// Admission and pin creation linearize under the same short mutex.
    pub fn admit(&self) -> Result<GenerationPin, LibraryError> {
        let pin = {
            let state = lock(&self.inner.state);
            match state.admission {
                AdmissionState::Open => {}
                AdmissionState::Changing => return Err(LibraryError::Changing),
                AdmissionState::Closed => return Err(LibraryError::Closed),
                AdmissionState::Unavailable => return Err(LibraryError::Unavailable),
            }
            GenerationPin::retain(Arc::clone(
                state.current.as_ref().ok_or(LibraryError::Unavailable)?,
            ))
        };
        pin.revalidate()?;
        Ok(pin)
    }

    pub fn admit_application_root(&self) -> Result<ApplicationRootPin, LibraryError> {
        let state = lock(&self.inner.state);
        if state.admission == AdmissionState::Closed {
            return Err(LibraryError::Closed);
        }
        let pin = ApplicationRootPin::retain(Arc::clone(&self.inner.application));
        drop(state);
        pin.revalidate()?;
        Ok(pin)
    }

    /// Construct once in composition and share clones. Cache descendants retain
    /// application admission even while the selected external library changes.
    pub fn runtime_cache(
        &self,
    ) -> Result<axial_minecraft::runtime::ManagedRuntimeCache, LibraryError> {
        let pin = self.admit_application_root()?;
        let directory = {
            let mut managed = lock(&self.inner.application.managed);
            if managed.is_none() {
                *managed = Some(ManagedLibraryRoot::from_directory(pin.directory()?)?);
            }
            managed
                .as_ref()
                .expect("initialized application files")
                .prepare_runtime_directory()?
        };
        Ok(pin.runtime_cache(directory)?)
    }

    pub fn snapshot(&self) -> LibrarySnapshot {
        let state = lock(&self.inner.state);
        LibrarySnapshot {
            admission: state.admission,
            current: state.current.as_ref().map(|value| value.snapshot()),
            retiring: state
                .retiring
                .iter()
                .map(|value| value.snapshot())
                .collect(),
            unresolved_admission: state.unresolved_admission.is_some(),
        }
    }

    pub fn begin_switch(&self) -> Result<LibrarySwitch, LibraryError> {
        self.ensure_no_interrupted_launch()?;
        self.collect_retired();
        let mut state = lock(&self.inner.state);
        if state.interrupted_launch {
            return Err(LibraryError::UnsettledLaunch);
        }
        match state.admission {
            AdmissionState::Open | AdmissionState::Unavailable => {}
            AdmissionState::Changing => return Err(LibraryError::Changing),
            AdmissionState::Closed => return Err(LibraryError::Closed),
        }
        if !state.retiring.is_empty() {
            return Err(LibraryError::RetirementPending);
        }
        if state.unresolved_admission.is_some() {
            return Err(LibraryError::AdmissionUnresolved);
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(LibraryError::GenerationExhausted)?;
        let previous_admission = state.admission;
        state.admission = AdmissionState::Changing;
        Ok(LibrarySwitch {
            owner: self.clone(),
            revision,
            previous_admission,
            prepared: None,
            finished: false,
            _application_pin: ApplicationRootPin::retain(Arc::clone(&self.inner.application)),
        })
    }

    /// Release only the root-acquisition effects of a selection that never
    /// committed. Native cleanup retains the exact obligation on ambiguity.
    pub fn cleanup_failed_admission(&self) -> Result<(), LibraryError> {
        let mut state = lock(&self.inner.state);
        if let Some(obligation) = state.unresolved_admission.take() {
            if let Err(obligation) = obligation.cleanup() {
                state.unresolved_admission = Some(obligation);
                return Err(LibraryError::AdmissionUnresolved);
            }
        }
        Ok(())
    }

    /// Irreversible admission closure. Existing pins remain usable until their
    /// operations settle; an updater/reset must not force their revocation.
    pub fn close_admission(&self) {
        let mut state = lock(&self.inner.state);
        state.admission = AdmissionState::Closed;
        if let Some(current) = state.current.take() {
            state.retiring.push(current);
        }
        self.inner.changed.notify_waiters();
    }

    /// Makes progress only on generations with no escaped guards or receipts.
    /// A false result includes retained native filesystem recovery obligations.
    pub fn collect_retired(&self) -> bool {
        let candidates = {
            let state = lock(&self.inner.state);
            if state.interrupted_launch {
                return false;
            }
            state.retiring.clone()
        };
        let retired: Vec<_> = candidates
            .iter()
            .filter(|generation| generation.retire())
            .cloned()
            .collect();
        let mut state = lock(&self.inner.state);
        state
            .retiring
            .retain(|generation| !retired.iter().any(|old| Arc::ptr_eq(old, generation)));
        state.retiring.is_empty()
    }

    pub async fn wait_for_pins(&self, timeout: Duration) -> Result<(), LibraryError> {
        tokio::time::timeout(timeout, async {
            loop {
                // Register before observing counters, preventing lost wakeups.
                let changed = self.inner.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let pins = {
                    let state = lock(&self.inner.state);
                    state
                        .current
                        .iter()
                        .chain(state.retiring.iter())
                        .any(|generation| generation.pins.load(Ordering::Acquire) != 0)
                };
                if !pins && self.inner.application.pins.load(Ordering::Acquire) == 0 {
                    return;
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| LibraryError::DrainTimeout)
    }

    /// Called only after application-owned tasks and metadata connections have
    /// joined/closed. The returned native outcome owns reset settlement; this
    /// function neither clears files nor guesses how to settle recovery.
    pub fn begin_root_reset(&self) -> Result<axial_fs::ResetStartOutcome, LibraryError> {
        Ok(self.take_reset_session()?.begin_reset())
    }

    /// A replayed reset fence blocks metadata/services but grants no deletion
    /// authority. Native startup must obtain a fresh explicit confirmation.
    pub fn interrupted_root_reset(&self) -> Result<bool, LibraryError> {
        let session = lock(&self.inner.application.session);
        session
            .as_ref()
            .ok_or(LibraryError::Closed)?
            .interrupted_reset(
                &axial_fs::LeafName::new(".axial-rewrite-profile").expect("fixed profile marker"),
            )
            .map_err(LibraryError::Files)
    }

    /// Transfer only after every application owner drains. Any failure leaves
    /// the exact session here so the caller can retain and retry preservation.
    pub fn take_reset_session(&self) -> Result<RootSession, LibraryError> {
        let state = lock(&self.inner.state);
        if state.interrupted_launch {
            return Err(LibraryError::UnsettledLaunch);
        }
        if state.admission != AdmissionState::Closed {
            return Err(LibraryError::Closed);
        }
        if state.unresolved_admission.is_some() {
            return Err(LibraryError::AdmissionUnresolved);
        }
        drop(state);
        if !self.collect_retired() {
            return Err(LibraryError::RetirementPending);
        }
        if self.inner.application.pins.load(Ordering::Acquire) != 0 {
            return Err(LibraryError::RetirementPending);
        }
        self.release_managed_application()?;
        let session = lock(&self.inner.application.session)
            .take()
            .ok_or(LibraryError::Closed)?;
        Ok(session)
    }

    pub fn revoke_application_root(&self) -> Result<RootRevokeOutcome, LibraryError> {
        self.ensure_no_interrupted_launch()?;
        if lock(&self.inner.state).admission != AdmissionState::Closed {
            return Err(LibraryError::Closed);
        }
        if !self.collect_retired() {
            return Err(LibraryError::RetirementPending);
        }
        if self.inner.application.pins.load(Ordering::Acquire) != 0 {
            return Err(LibraryError::RetirementPending);
        }
        self.release_managed_application()?;
        if lock(&self.inner.state).unresolved_admission.is_some() {
            return Err(LibraryError::AdmissionUnresolved);
        }
        let session = lock(&self.inner.application.session)
            .take()
            .ok_or(LibraryError::Closed)?;
        Ok(session.revoke())
    }

    fn release_managed_application(&self) -> Result<(), LibraryError> {
        let mut managed = lock(&self.inner.application.managed);
        if let Some(root) = managed.as_ref() {
            root.settle()?;
        }
        drop(managed.take());
        Ok(())
    }

    // Restored before feature recovery. Dropping a request or status owner must
    // never release this fence; only exact process settlement could clear it.
    pub(crate) fn fence_interrupted_launch(&self) {
        lock(&self.inner.state).interrupted_launch = true;
    }

    pub fn ensure_no_interrupted_launch(&self) -> Result<(), LibraryError> {
        if lock(&self.inner.state).interrupted_launch {
            return Err(LibraryError::UnsettledLaunch);
        }
        Ok(())
    }

    /// Fence new library admission and settle its existing managed effects
    /// without revoking long-lived cache/skin capabilities. The caller first
    /// joins every producer, including read requests which update caches, and
    /// settles the shared runtime cache. Failure leaves this owner intact.
    pub fn try_preserve(&self) -> Result<(), LibraryError> {
        self.close_admission();
        let mut retirement = lock(&self.inner.application.retirement);
        if let Some(previous) = retirement.take() {
            let outcome = match previous {
                RootRevokeOutcome::Pending(drain) => drain.try_settle(),
                RootRevokeOutcome::Refused(failure) => failure.retry(),
                RootRevokeOutcome::Failed(failure) => failure.retry(),
                outcome => outcome,
            };
            if matches!(outcome, RootRevokeOutcome::Revoked) {
                return Ok(());
            }
            *retirement = Some(outcome);
            return Err(LibraryError::RetirementPending);
        }
        let generations = {
            let mut state = lock(&self.inner.state);
            if let Some(obligation) = state.unresolved_admission.take() {
                if let Err(obligation) = obligation.acknowledge_preserved() {
                    state.unresolved_admission = Some(obligation);
                    return Err(LibraryError::AdmissionUnresolved);
                }
            }
            state.retiring.clone()
        };
        let mut roots = vec![Arc::clone(&self.inner.application.managed)];
        for generation in &generations {
            if !roots
                .iter()
                .any(|root| Arc::ptr_eq(root, &generation.managed))
            {
                roots.push(Arc::clone(&generation.managed));
            }
        }
        for root in roots {
            if let Some(root) = lock(&root).as_ref() {
                root.settle()?;
            }
        }
        // Startup failure may preserve this process's files and exit without
        // asserting that an older game process stopped. Reset stays fenced.
        if lock(&self.inner.state).interrupted_launch {
            return Ok(());
        }
        // A retained generation is permitted here: its capabilities still own
        // the root. Only reset/revocation requires every pin to disappear.
        let drained = self.collect_retired();
        if lock(&self.inner.state)
            .retiring
            .iter()
            .any(|generation| generation.pins.load(Ordering::Acquire) == 0)
        {
            return Err(LibraryError::RetirementPending);
        }
        if drained
            && self.inner.application.pins.load(Ordering::Acquire) == 0
            && lock(&self.inner.application.session).is_some()
        {
            let outcome = self.revoke_application_root()?;
            if !matches!(outcome, RootRevokeOutcome::Revoked) {
                *retirement = Some(outcome);
                return Err(LibraryError::RetirementPending);
            }
        }
        Ok(())
    }
}

#[must_use = "prepared selection must be committed after persistence or explicitly abandoned"]
pub struct LibrarySwitch {
    owner: LibraryLifecycle,
    revision: u64,
    previous_admission: AdmissionState,
    prepared: Option<Arc<Generation>>,
    finished: bool,
    _application_pin: ApplicationRootPin,
}

impl LibrarySwitch {
    pub fn prepare_managed(&mut self, library_id: LibraryId) -> Result<(), LibraryError> {
        self.ensure_current()?;
        if self.prepared.is_some() {
            return Err(LibraryError::Changing);
        }
        let generation = Arc::new(Generation {
            id: GenerationId(self.revision),
            library_id,
            mode: LibraryMode::Managed,
            root: GenerationRoot::Managed(Arc::clone(&self.owner.inner.application)),
            pins: AtomicUsize::new(0),
            changed: Arc::clone(&self.owner.inner.changed),
            managed: Arc::clone(&self.owner.inner.application.managed),
            projection: self.owner.inner.application.projection.clone(),
        });
        self.prepared = Some(generation);
        self.prepared
            .as_ref()
            .expect("prepared generation")
            .directory()?;
        Ok(())
    }

    pub fn prepare_existing(
        &mut self,
        path: &Path,
        library_id: LibraryId,
    ) -> Result<(), LibraryError> {
        self.ensure_current()?;
        if self.prepared.is_some() {
            return Err(LibraryError::Changing);
        }
        let admission = {
            let session = lock(&self.owner.inner.application.session);
            let session = session.as_ref().ok_or(LibraryError::Closed)?;
            match session.admit_absolute_directory_authority_outside_root(path) {
                AbsoluteDirectoryOutsideRootAdmission::Admitted(admission) => admission,
                AbsoluteDirectoryOutsideRootAdmission::InsideRoot => {
                    return Err(LibraryError::InsideApplicationRoot);
                }
                AbsoluteDirectoryOutsideRootAdmission::Unavailable(error) => {
                    return Err(error.into());
                }
            }
        };
        let session = match admission.acquire_root_session()? {
            AdmittedRootSessionAcquireOutcome::Acquired(session) => session,
            AdmittedRootSessionAcquireOutcome::NoEffect(error) => {
                return Err(LibraryError::Root(error));
            }
            AdmittedRootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                lock(&self.owner.inner.state).unresolved_admission = Some(obligation);
                return Err(LibraryError::AdmissionUnresolved);
            }
        };
        let generation = Arc::new(Generation {
            id: GenerationId(self.revision),
            library_id,
            mode: LibraryMode::Existing,
            root: GenerationRoot::Existing(Mutex::new(ExternalState::Live(session))),
            pins: AtomicUsize::new(0),
            changed: Arc::clone(&self.owner.inner.changed),
            managed: Arc::new(Mutex::new(None)),
            projection: Some(path.to_path_buf()),
        });
        self.prepared = Some(generation);
        self.prepared
            .as_ref()
            .expect("prepared generation")
            .directory()?;
        Ok(())
    }

    pub fn prepared(&self) -> Option<GenerationSnapshot> {
        self.prepared
            .as_ref()
            .map(|generation| generation.snapshot())
    }

    /// Call only after durable selection persistence. A changed physical root
    /// after persistence leaves admission unavailable, never silently reverting
    /// to a library that no longer matches the saved selection.
    pub fn commit_after_persistence(mut self) -> Result<GenerationId, LibraryError> {
        self.ensure_current()?;
        let validation = self
            .prepared
            .as_ref()
            .ok_or(LibraryError::Unavailable)?
            .directory();
        let mut state = lock(&self.owner.inner.state);
        if state.admission != AdmissionState::Changing
            || state.revision.checked_add(1) != Some(self.revision)
        {
            return Err(LibraryError::StaleGeneration);
        }
        let prepared = self.prepared.take().expect("validated prepared generation");
        if let Some(current) = state.current.take() {
            state.retiring.push(current);
        }
        state.revision = self.revision;
        state.current = Some(prepared);
        state.admission = if validation.is_ok() {
            AdmissionState::Open
        } else {
            AdmissionState::Unavailable
        };
        self.finished = true;
        drop(state);
        self.owner.inner.changed.notify_waiters();
        validation?;
        Ok(GenerationId(self.revision))
    }

    fn ensure_current(&self) -> Result<(), LibraryError> {
        let state = lock(&self.owner.inner.state);
        if state.admission == AdmissionState::Changing
            && state.revision.checked_add(1) == Some(self.revision)
        {
            Ok(())
        } else {
            Err(LibraryError::StaleGeneration)
        }
    }
}

impl Drop for LibrarySwitch {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = lock(&self.owner.inner.state);
        if let Some(prepared) = self.prepared.take() {
            state.retiring.push(prepared);
        }
        if state.admission == AdmissionState::Changing
            && state.revision.checked_add(1) == Some(self.revision)
        {
            state.admission = if state.unresolved_admission.is_some() {
                AdmissionState::Unavailable
            } else {
                self.previous_admission
            };
        }
        self.owner.inner.changed.notify_waiters();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("library lifecycle lock poisoned")
}

fn closed_authority() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "library root authority is closed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary() -> tempfile::TempDir {
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }

    fn open(path: &Path) -> LibraryLifecycle {
        match LibraryLifecycle::open(path) {
            LibraryOpenOutcome::Ready(owner) => owner,
            LibraryOpenOutcome::NoEffect(error) => panic!("fixture acquisition failed: {error}"),
            LibraryOpenOutcome::Unresolved(obligation) => {
                let result = obligation.acknowledge_preserved();
                assert!(result.is_ok(), "fixture acquisition could not be preserved");
                panic!("fixture acquisition was unresolved")
            }
        }
    }

    fn revoke(owner: &LibraryLifecycle) {
        owner.close_admission();
        assert!(matches!(
            owner.revoke_application_root().unwrap(),
            RootRevokeOutcome::Revoked
        ));
    }

    fn save_existing(application: &Path, external: &Path, library_id: LibraryId) {
        std::fs::write(
            application.join("library.json"),
            serde_json::to_vec(&serde_json::json!({
                "mode": "existing",
                "library_id": library_id.to_string(),
                "path": external,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn startup_absent_or_explicit_managed_selection_uses_the_profile_identity() {
        for selection in [None, Some(br#"{"mode":"managed"}"#.as_slice())] {
            let application = temporary();
            if let Some(selection) = selection {
                std::fs::write(application.path().join("library.json"), selection).unwrap();
            }
            let owner = open(application.path());
            let managed_id = owner.snapshot().current.unwrap().library_id;
            owner.restore_startup_selection(managed_id).unwrap();
            let current = owner.snapshot().current.unwrap();
            assert_eq!(current.mode, LibraryMode::Managed);
            assert_eq!(current.library_id, managed_id);
            assert_eq!(
                owner.admit().unwrap().read_projection().unwrap(),
                application.path()
            );
            revoke(&owner);
        }
    }

    #[test]
    fn startup_external_selection_keeps_identity_and_separate_application_root_on_reopen() {
        let application = temporary();
        let external = temporary();
        let external_id = LibraryId::new();
        save_existing(application.path(), external.path(), external_id);
        std::fs::write(external.path().join("user-file.txt"), b"untouched").unwrap();
        for _ in 0..2 {
            let owner = open(application.path());
            let managed_id = owner.snapshot().current.unwrap().library_id;
            owner.restore_startup_selection(managed_id).unwrap();
            let pin = owner.admit().unwrap();
            assert_eq!(pin.library_id(), external_id);
            assert_eq!(pin.read_projection().unwrap(), external.path());
            assert_eq!(
                owner.snapshot().current.unwrap().mode,
                LibraryMode::Existing
            );
            let app = owner.admit_application_root().unwrap();
            assert_eq!(app.read_projection().unwrap(), application.path());
            drop((pin, app));
            owner.try_preserve().unwrap();
            let independent = open(external.path());
            revoke(&independent);
        }
        assert_eq!(
            std::fs::read(external.path().join("user-file.txt")).unwrap(),
            b"untouched"
        );
    }

    #[test]
    fn startup_missing_external_root_is_unavailable_without_creation_or_managed_fallback() {
        let application = temporary();
        let external = temporary();
        let missing = external.path().join("absent").join("library");
        save_existing(application.path(), &missing, LibraryId::new());
        let owner = open(application.path());
        let managed_id = owner.snapshot().current.unwrap().library_id;
        owner.restore_startup_selection(managed_id).unwrap();
        assert_eq!(owner.snapshot().admission, AdmissionState::Unavailable);
        assert!(owner.snapshot().current.is_none());
        assert!(owner.snapshot().retiring.is_empty());
        assert!(matches!(owner.admit(), Err(LibraryError::Unavailable)));
        assert!(!external.path().join("absent").exists());
        let app = owner.admit_application_root().unwrap();
        app.revalidate().unwrap();
        let runtime = owner.runtime_cache().unwrap();
        runtime.settle().unwrap();
        drop((app, runtime));
        owner.try_preserve().unwrap();
        let reopened = open(application.path());
        revoke(&reopened);
    }

    #[test]
    fn startup_rejects_invalid_selection_without_fallback() {
        let application = temporary();
        let managed_id = LibraryId::new();
        let external_id = LibraryId::new();
        let existing = |library_id: String, path: serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({
                "mode": "existing", "library_id": library_id, "path": path,
            }))
            .unwrap()
        };
        let cases = [
            b"{".to_vec(),
            br#"{"mode":"other"}"#.to_vec(),
            br#"{"mode":"managed","path":"ignored"}"#.to_vec(),
            br#"{"mode":"existing"}"#.to_vec(),
            existing("invalid".into(), serde_json::json!(application.path())),
            existing(
                Uuid::nil().to_string(),
                serde_json::json!(application.path()),
            ),
            existing(
                managed_id.to_string(),
                serde_json::json!(application.path()),
            ),
            existing(external_id.to_string(), serde_json::json!(17)),
            existing(external_id.to_string(), serde_json::json!("/invalid\0path")),
            existing(
                external_id.to_string(),
                serde_json::json!("relative/library"),
            ),
            existing(
                external_id.to_string(),
                serde_json::json!(application.path().join("..")),
            ),
            vec![b' '; MAX_STARTUP_SELECTION_BYTES as usize + 1],
        ];
        for bytes in cases {
            std::fs::write(application.path().join("library.json"), bytes).unwrap();
            let owner = open(application.path());
            assert!(matches!(
                owner.restore_startup_selection(managed_id),
                Err(LibraryError::InvalidSelection(_))
            ));
            assert_eq!(owner.snapshot().admission, AdmissionState::Unavailable);
            assert!(owner.snapshot().current.is_none());
            assert!(matches!(owner.admit(), Err(LibraryError::Unavailable)));
            owner.try_preserve().unwrap();
        }
    }

    #[test]
    fn startup_selection_file_must_be_regular_and_unaliased() {
        for hard_link in [false, true] {
            let application = temporary();
            let path = application.path().join("library.json");
            if hard_link {
                let original = application.path().join("original.json");
                std::fs::write(&original, br#"{"mode":"managed"}"#).unwrap();
                std::fs::hard_link(original, &path).unwrap();
            } else {
                std::fs::create_dir(&path).unwrap();
            }
            let owner = open(application.path());
            let managed_id = owner.snapshot().current.unwrap().library_id;
            assert!(matches!(
                owner.restore_startup_selection(managed_id),
                Err(LibraryError::Files(_))
            ));
            assert!(owner.snapshot().current.is_none());
            owner.try_preserve().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_selection_file_rejects_symlinks_including_dangling_links() {
        for dangling in [false, true] {
            let application = temporary();
            let target = application.path().join("target.json");
            if !dangling {
                std::fs::write(&target, br#"{"mode":"managed"}"#).unwrap();
            }
            std::os::unix::fs::symlink(&target, application.path().join("library.json")).unwrap();
            let owner = open(application.path());
            let managed_id = owner.snapshot().current.unwrap().library_id;
            assert!(matches!(
                owner.restore_startup_selection(managed_id),
                Err(LibraryError::Files(_))
            ));
            assert!(owner.snapshot().current.is_none());
            owner.try_preserve().unwrap();
        }
    }

    #[test]
    fn startup_external_selection_rejects_application_root() {
        let application = temporary();
        save_existing(application.path(), application.path(), LibraryId::new());
        let owner = open(application.path());
        let managed_id = owner.snapshot().current.unwrap().library_id;
        assert!(matches!(
            owner.restore_startup_selection(managed_id),
            Err(LibraryError::InsideApplicationRoot)
        ));
        assert!(owner.snapshot().current.is_none());
        owner.try_preserve().unwrap();
    }

    #[test]
    fn startup_selection_retains_existing_pins_until_retirement() {
        let application = temporary();
        let external = temporary();
        save_existing(application.path(), external.path(), LibraryId::new());
        let owner = open(application.path());
        let pin = owner.admit().unwrap();
        assert!(matches!(
            owner.restore_startup_selection(pin.library_id()),
            Err(LibraryError::RetirementPending)
        ));
        pin.revalidate().unwrap();
        assert_eq!(owner.snapshot().retiring[0].generation, pin.generation());
        drop(pin);
        assert!(owner.collect_retired());
        owner.try_preserve().unwrap();
    }

    #[test]
    fn startup_external_selection_reset_clears_only_the_application_root() {
        let application = temporary();
        let external = temporary();
        save_existing(application.path(), external.path(), LibraryId::new());
        std::fs::write(external.path().join("user-file.txt"), b"untouched").unwrap();
        let owner = open(application.path());
        let managed_id = owner.snapshot().current.unwrap().library_id;
        owner.restore_startup_selection(managed_id).unwrap();
        owner.close_admission();
        let authority = match owner.begin_root_reset().unwrap() {
            axial_fs::ResetStartOutcome::Ready(authority) => authority,
            outcome => panic!("unexpected reset outcome: {outcome:?}"),
        };
        match authority.clear_root() {
            axial_fs::RootClearOutcome::Cleared(receipt) => assert!(receipt.release().is_ok()),
            outcome => panic!("unexpected clear outcome: {outcome:?}"),
        }
        assert!(!application.path().join("library.json").exists());
        assert_eq!(
            std::fs::read(external.path().join("user-file.txt")).unwrap(),
            b"untouched"
        );
        let reopened = open(external.path());
        revoke(&reopened);
    }

    #[test]
    fn switch_closes_admission_but_retains_old_pins() {
        let directory = temporary();
        let owner = open(directory.path());
        let pin = owner.admit().unwrap();
        let old_id = pin.generation();
        let escaped = pin.clone();
        let mut change = owner.begin_switch().unwrap();
        assert!(matches!(owner.admit(), Err(LibraryError::Changing)));
        change.prepare_managed(LibraryId::new()).unwrap();
        let new_id = change.commit_after_persistence().unwrap();
        assert_ne!(old_id, new_id);
        assert_eq!(escaped.generation(), old_id);
        escaped.revalidate().unwrap();
        assert_eq!(owner.snapshot().retiring[0].pins, 2);
        drop(pin);
        assert!(!owner.collect_retired());
        drop(escaped);
        assert!(owner.collect_retired());
        revoke(&owner);
    }

    #[test]
    fn abandoning_switch_reopens_the_unchanged_generation() {
        let directory = temporary();
        let owner = open(directory.path());
        let before = owner.snapshot().current.unwrap();
        drop(owner.begin_switch().unwrap());
        assert_eq!(owner.snapshot().current.unwrap(), before);
        assert!(owner.admit().is_ok());
        revoke(&owner);
    }

    #[test]
    fn closure_wins_over_late_switch_completion() {
        let directory = temporary();
        let owner = open(directory.path());
        let mut change = owner.begin_switch().unwrap();
        change.prepare_managed(LibraryId::new()).unwrap();
        owner.close_admission();
        assert!(matches!(
            change.commit_after_persistence(),
            Err(LibraryError::StaleGeneration)
        ));
        assert!(matches!(owner.admit(), Err(LibraryError::Closed)));
        revoke(&owner);
    }

    #[tokio::test]
    async fn reset_waits_for_escaped_pin_and_keeps_root_untouched() {
        let directory = temporary();
        std::fs::write(directory.path().join("valuable.txt"), b"kept until reset").unwrap();
        let owner = open(directory.path());
        let escaped = owner.admit().unwrap();
        owner.close_admission();
        assert!(matches!(
            owner.begin_root_reset(),
            Err(LibraryError::RetirementPending)
        ));
        assert!(matches!(
            owner.wait_for_pins(Duration::from_millis(1)).await,
            Err(LibraryError::DrainTimeout)
        ));
        assert_eq!(
            std::fs::read(directory.path().join("valuable.txt")).unwrap(),
            b"kept until reset"
        );
        escaped.revalidate().unwrap();
        drop(escaped);
        owner.wait_for_pins(Duration::from_secs(1)).await.unwrap();
        let authority = match owner.begin_root_reset().unwrap() {
            axial_fs::ResetStartOutcome::Ready(authority) => authority,
            outcome => panic!("unexpected reset outcome: {outcome:?}"),
        };
        match authority.clear_root() {
            axial_fs::RootClearOutcome::Cleared(receipt) => assert!(receipt.release().is_ok()),
            outcome => panic!("unexpected clear outcome: {outcome:?}"),
        }
        assert!(!directory.path().join("valuable.txt").exists());
    }

    #[test]
    fn existing_library_inside_application_root_is_rejected() {
        let directory = temporary();
        let owner = open(directory.path());
        let mut change = owner.begin_switch().unwrap();
        assert!(matches!(
            change.prepare_existing(directory.path(), LibraryId::new()),
            Err(LibraryError::InsideApplicationRoot)
        ));
        drop(change);
        revoke(&owner);
    }

    #[test]
    fn external_generation_retires_only_after_its_last_pin() {
        let application = temporary();
        let external = temporary();
        std::fs::write(external.path().join("user-file.txt"), b"untouched").unwrap();
        let owner = open(application.path());
        let mut change = owner.begin_switch().unwrap();
        change
            .prepare_existing(external.path(), LibraryId::new())
            .unwrap();
        change.commit_after_persistence().unwrap();
        assert!(owner.collect_retired());
        let pin = owner.admit().unwrap();
        let mut change = owner.begin_switch().unwrap();
        change.prepare_managed(LibraryId::new()).unwrap();
        change.commit_after_persistence().unwrap();
        assert!(!owner.collect_retired());
        pin.revalidate().unwrap();
        drop(pin);
        assert!(owner.collect_retired());
        assert_eq!(
            std::fs::read(external.path().join("user-file.txt")).unwrap(),
            b"untouched"
        );
        revoke(&owner);
    }

    #[test]
    fn abandoned_prepared_external_root_is_explicitly_retired() {
        let application = temporary();
        let external = temporary();
        let owner = open(application.path());
        let mut change = owner.begin_switch().unwrap();
        change
            .prepare_existing(external.path(), LibraryId::new())
            .unwrap();
        drop(change);
        assert_eq!(owner.snapshot().retiring.len(), 1);
        assert!(owner.collect_retired());
        let independent = open(external.path());
        revoke(&independent);
        revoke(&owner);
    }

    #[tokio::test]
    async fn escaped_managed_operation_blocks_reset_until_its_last_clone() {
        let application = temporary();
        let owner = open(application.path());
        let pin = owner.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let escaped = operation.clone();
        let witness = operation.witness();
        drop((operation, pin));
        owner.close_admission();
        assert!(matches!(
            owner.begin_root_reset(),
            Err(LibraryError::RetirementPending)
        ));
        escaped.revalidate().unwrap();
        drop(escaped);
        assert!(matches!(
            owner.wait_for_pins(Duration::from_millis(1)).await,
            Err(LibraryError::DrainTimeout)
        ));
        drop(witness);
        owner.wait_for_pins(Duration::from_secs(1)).await.unwrap();
        assert!(matches!(
            owner.revoke_application_root().unwrap(),
            RootRevokeOutcome::Revoked
        ));
    }

    #[test]
    fn application_cache_does_not_pin_a_retired_external_library() {
        let application = temporary();
        let external = temporary();
        std::fs::create_dir(application.path().join("runtime")).unwrap();
        let owner = open(application.path());
        let mut change = owner.begin_switch().unwrap();
        change
            .prepare_existing(external.path(), LibraryId::new())
            .unwrap();
        change.commit_after_persistence().unwrap();
        assert!(owner.collect_retired());
        let app_pin = owner.admit_application_root().unwrap();
        let runtime = app_pin
            .directory()
            .unwrap()
            .open_directory(&axial_fs::LeafName::new("runtime").unwrap())
            .unwrap();
        let cache = app_pin.runtime_cache(runtime).unwrap();
        let escaped = cache.clone();
        drop((cache, app_pin));
        let mut change = owner.begin_switch().unwrap();
        change.prepare_managed(LibraryId::new()).unwrap();
        change.commit_after_persistence().unwrap();
        assert!(owner.collect_retired());
        let independent = open(external.path());
        revoke(&independent);
        owner.close_admission();
        assert!(matches!(
            owner.begin_root_reset(),
            Err(LibraryError::RetirementPending)
        ));
        drop(escaped);
        assert!(matches!(
            owner.revoke_application_root().unwrap(),
            RootRevokeOutcome::Revoked
        ));
    }

    #[test]
    fn preserved_startup_closes_and_releases_a_clean_root() {
        let application = temporary();
        let owner = open(application.path());
        owner.runtime_cache().unwrap().settle().unwrap();
        owner.try_preserve().unwrap();
        owner.try_preserve().unwrap();
        assert!(matches!(owner.admit(), Err(LibraryError::Closed)));
        let reopened = open(application.path());
        revoke(&reopened);
    }

    #[test]
    fn interrupted_launch_fences_switch_reset_and_revoke_but_allows_preservation() {
        let application = temporary();
        std::fs::write(application.path().join("valuable.txt"), b"preserved").unwrap();
        let owner = open(application.path());
        let cache = owner.runtime_cache().unwrap();
        owner.fence_interrupted_launch();
        drop(owner.clone());
        assert!(matches!(
            owner.begin_switch(),
            Err(LibraryError::UnsettledLaunch)
        ));
        cache.settle().unwrap();
        drop(cache);
        owner.try_preserve().unwrap();
        owner.try_preserve().unwrap();
        assert!(!owner.collect_retired());
        assert!(matches!(
            owner.take_reset_session(),
            Err(LibraryError::UnsettledLaunch)
        ));
        assert!(matches!(
            owner.revoke_application_root(),
            Err(LibraryError::UnsettledLaunch)
        ));
        assert!(matches!(
            owner.ensure_no_interrupted_launch(),
            Err(LibraryError::UnsettledLaunch)
        ));
        drop(owner);
        assert_eq!(
            std::fs::read(application.path().join("valuable.txt")).unwrap(),
            b"preserved"
        );
        let reopened = open(application.path());
        revoke(&reopened);
    }

    #[test]
    fn shutdown_settlement_keeps_long_lived_cache_pins_valid() {
        let application = temporary();
        let owner = open(application.path());
        let cache = owner.runtime_cache().unwrap();
        cache.settle().unwrap();
        owner.try_preserve().unwrap();
        assert!(matches!(
            owner.begin_root_reset(),
            Err(LibraryError::RetirementPending)
        ));
        cache.settle().unwrap();
        drop(cache);
        owner.try_preserve().unwrap();
        let reopened = open(application.path());
        revoke(&reopened);
    }

    #[test]
    fn native_selection_captures_revision_before_the_deferred_read() {
        let application = temporary();
        let selection = temporary();
        let path = selection.path().join("skin.png");
        std::fs::write(&path, b"selected bytes").unwrap();
        let owner = open(application.path());
        let pin = owner.admit_application_root().unwrap();
        let admission = pin.admit_native_file(&path, 64).unwrap();
        drop(pin);
        owner.close_admission();
        assert!(matches!(
            owner.begin_root_reset(),
            Err(LibraryError::RetirementPending)
        ));
        std::fs::rename(&path, selection.path().join("original.png")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert!(admission.read().is_err());
        owner.try_preserve().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }
}
