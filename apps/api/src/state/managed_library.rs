use axial_config::{
    AppConfig, AppPaths, AppRootSession, ExistingLibraryDirectoryAdmission,
};
use axial_fs::AdmittedAbsoluteDirectory;
use axial_minecraft::managed_path::{
    ManagedLibraryAdmissionRebindFailure, ManagedLibraryBinding, ManagedLibraryOperation,
    ManagedLibraryRetirement, ManagedLibraryRetirementBinding, ManagedLibraryRoot,
    ManagedLibraryWitness, PreparedManagedLibraryAdmissionRebind,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const LIBRARY_STATE_LOCK_INVARIANT: &str = "managed library state lock poisoned";

#[derive(Clone)]
pub(crate) struct ManagedLibraryOwner {
    inner: Arc<ManagedLibraryOwnerInner>,
}

struct ManagedLibraryOwnerInner {
    root_session: Arc<AppRootSession>,
    paths: AppPaths,
    rotation: Arc<AsyncMutex<()>>,
    state: Mutex<ManagedLibraryState>,
}

struct ManagedLibraryState {
    closed: bool,
    revision: u64,
    publishing_revision: Option<u64>,
    current: Option<CurrentLibraryGeneration>,
    retiring: Option<RetiringLibraryGeneration>,
    degraded: Option<ManagedLibraryDegradedReason>,
}

struct CurrentLibraryGeneration {
    id: LibraryGenerationId,
    fingerprint: LibraryFingerprint,
    binding: ManagedLibraryBinding,
    root: ManagedLibraryRoot,
}

struct RetiringLibraryGeneration {
    id: LibraryGenerationId,
    retirement: Arc<ManagedLibraryRetirement>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LibraryGenerationId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryMode {
    Managed,
    Existing,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct LibraryFingerprint {
    mode: LibraryMode,
    configured_path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ManagedLibraryStartupSelection {
    Unconfigured,
    Configured(LibraryFingerprint),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ManagedLibraryConfigError {
    #[error("managed library mode is invalid")]
    InvalidMode,
    #[error("managed library location does not match its application-owned location")]
    ManagedLocationMismatch,
    #[error("existing library location is empty")]
    EmptyExistingLocation,
    #[error("existing library location is not absolute")]
    RelativeExistingLocation,
    #[error("existing library location is inside the application data root")]
    ExistingLocationInsideAppRoot,
}

#[derive(Clone)]
pub(crate) struct LibraryOperation {
    generation: LibraryGenerationId,
    fingerprint: LibraryFingerprint,
    operation: ManagedLibraryOperation,
}

#[must_use = "prepared library change must be committed after persistence or dropped"]
pub(crate) struct PreparedManagedLibraryChange {
    inner: Arc<ManagedLibraryOwnerInner>,
    expected: Option<ExpectedLibraryGeneration>,
    next_revision: u64,
    change: PreparedManagedLibraryChangeKind,
    _rotation: OwnedMutexGuard<()>,
}

#[derive(Clone)]
struct ExpectedLibraryGeneration {
    id: LibraryGenerationId,
    fingerprint: LibraryFingerprint,
    binding: ManagedLibraryBinding,
}

enum PreparedManagedLibraryChangeKind {
    Unconfigure,
    Rebind {
        fingerprint: LibraryFingerprint,
        binding: ManagedLibraryBinding,
        admission: PreparedManagedLibraryAdmissionRebind,
    },
    Replace {
        fingerprint: LibraryFingerprint,
        binding: ManagedLibraryBinding,
        root: ManagedLibraryRoot,
    },
}

enum PreparedManagedLibraryWorkerOutcome {
    NoChange,
    Change(PreparedManagedLibraryChangeKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLibraryCommitOutcome {
    Ready,
    Unconfigured,
    Degraded(ManagedLibraryDegradedReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLibraryDegradedReason {
    ExistingLibraryUnavailable,
    AdmissionChangedAfterPersistence,
    AdmissionBindingLostAfterPersistence,
    GenerationClosedAfterPersistence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLibraryAvailability {
    Ready {
        generation: LibraryGenerationId,
        mode: LibraryMode,
    },
    Unconfigured,
    Degraded(ManagedLibraryDegradedReason),
    Changing { next_revision: u64 },
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLibraryStatus {
    pub(crate) revision: u64,
    pub(crate) availability: ManagedLibraryAvailability,
    pub(crate) retirement_pending: bool,
}

pub(crate) struct ManagedLibraryStartup {
    owner: ManagedLibraryOwner,
    degraded: Option<ManagedLibraryDegradedReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ManagedLibraryStartupError {
    #[error(transparent)]
    Config(#[from] ManagedLibraryConfigError),
    #[error("managed library location could not be admitted ({0:?})")]
    ManagedLocationUnavailable(io::ErrorKind),
}

impl std::fmt::Debug for ManagedLibraryOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLibraryOwner")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for LibraryFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibraryFingerprint")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for LibraryOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibraryOperation")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedManagedLibraryChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedManagedLibraryChange")
            .finish_non_exhaustive()
    }
}

impl ManagedLibraryStartupSelection {
    pub(crate) fn from_config(
        config: &AppConfig,
        paths: &AppPaths,
    ) -> Result<Self, ManagedLibraryConfigError> {
        let configured = config.library_dir.trim();
        match config.library_mode.as_str() {
            "managed" if configured.is_empty() => Ok(Self::Unconfigured),
            "managed" => {
                let configured_path = normalize_path(Path::new(configured));
                let managed_path = normalize_path(paths.library_dir());
                if configured_path != managed_path {
                    return Err(ManagedLibraryConfigError::ManagedLocationMismatch);
                }
                Ok(Self::Configured(LibraryFingerprint {
                    mode: LibraryMode::Managed,
                    configured_path,
                }))
            }
            "existing" if configured.is_empty() => {
                Err(ManagedLibraryConfigError::EmptyExistingLocation)
            }
            "existing" => {
                let configured_path = normalize_path(Path::new(configured));
                if !configured_path.is_absolute() {
                    return Err(ManagedLibraryConfigError::RelativeExistingLocation);
                }
                let app_root = paths
                    .library_dir()
                    .parent()
                    .expect("managed library path has an application-root parent");
                if configured_path.starts_with(normalize_path(app_root)) {
                    return Err(ManagedLibraryConfigError::ExistingLocationInsideAppRoot);
                }
                Ok(Self::Configured(LibraryFingerprint {
                    mode: LibraryMode::Existing,
                    configured_path,
                }))
            }
            _ => Err(ManagedLibraryConfigError::InvalidMode),
        }
    }
}

impl ManagedLibraryStartup {
    pub(crate) fn prepare(
        root_session: Arc<AppRootSession>,
        paths: &AppPaths,
        config: &AppConfig,
    ) -> Result<Self, ManagedLibraryStartupError> {
        let selection = ManagedLibraryStartupSelection::from_config(config, paths)?;
        match selection {
            ManagedLibraryStartupSelection::Unconfigured => Ok(Self {
                owner: ManagedLibraryOwner::unconfigured(root_session, paths.clone()),
                degraded: None,
            }),
            ManagedLibraryStartupSelection::Configured(fingerprint)
                if fingerprint.mode() == LibraryMode::Managed =>
            {
                let admission = root_session
                    .prepare_managed_library_directory(paths)
                    .map_err(|error| {
                        ManagedLibraryStartupError::ManagedLocationUnavailable(error.kind())
                    })?;
                let owner = ManagedLibraryOwner::from_admission(
                    root_session,
                    paths.clone(),
                    fingerprint,
                    admission,
                )
                .map_err(|error| {
                    ManagedLibraryStartupError::ManagedLocationUnavailable(error.kind())
                })?;
                Ok(Self {
                    owner,
                    degraded: None,
                })
            }
            ManagedLibraryStartupSelection::Configured(fingerprint) => {
                match root_session
                    .admit_existing_library_directory(fingerprint.configured_path())
                {
                    ExistingLibraryDirectoryAdmission::Admitted(admission) => {
                        match ManagedLibraryOwner::from_admission(
                            Arc::clone(&root_session),
                            paths.clone(),
                            fingerprint,
                            admission,
                        ) {
                            Ok(owner) => Ok(Self {
                                owner,
                                degraded: None,
                            }),
                            Err(_) => Ok(Self {
                                owner: ManagedLibraryOwner::degraded(
                                    root_session,
                                    paths.clone(),
                                    ManagedLibraryDegradedReason::ExistingLibraryUnavailable,
                                ),
                                degraded: Some(
                                    ManagedLibraryDegradedReason::ExistingLibraryUnavailable,
                                ),
                            }),
                        }
                    }
                    ExistingLibraryDirectoryAdmission::InsideRoot => {
                        Err(ManagedLibraryConfigError::ExistingLocationInsideAppRoot.into())
                    }
                    ExistingLibraryDirectoryAdmission::Unavailable(_) => {
                        let reason = ManagedLibraryDegradedReason::ExistingLibraryUnavailable;
                        Ok(Self {
                            owner: ManagedLibraryOwner::degraded(
                                root_session,
                                paths.clone(),
                                reason,
                            ),
                            degraded: Some(reason),
                        })
                    }
                }
            }
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (ManagedLibraryOwner, Option<ManagedLibraryDegradedReason>) {
        (self.owner, self.degraded)
    }
}

impl ManagedLibraryStartupError {
    pub(crate) fn into_io_error(self) -> io::Error {
        let kind = match &self {
            Self::Config(_) => io::ErrorKind::InvalidData,
            Self::ManagedLocationUnavailable(kind) => *kind,
        };
        io::Error::new(kind, self)
    }
}

impl LibraryFingerprint {
    pub(crate) fn mode(&self) -> LibraryMode {
        self.mode
    }

    pub(crate) fn configured_path(&self) -> &Path {
        &self.configured_path
    }
}

impl ManagedLibraryOwner {
    pub(crate) fn unconfigured(root_session: Arc<AppRootSession>, paths: AppPaths) -> Self {
        Self::from_state(root_session, paths, ManagedLibraryState::default())
    }

    pub(crate) fn from_admission(
        root_session: Arc<AppRootSession>,
        paths: AppPaths,
        fingerprint: LibraryFingerprint,
        admission: AdmittedAbsoluteDirectory,
    ) -> io::Result<Self> {
        admission.revalidate()?;
        let expected_binding = ManagedLibraryRoot::admitted_binding(&admission)?;
        let root = ManagedLibraryRoot::from_admitted_directory(admission)?;
        if root.binding()? != expected_binding {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed library binding changed during generation construction",
            ));
        }
        root.revalidate()?;
        if fingerprint.mode == LibraryMode::Managed {
            root.try_acquire()?.prepare_layout()?;
        }
        Ok(Self::from_state(
            root_session,
            paths,
            ManagedLibraryState {
                closed: false,
                revision: 1,
                publishing_revision: None,
                current: Some(CurrentLibraryGeneration {
                    id: LibraryGenerationId(1),
                    fingerprint,
                    binding: expected_binding,
                    root,
                }),
                retiring: None,
                degraded: None,
            },
        ))
    }

    fn degraded(
        root_session: Arc<AppRootSession>,
        paths: AppPaths,
        reason: ManagedLibraryDegradedReason,
    ) -> Self {
        let mut state = ManagedLibraryState::default();
        state.degraded = Some(reason);
        Self::from_state(root_session, paths, state)
    }

    fn from_state(
        root_session: Arc<AppRootSession>,
        paths: AppPaths,
        state: ManagedLibraryState,
    ) -> Self {
        Self {
            inner: Arc::new(ManagedLibraryOwnerInner {
                root_session,
                paths,
                rotation: Arc::new(AsyncMutex::new(())),
                state: Mutex::new(state),
            }),
        }
    }

    pub(crate) fn try_acquire(&self) -> io::Result<LibraryOperation> {
        let (generation, fingerprint, witness) = {
            let state = self.lock_state();
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "managed library owner is closed",
                ));
            }
            if state.publishing_revision.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "managed library generation is changing",
                ));
            }
            let current = state.current.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "managed library is not configured",
                )
            })?;
            (
                current.id,
                current.fingerprint.clone(),
                current.root.witness(),
            )
        };
        let operation = witness.try_acquire()?;
        let state = self.lock_state();
        if state.closed
            || state.publishing_revision.is_some()
            || state.revision != generation.0
            || !state
                .current
                .as_ref()
                .is_some_and(|current| current.id == generation && current.fingerprint == fingerprint)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed library generation changed during operation admission",
            ));
        }
        Ok(LibraryOperation {
            generation,
            fingerprint,
            operation,
        })
    }

    pub(crate) fn validate_current(&self, operation: &LibraryOperation) -> io::Result<()> {
        let state = self.lock_state();
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed library owner is closed",
            ));
        }
        if state.publishing_revision.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed library generation is changing",
            ));
        }
        let current = state.current.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "managed library is not configured",
            )
        })?;
        if state.revision != operation.generation.0
            || current.id != operation.generation
            || current.fingerprint != operation.fingerprint
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed library operation belongs to a stale generation",
            ));
        }
        drop(state);
        operation.revalidate()
    }

    pub(crate) async fn close(&self) -> io::Result<()> {
        let _rotation = self.inner.rotation.clone().lock_owned().await;
        loop {
            let retirement = {
                let mut state = self.lock_state();
                state.closed = true;
                if state.retiring.is_none() {
                    if let Some(current) = state.current.take() {
                        state.retiring = Some(RetiringLibraryGeneration {
                            id: current.id,
                            retirement: Arc::new(current.root.begin_retirement()),
                        });
                    }
                }
                state
                    .retiring
                    .as_ref()
                    .map(|retiring| (retiring.id, Arc::clone(&retiring.retirement)))
            };

            let Some((generation, retirement)) = retirement else {
                return Ok(());
            };
            let binding = retirement.drain_and_settle().await?;
            diagnose_retirement_binding(generation, binding);

            let mut state = self.lock_state();
            if state
                .retiring
                .as_ref()
                .is_some_and(|retiring| retiring.id == generation)
            {
                state.retiring = None;
            }
        }
    }

    pub(crate) fn status(&self) -> ManagedLibraryStatus {
        let state = self.lock_state();
        let availability = if state.closed {
            ManagedLibraryAvailability::Closed
        } else if let Some(next_revision) = state.publishing_revision {
            ManagedLibraryAvailability::Changing { next_revision }
        } else if let Some(current) = state.current.as_ref() {
            ManagedLibraryAvailability::Ready {
                generation: current.id,
                mode: current.fingerprint.mode,
            }
        } else if let Some(reason) = state.degraded {
            ManagedLibraryAvailability::Degraded(reason)
        } else {
            ManagedLibraryAvailability::Unconfigured
        };
        ManagedLibraryStatus {
            revision: state.revision,
            availability,
            retirement_pending: state.retiring.is_some(),
        }
    }

    pub(crate) async fn prepare_change(
        &self,
        selection: ManagedLibraryStartupSelection,
    ) -> io::Result<Option<PreparedManagedLibraryChange>> {
        let rotation = self.inner.rotation.clone().lock_owned().await;
        self.settle_retirement_locked().await?;

        let (expected, witness, degraded) = self.preparation_snapshot()?;
        match selection {
            ManagedLibraryStartupSelection::Unconfigured
                if expected.is_none() && degraded.is_none() =>
            {
                Ok(None)
            }
            ManagedLibraryStartupSelection::Unconfigured => {
                prepare_change_kind(
                    Arc::clone(&self.inner),
                    expected,
                    PreparedManagedLibraryChangeKind::Unconfigure,
                    rotation,
                )
                .map(Some)
            }
            ManagedLibraryStartupSelection::Configured(fingerprint) => {
                let inner = Arc::clone(&self.inner);
                let root_session = Arc::clone(&inner.root_session);
                let paths = inner.paths.clone();
                let worker_expected = expected.clone();
                let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let prepared = tokio::task::spawn_blocking(move || {
                        prepare_configured_change(
                            root_session,
                            paths,
                            fingerprint,
                            worker_expected.as_ref(),
                            witness,
                        )
                    })
                    .await
                    .map_err(|_| io::Error::other("managed library preparation task stopped"))
                    .and_then(|result| result)
                    .and_then(|outcome| match outcome {
                        PreparedManagedLibraryWorkerOutcome::NoChange => Ok(None),
                        PreparedManagedLibraryWorkerOutcome::Change(change) => {
                            prepare_change_kind(inner, expected, change, rotation).map(Some)
                        }
                    });
                    let _ = completed_tx.send(prepared);
                });
                completed_rx.await.map_err(|_| {
                    io::Error::other("managed library preparation owner stopped")
                })?
            }
        }
    }

    pub(crate) async fn settle_retirement(&self) -> io::Result<()> {
        let _rotation = self.inner.rotation.clone().lock_owned().await;
        self.settle_retirement_locked().await
    }

    async fn settle_retirement_locked(&self) -> io::Result<()> {
        loop {
            let retirement = self
                .lock_state()
                .retiring
                .as_ref()
                .map(|retiring| (retiring.id, Arc::clone(&retiring.retirement)));
            let Some((generation, retirement)) = retirement else {
                return Ok(());
            };
            let binding = retirement.drain_and_settle().await?;
            diagnose_retirement_binding(generation, binding);
            let mut state = self.lock_state();
            if state
                .retiring
                .as_ref()
                .is_some_and(|retiring| retiring.id == generation)
            {
                state.retiring = None;
            }
        }
    }

    fn preparation_snapshot(
        &self,
    ) -> io::Result<(
        Option<ExpectedLibraryGeneration>,
        Option<ManagedLibraryWitness>,
        Option<ManagedLibraryDegradedReason>,
    )> {
        let state = self.lock_state();
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed library owner is closed",
            ));
        }
        if state.publishing_revision.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed library generation is changing",
            ));
        }
        let expected = state
            .current
            .as_ref()
            .map(|current| ExpectedLibraryGeneration {
                id: current.id,
                fingerprint: current.fingerprint.clone(),
                binding: current.binding,
            });
        let witness = state.current.as_ref().map(|current| current.root.witness());
        Ok((expected, witness, state.degraded))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ManagedLibraryState> {
        match self.inner.state.lock() {
            Ok(state) => state,
            Err(_) => {
                tracing::error!("{LIBRARY_STATE_LOCK_INVARIANT}");
                std::process::abort();
            }
        }
    }

    #[cfg(test)]
    fn revision_for_test(&self) -> u64 {
        self.lock_state().revision
    }

    #[cfg(test)]
    fn retiring_generation_for_test(&self) -> Option<LibraryGenerationId> {
        self.lock_state()
            .retiring
            .as_ref()
            .map(|retiring| retiring.id)
    }

    #[cfg(test)]
    fn closed_for_test(&self) -> bool {
        self.lock_state().closed
    }
}

