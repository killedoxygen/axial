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

    #[test]
    fn priority_error_text_is_bounded_and_path_scrubbed() {
        let error = io::Error::new(
            io::ErrorKind::Other,
            format!(
                " /tmp\\secret\n{}",
                "x".repeat(MAX_PRIORITY_ERROR_CHARS + 40)
            ),
        );

        let sanitized = sanitize_priority_error(&error).expect("sanitized error");

        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains('\\'));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.chars().count() <= MAX_PRIORITY_ERROR_CHARS);
    }
}
