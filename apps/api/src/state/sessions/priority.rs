use std::io;
use tokio::process::Command;

const MAX_PRIORITY_ERROR_CHARS: usize = 160;

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

impl LaunchPriorityMode {
    pub(super) fn proof_value(self) -> &'static str {
        match self {
            #[cfg(windows)]
            Self::BelowNormalUntilBoot => "below_normal_until_boot",
            #[cfg(not(windows))]
            Self::Noop => "noop",
        }
    }
}

impl PriorityPromotion {
    pub(super) fn proof_value(self) -> &'static str {
        match self {
            #[cfg(windows)]
            Self::Promoted => "promoted",
            #[cfg(windows)]
            Self::MissingPid => "missing_pid",
            #[cfg(not(windows))]
            Self::Noop => "noop",
        }
    }
}

pub(super) fn configure_start_priority(command: &mut Command) -> io::Result<LaunchPriorityMode> {
    platform::configure_start_priority(command)
}

pub(super) fn promote_after_boot(pid: Option<u32>) -> io::Result<PriorityPromotion> {
    platform::promote_after_boot(pid)
}

pub(super) fn sanitize_priority_error(error: &io::Error) -> Option<String> {
    let sanitized = error
        .to_string()
        .trim()
        .chars()
        .filter(|value| !value.is_control() && !matches!(value, '/' | '\\'))
        .take(MAX_PRIORITY_ERROR_CHARS)
        .collect::<String>();
    let sanitized = sanitized.trim();
    (!sanitized.is_empty()).then(|| sanitized.to_string())
}

#[cfg(windows)]
mod platform {
    use super::{LaunchPriorityMode, PriorityPromotion};
    use std::io;
    use tokio::process::Command;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_INFORMATION,
        SetPriorityClass,
    };

    struct ProcessHandle(HANDLE);

    impl ProcessHandle {
        fn open_for_priority(pid: u32) -> io::Result<Self> {
            // SAFETY: OpenProcess is called with a constant access mask, no inherited handle,
            // and a pid value supplied by the launched child process metadata. A non-null
            // return value is an owned handle that must be closed exactly once.
            let handle = unsafe { OpenProcess(PROCESS_SET_INFORMATION, 0, pid) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }

            Ok(Self(handle))
        }

        fn set_normal_priority(&self) -> io::Result<()> {
            // SAFETY: self.0 is a live process handle owned by this wrapper and opened with
            // PROCESS_SET_INFORMATION. NORMAL_PRIORITY_CLASS is a valid process priority class.
            let result = unsafe { SetPriorityClass(self.0, NORMAL_PRIORITY_CLASS) };
            if result == 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        }
    }

    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            // SAFETY: ProcessHandle is constructed only from a successful OpenProcess call
            // and owns that handle. Drop runs once, so CloseHandle is paired exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

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

        let handle = ProcessHandle::open_for_priority(pid)?;
        handle.set_normal_priority()?;

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

    #[test]
    fn priority_error_text_is_bounded_and_path_scrubbed() {
        let error = io::Error::other(format!(
            " /tmp\\secret\n{}",
            "x".repeat(MAX_PRIORITY_ERROR_CHARS + 40)
        ));

        let sanitized = sanitize_priority_error(&error).expect("sanitized error");

        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains('\\'));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.chars().count() <= MAX_PRIORITY_ERROR_CHARS);
    }
}