impl PreparedManagedLibraryChange {
    pub(crate) fn commit(self) -> ManagedLibraryCommitOutcome {
        self.commit_with_publication_hook(|| {})
    }

    fn commit_with_publication_hook(
        self,
        mut publication_started: impl FnMut(),
    ) -> ManagedLibraryCommitOutcome {
        let Self {
            inner,
            expected,
            next_revision,
            change,
            _rotation,
        } = self;
        match change {
            PreparedManagedLibraryChangeKind::Unconfigure => {
                let mut state = lock_owner_state(&inner);
                require_expected_current(&state, expected.as_ref());
                retire_current(&mut state);
                state.revision = next_revision;
                state.degraded = None;
                ManagedLibraryCommitOutcome::Unconfigured
            }
            PreparedManagedLibraryChangeKind::Rebind {
                fingerprint,
                binding,
                admission,
            } => {
                begin_publication(&inner, expected.as_ref(), next_revision);
                publication_started();
                match admission.commit() {
                    Ok(()) => {
                        let mut state = lock_owner_state(&inner);
                        require_publication(&state, next_revision);
                        let current = require_expected_current_mut(&mut state, expected.as_ref())
                            .unwrap_or_else(|| std::process::abort());
                        current.id = LibraryGenerationId(next_revision);
                        current.fingerprint = fingerprint;
                        current.binding = binding;
                        state.revision = next_revision;
                        state.publishing_revision = None;
                        state.degraded = None;
                        ManagedLibraryCommitOutcome::Ready
                    }
                    Err(failure) => {
                        let reason = match failure {
                            ManagedLibraryAdmissionRebindFailure::Stale(candidate) => {
                                drop(candidate);
                                ManagedLibraryDegradedReason::AdmissionChangedAfterPersistence
                            }
                            ManagedLibraryAdmissionRebindFailure::BindingLost(candidate) => {
                                drop(candidate);
                                ManagedLibraryDegradedReason::AdmissionBindingLostAfterPersistence
                            }
                            ManagedLibraryAdmissionRebindFailure::GenerationClosed(candidate) => {
                                drop(candidate);
                                ManagedLibraryDegradedReason::GenerationClosedAfterPersistence
                            }
                        };
                        let mut state = lock_owner_state(&inner);
                        require_publication(&state, next_revision);
                        require_expected_current(&state, expected.as_ref());
                        retire_current(&mut state);
                        state.revision = next_revision;
                        state.publishing_revision = None;
                        state.degraded = Some(reason);
                        ManagedLibraryCommitOutcome::Degraded(reason)
                    }
                }
            }
            PreparedManagedLibraryChangeKind::Replace {
                fingerprint,
                binding,
                root,
            } => {
                if root.revalidate().is_err() || root.binding().ok() != Some(binding) {
                    drop(root);
                    let mut state = lock_owner_state(&inner);
                    require_expected_current(&state, expected.as_ref());
                    retire_current(&mut state);
                    state.revision = next_revision;
                    state.degraded = Some(
                        ManagedLibraryDegradedReason::AdmissionBindingLostAfterPersistence,
                    );
                    return ManagedLibraryCommitOutcome::Degraded(
                        ManagedLibraryDegradedReason::AdmissionBindingLostAfterPersistence,
                    );
                }
                let mut state = lock_owner_state(&inner);
                require_expected_current(&state, expected.as_ref());
                let previous = state.current.take();
                if let Some(previous) = previous {
                    if state.retiring.is_some() {
                        std::process::abort();
                    }
                    state.retiring = Some(RetiringLibraryGeneration {
                        id: previous.id,
                        retirement: Arc::new(previous.root.begin_retirement()),
                    });
                }
                state.current = Some(CurrentLibraryGeneration {
                    id: LibraryGenerationId(next_revision),
                    fingerprint,
                    binding,
                    root,
                });
                state.revision = next_revision;
                state.degraded = None;
                ManagedLibraryCommitOutcome::Ready
            }
        }
    }
}

