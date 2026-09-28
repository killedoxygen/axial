//! Update progress authority. Package discovery, signature verification and installation
//! are native effects; neither HTTP callers nor persisted paths can mint a staged update.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use ts_rs::TS;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
pub enum UpdatePhase {
    Idle,
    Downloading,
    Ready,
    Applying,
    RestartPending,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub available: bool,
    pub platform: String,
    pub arch: String,
    pub kind: String,
    pub install_mode: String,
    pub notes_url: String,
    pub action_url: String,
    pub checksum_url: Option<String>,
    pub action_label: String,
    pub checked_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UpdateFlow {
    #[ts(type = "number")]
    pub revision: u64,
    pub phase: UpdatePhase,
    pub version: String,
    #[ts(type = "number")]
    pub received_bytes: u64,
    #[ts(type = "number | null")]
    pub total_bytes: Option<u64>,
    pub percent: Option<f64>,
    pub message: String,
    pub supported: bool,
    pub can_check: bool,
    pub can_download: bool,
    pub can_apply: bool,
    pub can_restart: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UpdateSnapshot {
    pub info: Option<UpdateInfo>,
    pub flow: UpdateFlow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateAttempt(u64);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("In-app updates are unavailable for this build.")]
    Unsupported,
    #[error("Another update operation is already active.")]
    Busy,
    #[error("Check for updates again before downloading this version.")]
    StaleRelease,
    #[error("A verified update must be downloaded before installation.")]
    NotReady,
    #[error("{0}")]
    Failed(String),
}

pub type UpdateFuture<T> = Pin<Box<dyn Future<Output = Result<T, UpdateError>> + Send + 'static>>;

/// Installed once by the desktop shell. HTTP commands supply no package paths,
/// signing keys, endpoints or bytes; the native owner retains that authority.
pub trait NativeUpdateAdapter: Send + Sync {
    fn check(&self, service: UpdateService) -> UpdateFuture<UpdateInfo>;
    fn download(&self, service: UpdateService, version: String) -> UpdateFuture<UpdateFlow>;
    fn apply(&self, service: UpdateService) -> UpdateFuture<UpdateFlow>;
}

#[derive(Clone)]
pub struct UpdateService {
    inner: Arc<Mutex<Inner>>,
    changes: watch::Sender<UpdateSnapshot>,
    adapter: Arc<Mutex<Option<Arc<dyn NativeUpdateAdapter>>>>,
}

struct Inner {
    snapshot: UpdateSnapshot,
    current_version: String,
    platform: String,
    arch: String,
    attempt: u64,
    active: Option<UpdateAttempt>,
    terminal: bool,
    configured: bool,
}

impl UpdateService {
    pub fn new(
        current_version: impl Into<String>,
        platform: impl Into<String>,
        arch: impl Into<String>,
    ) -> Self {
        let snapshot = UpdateSnapshot {
            info: None,
            flow: UpdateFlow {
                revision: 0,
                phase: UpdatePhase::Idle,
                version: String::new(),
                received_bytes: 0,
                total_bytes: None,
                percent: None,
                message: "In-app updates require a supported desktop package and a configured trusted release feed.".into(),
                supported: false,
                can_check: false,
                can_download: false,
                can_apply: false,
                can_restart: false,
            },
        };
        let (changes, _) = watch::channel(snapshot.clone());
        Self {
            inner: Arc::new(Mutex::new(Inner {
                snapshot,
                current_version: current_version.into(),
                platform: platform.into(),
                arch: arch.into(),
                attempt: 0,
                active: None,
                terminal: false,
                configured: false,
            })),
            changes,
            adapter: Arc::new(Mutex::new(None)),
        }
    }

    pub fn attach_adapter(&self, adapter: Arc<dyn NativeUpdateAdapter>) -> Result<(), UpdateError> {
        let mut installed = self.adapter.lock().expect("update adapter poisoned");
        if installed.is_some() || self.inner.lock().expect("update state poisoned").configured {
            return Err(UpdateError::Busy);
        }
        *installed = Some(adapter);
        self.configure(None);
        Ok(())
    }

    fn adapter(&self) -> Result<Arc<dyn NativeUpdateAdapter>, UpdateError> {
        self.adapter
            .lock()
            .expect("update adapter poisoned")
            .clone()
            .ok_or(UpdateError::Unsupported)
    }

