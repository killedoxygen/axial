use std::path::{Path, PathBuf};
use std::sync::Mutex;

const UPDATE_STAGING_DIR_NAME: &str = "updates";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateFlowPhase {
    Idle,
    Downloading,
    Ready,
    Applying,
    RestartPending,
    Failed,
}

#[derive(Clone, Debug)]
pub struct UpdateFlowSnapshot {
    pub phase: UpdateFlowPhase,
    pub version: String,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub message: String,
    pub staged_path: Option<PathBuf>,
}

impl UpdateFlowSnapshot {
    fn idle() -> Self {
        Self {
            phase: UpdateFlowPhase::Idle,
            version: String::new(),
            received_bytes: 0,
            total_bytes: None,
            message: String::new(),
            staged_path: None,
        }
    }
}

pub struct UpdaterStore {
    staging_dir: PathBuf,
    inner: Mutex<UpdaterInner>,
}

struct UpdaterInner {
    flow: UpdateFlowSnapshot,
    download_epoch: u64,
}

impl UpdaterStore {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            staging_dir: config_dir.join(UPDATE_STAGING_DIR_NAME),
            inner: Mutex::new(UpdaterInner {
                flow: UpdateFlowSnapshot::idle(),
                download_epoch: 0,
            }),
        }
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    pub fn snapshot(&self) -> UpdateFlowSnapshot {
        self.inner.lock().expect("updater lock").flow.clone()
    }

    pub fn begin_download(&self, version: &str) -> Result<u64, &'static str> {
        let mut inner = self.inner.lock().expect("updater lock");
        match inner.flow.phase {
            UpdateFlowPhase::Downloading | UpdateFlowPhase::Applying => {
                return Err("an update operation is already in progress");
            }
            UpdateFlowPhase::RestartPending => {
                return Err("an update is already applied; restart to finish");
            }
            UpdateFlowPhase::Idle | UpdateFlowPhase::Ready | UpdateFlowPhase::Failed => {}
        }
        inner.download_epoch += 1;
        inner.flow = UpdateFlowSnapshot {
            phase: UpdateFlowPhase::Downloading,
            version: version.to_string(),
            received_bytes: 0,
            total_bytes: None,
            message: String::new(),
            staged_path: None,
        };
        Ok(inner.download_epoch)
    }

    pub fn set_download_progress(&self, epoch: u64, received_bytes: u64, total_bytes: Option<u64>) {
        let mut inner = self.inner.lock().expect("updater lock");
        if inner.download_epoch != epoch || inner.flow.phase != UpdateFlowPhase::Downloading {
            return;
        }
        inner.flow.received_bytes = received_bytes;
        inner.flow.total_bytes = total_bytes;
    }

    pub fn mark_ready(&self, epoch: u64, staged_path: PathBuf) {
        let mut inner = self.inner.lock().expect("updater lock");
        if inner.download_epoch != epoch || inner.flow.phase != UpdateFlowPhase::Downloading {
            return;
        }
        inner.flow.phase = UpdateFlowPhase::Ready;
        inner.flow.received_bytes = inner.flow.total_bytes.unwrap_or(inner.flow.received_bytes);
        inner.flow.staged_path = Some(staged_path);
    }

    pub fn mark_download_failed(&self, epoch: u64, message: &'static str) {
        let mut inner = self.inner.lock().expect("updater lock");
        if inner.download_epoch != epoch || inner.flow.phase != UpdateFlowPhase::Downloading {
            return;
        }
        inner.flow.phase = UpdateFlowPhase::Failed;
        inner.flow.message = message.to_string();
        inner.flow.staged_path = None;
    }

    pub fn begin_apply(&self) -> Result<PathBuf, &'static str> {
        let mut inner = self.inner.lock().expect("updater lock");
        if inner.flow.phase != UpdateFlowPhase::Ready {
            return Err("no staged update is ready to apply");
        }
        let Some(staged_path) = inner.flow.staged_path.clone() else {
            return Err("no staged update is ready to apply");
        };
        inner.flow.phase = UpdateFlowPhase::Applying;
        Ok(staged_path)
    }

    pub fn mark_restart_pending(&self) {
        let mut inner = self.inner.lock().expect("updater lock");
        if inner.flow.phase != UpdateFlowPhase::Applying {
            return;
        }
        inner.flow.phase = UpdateFlowPhase::RestartPending;
        inner.flow.staged_path = None;
    }

    pub fn mark_apply_failed(&self, message: &'static str) {
        let mut inner = self.inner.lock().expect("updater lock");
        if inner.flow.phase != UpdateFlowPhase::Applying {
            return;
        }
        inner.flow.phase = UpdateFlowPhase::Failed;
        inner.flow.message = message.to_string();
        inner.flow.staged_path = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UpdaterStore {
        UpdaterStore::new(Path::new("/tmp/axial-test-config"))
    }

    #[test]
    fn download_lifecycle_reaches_ready() {
        let store = store();
        let epoch = store.begin_download("1.2.4").expect("begin download");
        store.set_download_progress(epoch, 10, Some(100));
        let snapshot = store.snapshot();
        assert_eq!(snapshot.phase, UpdateFlowPhase::Downloading);
        assert_eq!(snapshot.received_bytes, 10);
        assert_eq!(snapshot.total_bytes, Some(100));

        store.mark_ready(
            epoch,
            PathBuf::from("/tmp/axial-test-config/updates/staged"),
        );
        let snapshot = store.snapshot();
        assert_eq!(snapshot.phase, UpdateFlowPhase::Ready);
        assert_eq!(snapshot.received_bytes, 100);
        assert!(snapshot.staged_path.is_some());
    }

    #[test]
    fn concurrent_download_is_refused() {
        let store = store();
        store.begin_download("1.2.4").expect("begin download");
        assert!(store.begin_download("1.2.4").is_err());
    }

    #[test]
    fn stale_epoch_updates_are_ignored() {
        let store = store();
        let stale = store.begin_download("1.2.4").expect("begin download");
        store.mark_download_failed(stale, "update download failed");
        let fresh = store.begin_download("1.2.5").expect("retry download");
        store.set_download_progress(stale, 999, Some(1000));
        store.mark_ready(stale, PathBuf::from("/stale"));
        let snapshot = store.snapshot();
        assert_eq!(snapshot.phase, UpdateFlowPhase::Downloading);
        assert_eq!(snapshot.received_bytes, 0);
        assert_eq!(snapshot.version, "1.2.5");
        store.set_download_progress(fresh, 5, None);
        assert_eq!(store.snapshot().received_bytes, 5);
    }

    #[test]
    fn apply_requires_staged_update() {
        let store = store();
        assert!(store.begin_apply().is_err());

        let epoch = store.begin_download("1.2.4").expect("begin download");
        store.mark_ready(epoch, PathBuf::from("/tmp/staged"));
        let staged = store.begin_apply().expect("begin apply");
        assert_eq!(staged, PathBuf::from("/tmp/staged"));
        assert_eq!(store.snapshot().phase, UpdateFlowPhase::Applying);

        store.mark_restart_pending();
        let snapshot = store.snapshot();
        assert_eq!(snapshot.phase, UpdateFlowPhase::RestartPending);
        assert!(snapshot.staged_path.is_none());
        assert!(store.begin_download("1.2.5").is_err());
    }

    #[test]
    fn failed_apply_reports_failure() {
        let store = store();
        let epoch = store.begin_download("1.2.4").expect("begin download");
        store.mark_ready(epoch, PathBuf::from("/tmp/staged"));
        store.begin_apply().expect("begin apply");
        store.mark_apply_failed("could not apply the staged update");
        let snapshot = store.snapshot();
        assert_eq!(snapshot.phase, UpdateFlowPhase::Failed);
        assert_eq!(snapshot.message, "could not apply the staged update");
    }
}
