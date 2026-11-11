use crate::cancellation::{Cancellation, ThreadCancellation};
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(super) struct RuntimeCancellationSet {
    first: Cancellation,
    second: Option<Cancellation>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum RuntimeTestHookPoint {
    BeforePublicationClaim,
    Publication,
}

#[cfg(test)]
type RuntimeTestHookKey = (RuntimeTestHookPoint, PathBuf);

#[cfg(test)]
struct RuntimeTestHook {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
    claimed: Arc<AtomicBool>,
}

#[cfg(test)]
static RUNTIME_TEST_HOOKS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<RuntimeTestHookKey, RuntimeTestHook>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) struct RuntimeTestGate {
    key: RuntimeTestHookKey,
    reached: tokio::sync::oneshot::Receiver<()>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
    claimed: Arc<AtomicBool>,
}

#[cfg(test)]
impl RuntimeTestGate {
    pub(crate) async fn wait_until_reached(&mut self) {
        (&mut self.reached)
            .await
            .expect("runtime test hook should reach its gate");
    }

    pub(crate) fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[cfg(test)]
impl Drop for RuntimeTestGate {
    fn drop(&mut self) {
        if self.claimed.load(Ordering::Acquire) {
            return;
        }
        let mut hooks = runtime_test_hooks()
            .lock()
            .expect("runtime test hook registry");
        if !self.claimed.load(Ordering::Acquire) {
            hooks.remove(&self.key);
        }
    }
}

#[cfg(test)]
fn runtime_test_hooks() -> &'static std::sync::Mutex<HashMap<RuntimeTestHookKey, RuntimeTestHook>> {
    RUNTIME_TEST_HOOKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(super) fn arm_runtime_test_hook(point: RuntimeTestHookPoint, path: &Path) -> RuntimeTestGate {
    let key = (point, path.to_path_buf());
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let claimed = Arc::new(AtomicBool::new(false));
    let hook = RuntimeTestHook {
        reached: reached_tx,
        release: release_rx,
        claimed: Arc::clone(&claimed),
    };
    let mut hooks = runtime_test_hooks()
        .lock()
        .expect("runtime test hook registry");
    assert!(
        !hooks.contains_key(&key),
        "runtime test hook path is already armed"
    );
    hooks.insert(key.clone(), hook);
    drop(hooks);
    RuntimeTestGate {
        key,
        reached: reached_rx,
        release: Some(release_tx),
        claimed,
    }
}

#[cfg(test)]
pub(super) async fn wait_for_runtime_test_hook(point: RuntimeTestHookPoint, path: &Path) {
    let hook = runtime_test_hooks()
        .lock()
        .expect("runtime test hook registry")
        .remove(&(point, path.to_path_buf()));
    let Some(hook) = hook else {
        return;
    };
    hook.claimed.store(true, Ordering::Release);
    let _ = hook.reached.send(());
    let _ = hook.release.await;
}

impl RuntimeCancellationSet {
    pub(super) fn single(cancellation: Cancellation) -> Self {
        Self {
            first: cancellation,
            second: None,
        }
    }

    pub(super) fn pair(first: Cancellation, second: Cancellation) -> Self {
        Self {
            first,
            second: Some(second),
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.first.is_cancelled() || self.second.as_ref().is_some_and(Cancellation::is_cancelled)
    }

    pub(super) async fn cancelled(&mut self) {
        if let Some(second) = self.second.as_mut() {
            tokio::select! {
                biased;
                () = self.first.cancelled() => {},
                () = second.cancelled() => {},
            }
        } else {
            self.first.cancelled().await;
        }
    }

    pub(super) fn thread(&self) -> ThreadCancellation {
        let thread = self.first.thread();
        match &self.second {
            Some(second) => thread.or(second),
            None => thread,
        }
    }
}