    pub async fn check(&self) -> Result<UpdateInfo, UpdateError> {
        self.adapter()?.check(self.clone()).await
    }

    pub async fn download(&self, version: &str) -> Result<UpdateFlow, UpdateError> {
        self.adapter()?.download(self.clone(), version.into()).await
    }

    pub async fn apply(&self) -> Result<UpdateFlow, UpdateError> {
        self.adapter()?.apply(self.clone()).await
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        self.inner
            .lock()
            .expect("update state poisoned")
            .snapshot
            .clone()
    }

    /// `watch` subscribes atomically to the latest revision and rebases a slow reader.
    pub fn subscribe(&self) -> watch::Receiver<UpdateSnapshot> {
        self.changes.subscribe()
    }

    /// Called once by the native package adapter after validating its trusted config.
    pub fn configure(&self, unsupported_reason: Option<&str>) {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.configured || inner.active.is_some() || inner.terminal {
            return;
        }
        inner.configured = true;
        inner.snapshot.flow.supported = unsupported_reason.is_none();
        inner.snapshot.flow.message = unsupported_reason.unwrap_or("").into();
        self.publish(&mut inner);
    }

    pub fn begin_check(&self) -> Result<UpdateAttempt, UpdateError> {
        let mut inner = self.inner.lock().expect("update state poisoned");
        Self::admit(&inner)?;
        if inner.snapshot.flow.phase == UpdatePhase::Ready {
            return Err(UpdateError::Busy);
        }
        let attempt = Self::start(&mut inner);
        inner.snapshot.info = None;
        inner.snapshot.flow.phase = UpdatePhase::Idle;
        inner.snapshot.flow.version.clear();
        inner.snapshot.flow.received_bytes = 0;
        inner.snapshot.flow.total_bytes = None;
        inner.snapshot.flow.percent = None;
        inner.snapshot.flow.message = "Checking for updates...".into();
        self.publish(&mut inner);
        Ok(attempt)
    }

    /// `release` comes only from the native updater's validated release response.
    pub fn checked(&self, attempt: UpdateAttempt, release: Option<(&str, &str)>) -> bool {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.active != Some(attempt) || inner.snapshot.flow.phase != UpdatePhase::Idle {
            return false;
        }
        let latest_version = release
            .map(|(version, _)| version)
            .unwrap_or(&inner.current_version)
            .to_owned();
        let notes_url = release.map(|(_, notes)| notes).unwrap_or("").to_owned();
        inner.snapshot.info = Some(UpdateInfo {
            current_version: inner.current_version.clone(),
            latest_version,
            available: release.is_some(),
            platform: inner.platform.clone(),
            arch: inner.arch.clone(),
            kind: if release.is_some() {
                "release-asset"
            } else {
                "none"
            }
            .into(),
            install_mode: "in-app".into(),
            action_url: notes_url.clone(),
            notes_url,
            checksum_url: None,
            action_label: "Download update".into(),
            checked_at: chrono::Utc::now().to_rfc3339(),
        });
        inner.active = None;
        inner.snapshot.flow.phase = UpdatePhase::Idle;
        inner.snapshot.flow.message.clear();
        self.publish(&mut inner);
        true
    }

    pub fn begin_download(&self, version: &str) -> Result<UpdateAttempt, UpdateError> {
        let mut inner = self.inner.lock().expect("update state poisoned");
        Self::admit(&inner)?;
        if !inner
            .snapshot
            .info
            .as_ref()
            .is_some_and(|info| info.available && info.latest_version == version)
        {
            return Err(UpdateError::StaleRelease);
        }
        if inner.snapshot.flow.phase == UpdatePhase::Ready {
            return Err(UpdateError::Busy);
        }
        let attempt = Self::start(&mut inner);
        inner.snapshot.flow.phase = UpdatePhase::Downloading;
        inner.snapshot.flow.version = version.into();
        inner.snapshot.flow.received_bytes = 0;
        inner.snapshot.flow.total_bytes = None;
        inner.snapshot.flow.percent = None;
        inner.snapshot.flow.message = "Downloading update...".into();
        self.publish(&mut inner);
        Ok(attempt)
    }

