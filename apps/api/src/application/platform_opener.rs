use crate::{application::filesystem::BlockingFilesystemAdmission, state::ProducerLease};
use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::{oneshot, watch};

const MAX_ACTIVE_NATIVE_OPENERS: usize = 8;
const NATIVE_OPENER_POLL_INTERVAL: Duration = Duration::from_millis(50);
static ACTIVE_NATIVE_OPENERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct NativeFolderProjection {
    path: PathBuf,
    validate: Box<dyn FnMut() -> io::Result<()> + Send>,
}

impl NativeFolderProjection {
    pub(crate) fn new<Validate>(path: PathBuf, validate: Validate) -> Self
    where
        Validate: FnMut() -> io::Result<()> + Send + 'static,
    {
        Self {
            path,
            validate: Box::new(validate),
        }
    }

    fn validate(&mut self) -> io::Result<()> {
        (self.validate)()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeFolderOpenError {
    Capacity,
    PhysicalTask,
    Prepare,
    Spawn,
}

struct NativeOpenerSlot;

impl NativeOpenerSlot {
    fn try_acquire() -> Result<Self, NativeFolderOpenError> {
        ACTIVE_NATIVE_OPENERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_NATIVE_OPENERS).then_some(active + 1)
            })
            .map_err(|_| NativeFolderOpenError::Capacity)?;
        Ok(Self)
    }
}

impl Drop for NativeOpenerSlot {
    fn drop(&mut self) {
        let previous = ACTIVE_NATIVE_OPENERS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "native opener slot count underflowed");
    }
}

struct TrackedNativeOpener {
    child: Child,
    _projection: NativeFolderProjection,
    post_spawn_valid: bool,
    _slot: NativeOpenerSlot,
}

pub(crate) async fn open_native_folder_owned<Prepare>(
    admission: BlockingFilesystemAdmission,
    producer: ProducerLease,
    shutdown: watch::Receiver<bool>,
    prepare: Prepare,
) -> Result<(), NativeFolderOpenError>
where
    Prepare: FnOnce() -> io::Result<NativeFolderProjection> + Send + 'static,
{
    open_native_folder_owned_with(
        admission,
        producer,
        shutdown,
        prepare,
        native_folder_command,
    )
    .await
}

