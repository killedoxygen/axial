use std::io;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchPriorityMode {
    #[cfg(windows)]
    BelowNormalUntilBoot,
    #[cfg(not(windows))]
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PriorityPromotion {
    #[cfg(windows)]
    Promoted,
    #[cfg(windows)]
    MissingPid,
    #[cfg(not(windows))]
    Noop,
}

pub(super) fn configure_start_priority(command: &mut Command) -> io::Result<LaunchPriorityMode> {
    platform::configure_start_priority(command)
}

pub(super) fn promote_after_boot(pid: Option<u32>) -> io::Result<PriorityPromotion> {
    platform::promote_after_boot(pid)
}

#[cfg(windows)]
mod platform {
    use super::{LaunchPriorityMode, PriorityPromotion};
    use std::io;
    use tokio::process::Command;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_INFORMATION,
        SetPriorityClass,
    };

    pub(super) fn configure_start_priority(
        command: &mut Command,
    ) -> io::Result<LaunchPriorityMode> {
        command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        Ok(LaunchPriorityMode::BelowNormalUntilBoot)
    }

    pub(super) fn promote_after_boot(pid: Option<u32>) -> io::Result<PriorityPromotion> {
        let Some(pid) = pid else {
            return Ok(PriorityPromotion::MissingPid);
        };

        unsafe {
            let handle = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }

            let result = SetPriorityClass(handle, NORMAL_PRIORITY_CLASS);
            let set_error = if result == 0 {
                Some(io::Error::last_os_error())
            } else {
                None
            };
            let _ = CloseHandle(handle);

            if let Some(error) = set_error {
                return Err(error);
            }
        }

        Ok(PriorityPromotion::Promoted)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{LaunchPriorityMode, PriorityPromotion};
    use std::io;
    use tokio::process::Command;

    /// Unix demotion via nice is intentionally disabled until Croopor has a
    /// reliable unprivileged restore path; leaving a game deprioritized after
    /// boot would be worse than skipping the startup priority sandwich.
    pub(super) fn configure_start_priority(
        _command: &mut Command,
    ) -> io::Result<LaunchPriorityMode> {
        Ok(LaunchPriorityMode::Noop)
    }

    pub(super) fn promote_after_boot(_pid: Option<u32>) -> io::Result<PriorityPromotion> {
        Ok(PriorityPromotion::Noop)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn non_windows_priority_setup_is_explicit_noop() {
        let mut command = Command::new("unused-test-command");

        assert_eq!(
            configure_start_priority(&mut command).expect("priority setup"),
            LaunchPriorityMode::Noop
        );
        assert_eq!(
            promote_after_boot(Some(1234)).expect("priority promotion"),
            PriorityPromotion::Noop
        );
        assert_eq!(
            promote_after_boot(None).expect("missing pid promotion"),
            PriorityPromotion::Noop
        );
    }
}