    pub fn progress(&self, attempt: UpdateAttempt, received: u64, total: Option<u64>) {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.active != Some(attempt) || inner.snapshot.flow.phase != UpdatePhase::Downloading {
            return;
        }
        if received < inner.snapshot.flow.received_bytes {
            return;
        }
        inner.snapshot.flow.received_bytes = received;
        inner.snapshot.flow.total_bytes = total.filter(|total| *total >= received && *total > 0);
        inner.snapshot.flow.percent = inner
            .snapshot
            .flow
            .total_bytes
            .map(|total| received as f64 / total as f64 * 100.0);
        self.publish(&mut inner);
    }

    pub fn verifying(&self, attempt: UpdateAttempt) {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.active != Some(attempt) || inner.snapshot.flow.phase != UpdatePhase::Downloading {
            return;
        }
        inner.snapshot.flow.message = "Verifying update signature...".into();
        self.publish(&mut inner);
    }

    /// Native adapter calls this only after successful signature verification and
    /// retention of the exact bytes/Update pair. No path or HTTP DTO grants this state.
    pub fn staged(&self, attempt: UpdateAttempt) -> bool {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.active != Some(attempt) || inner.snapshot.flow.phase != UpdatePhase::Downloading {
            return false;
        }
        inner.active = None;
        inner.snapshot.flow.phase = UpdatePhase::Ready;
        inner.snapshot.flow.message = "Signature verified. Restart to install the update.".into();
        self.publish(&mut inner);
        true
    }

    /// Called after shell admission has atomically excluded all other active work.
    pub fn begin_apply(&self) -> Result<UpdateAttempt, UpdateError> {
        let mut inner = self.inner.lock().expect("update state poisoned");
        Self::admit(&inner)?;
        if inner.snapshot.flow.phase != UpdatePhase::Ready {
            return Err(UpdateError::NotReady);
        }
        inner.terminal = true;
        let attempt = Self::start(&mut inner);
        inner.snapshot.flow.phase = UpdatePhase::Applying;
        inner.snapshot.flow.message = "Installing verified update...".into();
        self.publish(&mut inner);
        Ok(attempt)
    }

    pub fn installed(&self, attempt: UpdateAttempt) {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.active != Some(attempt) || inner.snapshot.flow.phase != UpdatePhase::Applying {
            return;
        }
        inner.active = None;
        inner.snapshot.flow.phase = UpdatePhase::RestartPending;
        inner.snapshot.flow.message = "Update installed. Restart to finish.".into();
        self.publish(&mut inner);
    }

    pub fn failed(&self, attempt: UpdateAttempt, safe_message: &str) {
        let mut inner = self.inner.lock().expect("update state poisoned");
        if inner.active != Some(attempt) {
            return;
        }
        inner.active = None;
        inner.snapshot.flow.phase = UpdatePhase::Failed;
        inner.snapshot.flow.message = safe_message.chars().take(500).collect();
        self.publish(&mut inner);
    }

    fn admit(inner: &Inner) -> Result<(), UpdateError> {
        if !inner.snapshot.flow.supported {
            return Err(UpdateError::Unsupported);
        }
        if inner.active.is_some() || inner.terminal {
            return Err(UpdateError::Busy);
        }
        Ok(())
    }

    fn start(inner: &mut Inner) -> UpdateAttempt {
        inner.attempt = inner
            .attempt
            .checked_add(1)
            .expect("update attempt exhausted");
        let attempt = UpdateAttempt(inner.attempt);
        inner.active = Some(attempt);
        attempt
    }