impl Default for ManagedLibraryState {
    fn default() -> Self {
        Self {
            closed: false,
            revision: 0,
            publishing_revision: None,
            current: None,
            retiring: None,
            degraded: None,
        }
    }
}

impl LibraryOperation {
    pub(crate) fn generation(&self) -> LibraryGenerationId {
        self.generation
    }

    pub(crate) fn configured_path(&self) -> &Path {
        self.fingerprint.configured_path()
    }

    pub(crate) fn revalidate(&self) -> io::Result<()> {
        self.operation.revalidate()
    }

    pub(crate) fn prepare_layout(&self) -> io::Result<()> {
        self.operation.prepare_layout()
    }

    pub(crate) fn core(&self) -> &ManagedLibraryOperation {
        &self.operation
    }

    pub(crate) fn retained_core(&self) -> ManagedLibraryOperation {
        self.operation.clone()
    }
}

fn prepare_configured_change(
    root_session: Arc<AppRootSession>,
    paths: AppPaths,
    fingerprint: LibraryFingerprint,
    expected: Option<&ExpectedLibraryGeneration>,
    witness: Option<ManagedLibraryWitness>,
) -> io::Result<PreparedManagedLibraryWorkerOutcome> {
    let admission = match fingerprint.mode {
        LibraryMode::Managed => root_session.prepare_managed_library_directory(&paths)?,
        LibraryMode::Existing => {
            match root_session.admit_existing_library_directory(fingerprint.configured_path()) {
                ExistingLibraryDirectoryAdmission::Admitted(admission) => admission,
                ExistingLibraryDirectoryAdmission::InsideRoot => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "existing library location is inside the application data root",
                    ));
                }
                ExistingLibraryDirectoryAdmission::Unavailable(error) => return Err(error),
            }
        }
    };
    let binding = ManagedLibraryRoot::admitted_binding(&admission)?;
    if expected.is_some_and(|current| current.binding == binding) {
        let witness = witness.unwrap_or_else(|| std::process::abort());
        if expected.is_some_and(|current| current.fingerprint == fingerprint) {
            if let Ok(operation) = witness.try_acquire() {
                if fingerprint.mode == LibraryMode::Managed {
                    operation.prepare_layout()?;
                }
                return Ok(PreparedManagedLibraryWorkerOutcome::NoChange);
            }
        }
        if expected.is_some_and(|current| current.fingerprint.mode != fingerprint.mode) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "library ownership mode cannot change on the same physical binding",
            ));
        }
        let admission = witness.prepare_admission_rebind(admission)?;
        return Ok(PreparedManagedLibraryWorkerOutcome::Change(
            PreparedManagedLibraryChangeKind::Rebind {
                fingerprint,
                binding,
                admission,
            },
        ));
    }

    let root = ManagedLibraryRoot::from_admitted_directory(admission)?;
    if root.binding()? != binding {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed library binding changed during rotation preparation",
        ));
    }
    if fingerprint.mode == LibraryMode::Managed {
        root.try_acquire()?.prepare_layout()?;
    }
    Ok(PreparedManagedLibraryWorkerOutcome::Change(
        PreparedManagedLibraryChangeKind::Replace {
            fingerprint,
            binding,
            root,
        },
    ))
}

