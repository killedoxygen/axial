//! Per-thread low disk-I/O-priority ownership for blocking Execution work.

#[derive(Debug, Eq, PartialEq)]
pub(super) enum LowPriorityOutcome<Output> {
    Complete(Output),
    EnterFailed,
    RestoreFailed(Output),
}

pub(super) trait LowPriorityPlatform: Send + 'static {
    type Saved: Send + 'static;

    fn enter(&self) -> Result<Self::Saved, ()>;
    fn restore(&self, saved: &Self::Saved) -> Result<(), ()>;
}

pub(super) struct SystemLowPriorityPlatform;

#[cfg(target_os = "macos")]
const MACOS_IOPOL_THROTTLE: std::ffi::c_int = 3;

pub(super) fn run_at_low_priority<Platform, Work, Output>(
    platform: Platform,
    work: Work,
) -> LowPriorityOutcome<Output>
where
    Platform: LowPriorityPlatform,
    Work: FnOnce() -> Output,
{
    let guard = match LowPriorityGuard::enter(platform) {
        Ok(guard) => guard,
        Err(()) => return LowPriorityOutcome::EnterFailed,
    };
    let output = work();
    if guard.restore().is_ok() {
        LowPriorityOutcome::Complete(output)
    } else {
        LowPriorityOutcome::RestoreFailed(output)
    }
}

struct LowPriorityGuard<Platform: LowPriorityPlatform> {
    platform: Platform,
    saved: Option<Platform::Saved>,
}

impl<Platform: LowPriorityPlatform> LowPriorityGuard<Platform> {
    fn enter(platform: Platform) -> Result<Self, ()> {
        let saved = platform.enter()?;
        Ok(Self {
            platform,
            saved: Some(saved),
        })
    }

    fn restore(mut self) -> Result<(), ()> {
        let saved = self.saved.as_ref().expect("low-priority guard is armed");
        self.platform.restore(saved)?;
        self.saved = None;
        Ok(())
    }
}

impl<Platform: LowPriorityPlatform> Drop for LowPriorityGuard<Platform> {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.as_ref() {
            let _ = self.platform.restore(saved);
        }
    }
}

#[cfg(windows)]
impl LowPriorityPlatform for SystemLowPriorityPlatform {
    type Saved = ();

    fn enter(&self) -> Result<Self::Saved, ()> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };

        // SAFETY: the pseudo-handle targets only the calling blocking-worker thread.
        let entered =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) != 0 };
        entered.then_some(()).ok_or(())
    }

    fn restore(&self, _saved: &Self::Saved) -> Result<(), ()> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
        };

        // SAFETY: BACKGROUND_END restores the calling thread paired with BEGIN.
        let restored =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) != 0 };
        restored.then_some(()).ok_or(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl LowPriorityPlatform for SystemLowPriorityPlatform {
    type Saved = i32;

    fn enter(&self) -> Result<Self::Saved, ()> {
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        const IOPRIO_IDLE: libc::c_int = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;

        // SAFETY: who=0 addresses only the calling thread; arguments match the syscall ABI.
        let saved = unsafe { libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0) };
        if saved == -1 {
            return Err(());
        }
        // SAFETY: who=0 addresses only the calling thread; IOPRIO_IDLE is a valid encoded class.
        let entered =
            unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_IDLE) };
        if entered == -1 {
            Err(())
        } else {
            Ok(saved as i32)
        }
    }

    fn restore(&self, saved: &Self::Saved) -> Result<(), ()> {
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;

        // SAFETY: who=0 addresses only the calling thread and saved is the exact prior encoding.
        let restored =
            unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, *saved) };
        (restored != -1).then_some(()).ok_or(())
    }
}

#[cfg(target_os = "macos")]
impl LowPriorityPlatform for SystemLowPriorityPlatform {
    type Saved = std::ffi::c_int;