async fn open_native_folder_owned_with<Prepare, BuildCommand>(
    admission: BlockingFilesystemAdmission,
    producer: ProducerLease,
    shutdown: watch::Receiver<bool>,
    prepare: Prepare,
    build_command: BuildCommand,
) -> Result<(), NativeFolderOpenError>
where
    Prepare: FnOnce() -> io::Result<NativeFolderProjection> + Send + 'static,
    BuildCommand: FnOnce(&Path) -> Command + Send + 'static,
{
    let slot = NativeOpenerSlot::try_acquire()?;
    let (child_tx, child_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    producer.spawn(own_native_opener(child_rx, ready_tx, shutdown));

    admission
        .run(move || {
            let mut projection = prepare().map_err(|_| NativeFolderOpenError::Prepare)?;
            projection
                .validate()
                .map_err(|_| NativeFolderOpenError::Prepare)?;
            let mut command = build_command(&projection.path);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = command.spawn().map_err(|_| NativeFolderOpenError::Spawn)?;
            let post_spawn_valid = projection.validate().is_ok();
            let tracked = TrackedNativeOpener {
                child,
                _projection: projection,
                post_spawn_valid,
                _slot: slot,
            };
            if let Err(error) = child_tx.send(tracked) {
                let mut tracked = error;
                let _ = tracked.child.kill();
                let _ = tracked.child.wait();
                return Err(NativeFolderOpenError::Spawn);
            }
            Ok(())
        })
        .await
        .map_err(|_| NativeFolderOpenError::PhysicalTask)??;

    ready_rx.await.unwrap_or(Err(NativeFolderOpenError::Spawn))
}

async fn own_native_opener(
    child_rx: oneshot::Receiver<TrackedNativeOpener>,
    ready_tx: oneshot::Sender<Result<(), NativeFolderOpenError>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Ok(mut tracked) = child_rx.await else {
        return;
    };
    let mut stopping = *shutdown.borrow();
    if tracked.post_spawn_valid {
        let _ = ready_tx.send(Ok(()));
    } else {
        let _ = ready_tx.send(Err(NativeFolderOpenError::Prepare));
        stopping = true;
    }

    let mut kill_sent = false;
    loop {
        if stopping && !kill_sent {
            let _ = tracked.child.kill();
            kill_sent = true;
        }
        match tracked.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => {
                let _ = tracked.child.kill();
                let _ = tracked.child.wait();
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(NATIVE_OPENER_POLL_INTERVAL) => {}
            changed = shutdown.changed(), if !stopping => {
                stopping = changed.is_err() || *shutdown.borrow();
            }
        }
    }
}

fn native_folder_command(path: &Path) -> Command {
    if cfg!(target_os = "windows") {
        let mut command = Command::new("explorer.exe");
        command.arg(path);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(path);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    }
}

#[cfg(test)]
pub(crate) fn active_native_openers_for_test() -> usize {
    ACTIVE_NATIVE_OPENERS.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::filesystem::admit_blocking_filesystem,
        state::{AppLifecycle, AppLifecyclePhase},
    };
    use std::{
        fs,
        sync::{Arc, Condvar, Mutex},
    };

    const HELPER_ENV: &str = "AXIAL_NATIVE_OPENER_HELPER";
    const HELPER_MARKER_ENV: &str = "AXIAL_NATIVE_OPENER_MARKER";
    const HELPER_TEST: &str =
        "application::platform_opener::tests::native_filesystem_contract_native_opener_child";
    static NATIVE_OPENER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    #[ignore]
    fn native_filesystem_contract_native_opener_child() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let marker = std::env::var_os(HELPER_MARKER_ENV).expect("native opener marker");
        fs::write(marker, std::process::id().to_string()).expect("write native opener marker");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_filesystem_contract_native_openers_are_bounded_and_shutdown_owned() {
        let _serial = NATIVE_OPENER_TEST_LOCK.lock().await;
        let temporary = tempfile::tempdir().expect("native opener temporary directory");
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let handoff = request.producer_handoff();

        for index in 0..MAX_ACTIVE_NATIVE_OPENERS {
            let marker = temporary.path().join(format!("opener-{index}.marker"));
            let command_marker = marker.clone();
            open_native_folder_owned_with(
                admit_blocking_filesystem()
                    .await
                    .expect("filesystem admission"),
                handoff.try_claim().expect("claim opener producer"),
                lifecycle.subscribe_shutdown(),
                || Ok(NativeFolderProjection::new(std::env::temp_dir(), || Ok(()))),
                move |_| helper_command(&command_marker),
            )
            .await
            .expect("start retained native opener");
        }
        assert_eq!(active_native_openers_for_test(), MAX_ACTIVE_NATIVE_OPENERS);
        for index in 0..MAX_ACTIVE_NATIVE_OPENERS {
            wait_for_marker(&temporary.path().join(format!("opener-{index}.marker"))).await;
        }
        let capacity_error = open_native_folder_owned_with(
            admit_blocking_filesystem()
                .await
                .expect("filesystem admission"),
            handoff.try_claim().expect("claim capacity probe producer"),
            lifecycle.subscribe_shutdown(),
            || Ok(NativeFolderProjection::new(std::env::temp_dir(), || Ok(()))),
            |_| helper_command(Path::new("unused")),
        )
        .await
        .expect_err("ninth native opener must be refused");
        assert!(matches!(capacity_error, NativeFolderOpenError::Capacity));

        drop(request);
        lifecycle.quiesce().await.expect("shutdown reaps openers");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
        assert_eq!(active_native_openers_for_test(), 0);
        for index in 0..MAX_ACTIVE_NATIVE_OPENERS {
            assert!(
                temporary
                    .path()
                    .join(format!("opener-{index}.marker"))
                    .is_file()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_filesystem_contract_slow_opener_preparation_does_not_block_async_heartbeat() {
        let _serial = NATIVE_OPENER_TEST_LOCK.lock().await;
        let temporary = tempfile::tempdir().expect("native opener temporary directory");
        let marker = temporary.path().join("slow.marker");
        let command_marker = marker.clone();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_work = Arc::clone(&gate);
        let (started_tx, started_rx) = oneshot::channel();
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let opener = tokio::spawn(open_native_folder_owned_with(
            admit_blocking_filesystem()
                .await
                .expect("filesystem admission"),
            request
                .producer_handoff()
                .try_claim()
                .expect("claim opener producer"),
            lifecycle.subscribe_shutdown(),
            move || {
                let _ = started_tx.send(());
                let (lock, wake) = &*gate_work;
                let released = lock.lock().expect("lock preparation gate");
                drop(
                    wake.wait_while(released, |released| !*released)
                        .expect("wait for preparation release"),
                );
                Ok(NativeFolderProjection::new(std::env::temp_dir(), || Ok(())))
            },
            move |_| helper_command(&command_marker),
        ));
        started_rx.await.expect("preparation started");
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::time::sleep(Duration::from_millis(20)),
        )
        .await
        .expect("async heartbeat remains responsive");
        let (lock, wake) = &*gate;
        *lock.lock().expect("release preparation") = true;
        wake.notify_all();
        opener
            .await
            .expect("join opener request")
            .expect("open native folder");
        wait_for_marker(&marker).await;

        drop(request);
        lifecycle.quiesce().await.expect("shutdown reaps opener");
        assert!(marker.is_file());
        assert_eq!(active_native_openers_for_test(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_filesystem_contract_cross_owner_cancelled_request_retains_opener_owner() {
        let _serial = NATIVE_OPENER_TEST_LOCK.lock().await;
        let temporary = tempfile::tempdir().expect("native opener temporary directory");
        let marker = temporary.path().join("cancelled.marker");
        let command_marker = marker.clone();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_work = Arc::clone(&gate);
        let (started_tx, started_rx) = oneshot::channel();
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let opener = tokio::spawn(open_native_folder_owned_with(
            admit_blocking_filesystem()
                .await
                .expect("filesystem admission"),
            request
                .producer_handoff()
                .try_claim()
                .expect("claim opener producer"),
            lifecycle.subscribe_shutdown(),
            move || {
                let _ = started_tx.send(());
                let (lock, wake) = &*gate_work;
                let released = lock.lock().expect("lock preparation gate");
                drop(
                    wake.wait_while(released, |released| !*released)
                        .expect("wait for preparation release"),
                );
                Ok(NativeFolderProjection::new(std::env::temp_dir(), || Ok(())))
            },
            move |_| helper_command(&command_marker),
        ));
        started_rx.await.expect("preparation started");
        opener.abort();
        assert!(opener.await.expect_err("request cancelled").is_cancelled());
        let (lock, wake) = &*gate;
        *lock.lock().expect("release preparation") = true;
        wake.notify_all();
        wait_for_marker(&marker).await;

        drop(request);
        lifecycle
            .quiesce()
            .await
            .expect("shutdown reaps retained opener");
        assert_eq!(active_native_openers_for_test(), 0);
    }

    fn helper_command(marker: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg(HELPER_TEST)
            .arg("--ignored")
            .arg("--nocapture")
            .env(HELPER_ENV, "1")
            .env(HELPER_MARKER_ENV, marker);
        command
    }

    async fn wait_for_marker(marker: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native opener child starts");
    }
}