fn prepare_change_kind(
    inner: Arc<ManagedLibraryOwnerInner>,
    expected: Option<ExpectedLibraryGeneration>,
    change: PreparedManagedLibraryChangeKind,
    rotation: OwnedMutexGuard<()>,
) -> io::Result<PreparedManagedLibraryChange> {
    let state = lock_owner_state(&inner);
    if state.closed {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "managed library owner is closed",
        ));
    }
    if state.publishing_revision.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed library generation is changing",
        ));
    }
    require_expected_current(&state, expected.as_ref());
    let next_revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| io::Error::other("managed library generation revision overflowed"))?;
    drop(state);
    Ok(PreparedManagedLibraryChange {
        inner,
        expected,
        next_revision,
        change,
        _rotation: rotation,
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn lock_owner_state(
    inner: &ManagedLibraryOwnerInner,
) -> std::sync::MutexGuard<'_, ManagedLibraryState> {
    match inner.state.lock() {
        Ok(state) => state,
        Err(_) => {
            tracing::error!("{LIBRARY_STATE_LOCK_INVARIANT}");
            std::process::abort();
        }
    }
}

fn require_expected_current<'a>(
    state: &'a ManagedLibraryState,
    expected: Option<&ExpectedLibraryGeneration>,
) -> Option<&'a CurrentLibraryGeneration> {
    let current = state.current.as_ref();
    if !current_matches_expected(current, expected) {
        std::process::abort();
    }
    current
}