    fn enter(&self) -> Result<Self::Saved, ()> {
        const IOPOL_TYPE_DISK: std::ffi::c_int = 0;
        const IOPOL_SCOPE_THREAD: std::ffi::c_int = 1;

        unsafe extern "C" {
            fn getiopolicy_np(iotype: std::ffi::c_int, scope: std::ffi::c_int) -> std::ffi::c_int;
            fn setiopolicy_np(
                iotype: std::ffi::c_int,
                scope: std::ffi::c_int,
                policy: std::ffi::c_int,
            ) -> std::ffi::c_int;
        }

        // SAFETY: these libc calls get and set policy only for the calling worker thread.
        let saved = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD) };
        if saved == -1 {
            return Err(());
        }
        // SAFETY: IOPOL_THROTTLE is a valid disk policy for the calling thread.
        let entered =
            unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, MACOS_IOPOL_THROTTLE) };
        if entered == -1 { Err(()) } else { Ok(saved) }
    }

    fn restore(&self, saved: &Self::Saved) -> Result<(), ()> {
        const IOPOL_TYPE_DISK: std::ffi::c_int = 0;
        const IOPOL_SCOPE_THREAD: std::ffi::c_int = 1;

        unsafe extern "C" {
            fn setiopolicy_np(
                iotype: std::ffi::c_int,
                scope: std::ffi::c_int,
                policy: std::ffi::c_int,
            ) -> std::ffi::c_int;
        }

        // SAFETY: saved is the exact prior policy observed for the calling worker thread.
        let restored = unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, *saved) };
        (restored != -1).then_some(()).ok_or(())
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
impl LowPriorityPlatform for SystemLowPriorityPlatform {
    type Saved = ();

    fn enter(&self) -> Result<Self::Saved, ()> {
        Ok(())
    }

    fn restore(&self, _saved: &Self::Saved) -> Result<(), ()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_disk_policy_constants_match_the_system_abi() {
        const IOPOL_PASSIVE: std::ffi::c_int = 2;

        assert_eq!(IOPOL_PASSIVE, 2);
        assert_eq!(MACOS_IOPOL_THROTTLE, 3);
        assert_ne!(IOPOL_PASSIVE, MACOS_IOPOL_THROTTLE);
    }

    #[derive(Clone)]
    struct ScriptedPlatform {
        events: Arc<Mutex<Vec<&'static str>>>,
        enter_fails: bool,
        restore_failures: Arc<Mutex<usize>>,
    }

    impl ScriptedPlatform {
        fn new(enter_fails: bool, restore_failures: usize) -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                enter_fails,
                restore_failures: Arc::new(Mutex::new(restore_failures)),
            }
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.lock().expect("events").clone()
        }
    }

    impl LowPriorityPlatform for ScriptedPlatform {
        type Saved = ();

        fn enter(&self) -> Result<Self::Saved, ()> {
            self.events.lock().expect("events").push("enter");
            if self.enter_fails { Err(()) } else { Ok(()) }
        }

        fn restore(&self, _saved: &Self::Saved) -> Result<(), ()> {
            self.events.lock().expect("events").push("restore");
            let mut failures = self.restore_failures.lock().expect("restore failures");
            if *failures == 0 {
                Ok(())
            } else {
                *failures -= 1;
                Err(())
            }
        }
    }

    #[test]
    fn low_priority_scope_orders_enter_work_and_restore() {
        let platform = ScriptedPlatform::new(false, 0);
        let work_platform = platform.clone();

        let outcome = run_at_low_priority(platform.clone(), move || {
            work_platform.events.lock().expect("events").push("work");
            7
        });

        assert_eq!(outcome, LowPriorityOutcome::Complete(7));
        assert_eq!(platform.events(), vec!["enter", "work", "restore"]);
    }

    #[test]
    fn low_priority_enter_failure_runs_no_work() {
        let platform = ScriptedPlatform::new(true, 0);
        let work_platform = platform.clone();

        let outcome = run_at_low_priority(platform.clone(), move || {
            work_platform.events.lock().expect("events").push("work");
        });

        assert_eq!(outcome, LowPriorityOutcome::EnterFailed);
        assert_eq!(platform.events(), vec!["enter"]);
    }

    #[test]
    fn explicit_restore_failure_is_reported_and_drop_retries() {
        let platform = ScriptedPlatform::new(false, 1);

        let outcome = run_at_low_priority(platform.clone(), || 11);

        assert_eq!(outcome, LowPriorityOutcome::RestoreFailed(11));
        assert_eq!(platform.events(), vec!["enter", "restore", "restore"]);
    }

    #[test]
    fn panic_restores_priority_during_unwind() {
        let platform = ScriptedPlatform::new(false, 0);

        let panic = std::panic::catch_unwind({
            let platform = platform.clone();
            move || run_at_low_priority(platform, || panic!("injected work panic"))
        });

        assert!(panic.is_err());
        assert_eq!(platform.events(), vec!["enter", "restore"]);
    }
}