    fn publish(&self, inner: &mut Inner) {
        let flow = &mut inner.snapshot.flow;
        flow.revision = flow
            .revision
            .checked_add(1)
            .expect("update revision exhausted");
        flow.can_check = flow.supported
            && inner.active.is_none()
            && !inner.terminal
            && flow.phase != UpdatePhase::Ready;
        flow.can_download = flow.can_check
            && inner
                .snapshot
                .info
                .as_ref()
                .is_some_and(|info| info.available);
        flow.can_apply =
            flow.supported && flow.phase == UpdatePhase::Ready && inner.active.is_none();
        flow.can_restart = inner.terminal && inner.active.is_none();
        self.changes.send_replace(inner.snapshot.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> UpdateService {
        let service = UpdateService::new("1.0.0", "linux", "x86_64");
        service.configure(None);
        service
    }

    fn available(service: &UpdateService) {
        let check = service.begin_check().unwrap();
        service.checked(check, Some(("1.1.0", "https://example.com/rewrite/1.1.0")));
    }

    #[test]
    fn unsupported_check_is_not_no_update_success() {
        let service = UpdateService::new("1.0.0", "browser", "unknown");
        assert_eq!(service.begin_check(), Err(UpdateError::Unsupported));
        assert!(service.snapshot().info.is_none());
        assert!(!service.snapshot().flow.can_download);
    }

    #[test]
    fn unverified_download_cannot_apply_and_failed_verification_can_retry() {
        let service = configured();
        available(&service);
        let download = service.begin_download("1.1.0").unwrap();
        service.progress(download, 100, Some(100));
        service.verifying(download);
        assert_eq!(service.begin_apply(), Err(UpdateError::Busy));
        service.failed(download, "Update signature verification failed.");
        assert_eq!(service.begin_apply(), Err(UpdateError::NotReady));
        assert!(service.begin_download("1.1.0").is_ok());
    }

    #[test]
    fn stale_completions_do_not_overwrite_a_new_attempt_or_publish_twice() {
        let service = configured();
        let first = service.begin_check().unwrap();
        service.failed(first, "Check failed.");
        let second = service.begin_check().unwrap();
        service.checked(first, Some(("99.0.0", "")));
        assert!(service.snapshot().info.is_none());
        service.checked(second, None);
        let snapshot = service.snapshot();
        service.failed(second, "Late failure.");
        assert_eq!(service.snapshot(), snapshot);
    }

    #[test]
    fn applying_retains_terminal_fence_on_failure_and_restart_loses_only_memory_stage() {
        let service = configured();
        available(&service);
        let download = service.begin_download("1.1.0").unwrap();
        service.staged(download);
        let apply = service.begin_apply().unwrap();
        service.failed(apply, "Installation failed. Restart before continuing.");
        assert!(service.snapshot().flow.can_restart);
        assert_eq!(service.begin_check(), Err(UpdateError::Busy));
        assert_eq!(service.begin_download("1.1.0"), Err(UpdateError::Busy));
        let restarted = configured();
        assert_eq!(restarted.begin_apply(), Err(UpdateError::NotReady));
    }

    #[test]
    fn subscription_observes_current_snapshot_and_progress_never_regresses() {
        let service = configured();
        available(&service);
        let download = service.begin_download("1.1.0").unwrap();
        service.progress(download, 80, Some(100));
        let subscription = service.subscribe();
        assert_eq!(*subscription.borrow(), service.snapshot());
        service.progress(download, 20, Some(100));
        assert_eq!(service.snapshot().flow.received_bytes, 80);
        service.progress(download, 110, Some(100));
        assert_eq!(service.snapshot().flow.percent, None);
    }

    #[test]
    fn package_configuration_cannot_change_under_a_verified_stage() {
        let service = configured();
        available(&service);
        let download = service.begin_download("1.1.0").unwrap();
        service.staged(download);
        let ready = service.snapshot();
        service.configure(Some("A different feed was selected."));
        assert_eq!(service.snapshot(), ready);
        assert!(service.begin_apply().is_ok());
    }

    #[test]
    fn callbacks_cannot_cross_check_download_and_install_phases() {
        let service = configured();
        let check = service.begin_check().unwrap();
        let checking = service.snapshot();
        service.verifying(check);
        service.staged(check);
        service.installed(check);
        assert_eq!(service.snapshot(), checking);
        service.checked(check, Some(("1.1.0", "")));
        let download = service.begin_download("1.1.0").unwrap();
        let downloading = service.snapshot();
        service.checked(download, None);
        service.installed(download);
        assert_eq!(service.snapshot(), downloading);
        service.staged(download);
        let apply = service.begin_apply().unwrap();
        let applying = service.snapshot();
        service.checked(apply, None);
        service.verifying(apply);
        service.staged(apply);
        assert_eq!(service.snapshot(), applying);
        service.installed(apply);
        assert_eq!(service.snapshot().flow.phase, UpdatePhase::RestartPending);
        assert!(service.snapshot().flow.can_restart);
        assert!(!service.snapshot().flow.can_apply);
    }
}