fn require_expected_current_mut<'a>(
    state: &'a mut ManagedLibraryState,
    expected: Option<&ExpectedLibraryGeneration>,
) -> Option<&'a mut CurrentLibraryGeneration> {
    if !current_matches_expected(state.current.as_ref(), expected) {
        std::process::abort();
    }
    state.current.as_mut()
}

fn current_matches_expected(
    current: Option<&CurrentLibraryGeneration>,
    expected: Option<&ExpectedLibraryGeneration>,
) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(current), Some(expected)) => {
            current.id == expected.id
                && current.fingerprint == expected.fingerprint
                && current.binding == expected.binding
        }
        _ => false,
    }
}

fn begin_publication(
    inner: &ManagedLibraryOwnerInner,
    expected: Option<&ExpectedLibraryGeneration>,
    next_revision: u64,
) {
    let mut state = lock_owner_state(inner);
    require_expected_current(&state, expected);
    if state.closed
        || state.publishing_revision.is_some()
        || state.revision.checked_add(1) != Some(next_revision)
    {
        std::process::abort();
    }
    state.publishing_revision = Some(next_revision);
}

fn require_publication(state: &ManagedLibraryState, revision: u64) {
    if state.publishing_revision != Some(revision) {
        std::process::abort();
    }
}

fn retire_current(state: &mut ManagedLibraryState) {
    let Some(current) = state.current.take() else {
        return;
    };
    if state.retiring.is_some() {
        std::process::abort();
    }
    state.retiring = Some(RetiringLibraryGeneration {
        id: current.id,
        retirement: Arc::new(current.root.begin_retirement()),
    });
}

fn diagnose_retirement_binding(
    generation: LibraryGenerationId,
    binding: ManagedLibraryRetirementBinding,
) {
    if binding == ManagedLibraryRetirementBinding::BindingLost {
        tracing::warn!(
            generation = generation.0,
            "retired managed library admission binding was lost"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(1);

    fn paths(name: &str) -> AppPaths {
        let root = std::env::temp_dir()
            .join(format!(
                "axial-managed-library-state-{name}-{}-{}",
                std::process::id(),
                NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed)
            ))
            .join("app");
        AppPaths::from_root(root).expect("absolute test app path")
    }

    fn configured_owner(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<AppRootSession>,
        ManagedLibraryOwner,
    ) {
        let paths = paths(name);
        let app_root = paths
            .library_dir()
            .parent()
            .expect("managed library parent")
            .to_path_buf();
        std::fs::create_dir_all(&app_root).expect("create app root");
        let root_session = Arc::new(paths.open_root_session().expect("root session"));
        let admission = root_session
            .prepare_managed_library_directory(&paths)
            .expect("prepare managed library");
        let config = AppConfig {
            library_dir: paths.library_dir().to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let ManagedLibraryStartupSelection::Configured(fingerprint) =
            ManagedLibraryStartupSelection::from_config(&config, &paths)
                .expect("configured managed library")
        else {
            panic!("configured managed library was treated as unconfigured");
        };
        let owner = ManagedLibraryOwner::from_admission(
            Arc::clone(&root_session),
            paths.clone(),
            fingerprint,
            admission,
        )
        .expect("managed library owner");
        (app_root, paths, root_session, owner)
    }

    fn configured_existing_owner(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<AppRootSession>,
        ManagedLibraryOwner,
        PathBuf,
    ) {
        let paths = paths(name);
        let app_root = paths
            .library_dir()
            .parent()
            .expect("managed library parent")
            .to_path_buf();
        let existing = app_root
            .parent()
            .expect("temporary parent")
            .join("existing-library");
        std::fs::create_dir_all(&app_root).expect("create app root");
        std::fs::create_dir(&existing).expect("create existing library");
        let root_session = Arc::new(paths.open_root_session().expect("root session"));
        let config = AppConfig {
            library_mode: "existing".to_string(),
            library_dir: existing.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let startup = ManagedLibraryStartup::prepare(
            Arc::clone(&root_session),
            &paths,
            &config,
        )
        .expect("existing library startup");
        let (owner, degraded) = startup.into_parts();
        assert_eq!(degraded, None);
        (app_root, paths, root_session, owner, existing)
    }

    fn cleanup_test_root(
        app_root: PathBuf,
        root_session: Arc<AppRootSession>,
        owner: ManagedLibraryOwner,
    ) {
        drop(owner);
        drop(root_session);
        std::fs::remove_dir_all(app_root.parent().expect("temporary parent"))
            .expect("remove test root");
    }

    #[test]
    fn blank_managed_config_is_explicitly_unconfigured() {
        let paths = paths("blank-managed");
        assert!(matches!(
            ManagedLibraryStartupSelection::from_config(&AppConfig::default(), &paths),
            Ok(ManagedLibraryStartupSelection::Unconfigured)
        ));
    }

    #[test]
    fn startup_selection_rejects_invalid_or_unsafe_configurations() {
        let paths = paths("invalid");
        let cases = [
            (
                AppConfig {
                    library_mode: "legacy".to_string(),
                    ..AppConfig::default()
                },
                ManagedLibraryConfigError::InvalidMode,
            ),
            (
                AppConfig {
                    library_mode: "existing".to_string(),
                    ..AppConfig::default()
                },
                ManagedLibraryConfigError::EmptyExistingLocation,
            ),
            (
                AppConfig {
                    library_mode: "existing".to_string(),
                    library_dir: "relative/library".to_string(),
                    ..AppConfig::default()
                },
                ManagedLibraryConfigError::RelativeExistingLocation,
            ),
            (
                AppConfig {
                    library_mode: "existing".to_string(),
                    library_dir: paths.library_dir().to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
                ManagedLibraryConfigError::ExistingLocationInsideAppRoot,
            ),
        ];

        for (config, expected) in cases {
            assert_eq!(
                ManagedLibraryStartupSelection::from_config(&config, &paths),
                Err(expected)
            );
        }
    }

    #[test]
    fn managed_startup_prepares_the_capability_relative_layout() {
        let paths = paths("managed-startup-layout");
        let app_root = paths
            .library_dir()
            .parent()
            .expect("app root")
            .to_path_buf();
        std::fs::create_dir_all(&app_root).expect("create app root");
        let root_session = Arc::new(paths.open_root_session().expect("root session"));
        let config = AppConfig {
            library_dir: paths.library_dir().to_string_lossy().into_owned(),
            ..AppConfig::default()
        };

        let startup = ManagedLibraryStartup::prepare(
            Arc::clone(&root_session),
            &paths,
            &config,
        )
        .expect("managed startup");
        let (owner, degraded) = startup.into_parts();
        assert_eq!(degraded, None);
        for child in ["versions", "libraries", "assets", "cache/loaders/catalog"] {
            assert!(paths.library_dir().join(child).is_dir());
        }
        cleanup_test_root(app_root, root_session, owner);
    }

    #[test]
    fn fingerprint_debug_does_not_expose_the_configured_path() {
        let fingerprint = LibraryFingerprint {
            mode: LibraryMode::Existing,
            configured_path: PathBuf::from("/private/library/location"),
        };
        let rendered = format!("{fingerprint:?}");
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("location"));
        assert!(rendered.contains("Existing"));
    }

    #[test]
    fn unconfigured_owner_has_no_generation() {
        let paths = paths("unconfigured-owner");
        let app_root = paths
            .library_dir()
            .parent()
            .expect("app root")
            .to_path_buf();
        std::fs::create_dir_all(&app_root).expect("create app root");
        let root_session = Arc::new(paths.open_root_session().expect("root session"));
        let owner = ManagedLibraryOwner::unconfigured(Arc::clone(&root_session), paths.clone());
        assert_eq!(owner.revision_for_test(), 0);
        assert_eq!(
            owner.try_acquire().expect_err("unconfigured owner").kind(),
            io::ErrorKind::NotFound
        );
        cleanup_test_root(app_root, root_session, owner);
    }

    #[test]
    fn absent_existing_library_starts_degraded_without_creating_the_target() {
        let paths = paths("missing-existing");
        let app_root = paths
            .library_dir()
            .parent()
            .expect("app root")
            .to_path_buf();
        let missing = app_root
            .parent()
            .expect("temporary parent")
            .join("missing-library");
        std::fs::create_dir_all(&app_root).expect("create app root");
        let root_session = Arc::new(paths.open_root_session().expect("root session"));
        let config = AppConfig {
            library_mode: "existing".to_string(),
            library_dir: missing.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };

        let startup = ManagedLibraryStartup::prepare(
            Arc::clone(&root_session),
            &paths,
            &config,
        )
        .expect("unavailable existing library is degraded");
        let (owner, degraded) = startup.into_parts();
        assert_eq!(
            degraded,
            Some(ManagedLibraryDegradedReason::ExistingLibraryUnavailable)
        );
        assert_eq!(
            owner.status().availability,
            ManagedLibraryAvailability::Degraded(
                ManagedLibraryDegradedReason::ExistingLibraryUnavailable
            )
        );
        assert!(!missing.exists());
        cleanup_test_root(app_root, root_session, owner);
    }

    #[tokio::test]
    async fn unconfiguring_a_degraded_owner_clears_the_degraded_state() {
        let paths = paths("degraded-unconfigure");
        let app_root = paths
            .library_dir()
            .parent()
            .expect("app root")
            .to_path_buf();
        let missing = app_root
            .parent()
            .expect("temporary parent")
            .join("missing-library");
        std::fs::create_dir_all(&app_root).expect("create app root");
        let root_session = Arc::new(paths.open_root_session().expect("root session"));
        let startup = ManagedLibraryStartup::prepare(
            Arc::clone(&root_session),
            &paths,
            &AppConfig {
                library_mode: "existing".to_string(),
                library_dir: missing.to_string_lossy().into_owned(),
                ..AppConfig::default()
            },
        )
        .expect("degraded startup");
        let (owner, _) = startup.into_parts();
        let prepared = owner
            .prepare_change(ManagedLibraryStartupSelection::Unconfigured)
            .await
            .expect("prepare unconfigure")
            .expect("degraded state needs a commit");
        assert_eq!(
            prepared.commit(),
            ManagedLibraryCommitOutcome::Unconfigured
        );
        assert_eq!(
            owner.status(),
            ManagedLibraryStatus {
                revision: 1,
                availability: ManagedLibraryAvailability::Unconfigured,
                retirement_pending: false,
            }
        );
        owner.close().await.expect("close owner");
        cleanup_test_root(app_root, root_session, owner);
    }

    #[tokio::test]
    async fn identical_candidate_is_no_change_and_dropped_rebind_has_no_effect() {
        let (app_root, paths, root_session, owner, existing) =
            configured_existing_owner("no-change");
        let config = AppConfig {
            library_mode: "existing".to_string(),
            library_dir: existing.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let selection = ManagedLibraryStartupSelection::from_config(&config, &paths)
            .expect("existing selection");
        assert!(
            owner
                .prepare_change(selection)
                .await
                .expect("prepare no change")
                .is_none()
        );
        assert_eq!(owner.status().revision, 1);

        let renamed = existing.with_file_name("renamed-existing-library");
        std::fs::rename(&existing, &renamed).expect("rename existing library");
        let changed = ManagedLibraryStartupSelection::from_config(
            &AppConfig {
                library_mode: "existing".to_string(),
                library_dir: renamed.to_string_lossy().into_owned(),
                ..AppConfig::default()
            },
            &paths,
        )
        .expect("renamed existing selection");
        let prepared = owner
            .prepare_change(changed)
            .await
            .expect("prepare rebind")
            .expect("changed fingerprint");
        drop(prepared);
        assert_eq!(owner.status().revision, 1);
        assert!(owner.try_acquire().is_err());
        owner.close().await.expect("close owner");
        cleanup_test_root(app_root, root_session, owner);
    }

    #[tokio::test]
    async fn rebind_publication_barrier_refuses_mixed_metadata_operations() {
        let (app_root, paths, root_session, owner, existing) =
            configured_existing_owner("rebind-barrier");
        let renamed = existing.with_file_name("renamed-existing-library");
        std::fs::rename(&existing, &renamed).expect("rename existing library");
        let changed = ManagedLibraryStartupSelection::from_config(
            &AppConfig {
                library_mode: "existing".to_string(),
                library_dir: renamed.to_string_lossy().into_owned(),
                ..AppConfig::default()
            },
            &paths,
        )
        .expect("renamed existing selection");
        let prepared = owner
            .prepare_change(changed)
            .await
            .expect("prepare rebind")
            .expect("changed fingerprint");
        let outcome = prepared.commit_with_publication_hook(|| {
            assert_eq!(
                owner.status().availability,
                ManagedLibraryAvailability::Changing { next_revision: 2 }
            );
            assert_eq!(
                owner
                    .try_acquire()
                    .expect_err("publication must block operation admission")
                    .kind(),
                io::ErrorKind::WouldBlock
            );
        });
        assert_eq!(outcome, ManagedLibraryCommitOutcome::Ready);
        assert_eq!(
            owner.status().availability,
            ManagedLibraryAvailability::Ready {
                generation: LibraryGenerationId(2),
                mode: LibraryMode::Existing,
            }
        );
        owner.close().await.expect("close owner");
        cleanup_test_root(app_root, root_session, owner);
    }

    #[tokio::test]
    async fn replacement_publishes_new_generation_before_old_retirement_drains() {
        let (app_root, paths, root_session, owner) = configured_owner("replacement");
        let old_operation = owner.try_acquire().expect("old generation operation");
        let external = app_root
            .parent()
            .expect("temporary parent")
            .join("external-library");
        std::fs::create_dir(&external).expect("create external library");
        let config = AppConfig {
            library_mode: "existing".to_string(),
            library_dir: external.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let selection = ManagedLibraryStartupSelection::from_config(&config, &paths)
            .expect("existing selection");
        let prepared = owner
            .prepare_change(selection)
            .await
            .expect("prepare replacement")
            .expect("replacement change");
        assert_eq!(prepared.commit(), ManagedLibraryCommitOutcome::Ready);
        assert!(owner.status().retirement_pending);
        assert!(owner.validate_current(&old_operation).is_err());
        let new_operation = owner.try_acquire().expect("new generation operation");
        assert_eq!(new_operation.generation(), LibraryGenerationId(2));

        let settling_owner = owner.clone();
        let settlement = tokio::spawn(async move { settling_owner.settle_retirement().await });
        tokio::task::yield_now().await;
        assert!(!settlement.is_finished());
        drop(old_operation);
        settlement
            .await
            .expect("settlement task")
            .expect("settle old generation");
        assert!(!owner.status().retirement_pending);
        drop(new_operation);
        owner.close().await.expect("close owner");
        cleanup_test_root(app_root, root_session, owner);
    }

    #[tokio::test]
    async fn operation_is_current_only_while_its_generation_is_open() {
        let (app_root, _paths, root_session, owner) =
            configured_owner("operation-currentness");
        let operation = owner.try_acquire().expect("library operation");
        assert_eq!(operation.generation(), LibraryGenerationId(1));
        owner
            .validate_current(&operation)
            .expect("current operation");
        operation.core().revalidate().expect("core operation");

        let closing_owner = owner.clone();
        let close = tokio::spawn(async move { closing_owner.close().await });
        while !owner.closed_for_test() {
            tokio::task::yield_now().await;
        }
        assert!(owner.validate_current(&operation).is_err());
        drop(operation);
        close.await.expect("close task").expect("close owner");
        cleanup_test_root(app_root, root_session, owner);
    }

    #[tokio::test]
    async fn cancelled_close_resumes_the_same_sole_retirement() {
        let (app_root, _paths, root_session, owner) =
            configured_owner("cancelled-retirement");
        let operation = owner.try_acquire().expect("library operation");
        let first_owner = owner.clone();
        let first = tokio::spawn(async move { first_owner.close().await });
        let generation = loop {
            if let Some(generation) = owner.retiring_generation_for_test() {
                break generation;
            }
            tokio::task::yield_now().await;
        };
        first.abort();
        assert!(first.await.expect_err("cancel close").is_cancelled());
        assert_eq!(owner.retiring_generation_for_test(), Some(generation));

        let second_owner = owner.clone();
        let second = tokio::spawn(async move { second_owner.close().await });
        tokio::task::yield_now().await;
        assert_eq!(owner.retiring_generation_for_test(), Some(generation));
        assert!(!second.is_finished());
        drop(operation);
        second.await.expect("second close task").expect("resume close");
        assert_eq!(owner.retiring_generation_for_test(), None);
        cleanup_test_root(app_root, root_session, owner);
    }
}
