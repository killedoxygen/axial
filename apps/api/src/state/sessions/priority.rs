use crate::observability::{RedactionAudience, sanitize_public_diagnostic_text};
use std::io;
use tokio::process::{Child, Command};

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
    MissingHandle,
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
            Self::MissingHandle => "missing_process_handle",
            #[cfg(not(windows))]
            Self::Noop => "noop",
        }
    }
}

pub(super) fn configure_start_priority(command: &mut Command) -> io::Result<LaunchPriorityMode> {
    platform::configure_start_priority(command)
}

pub(super) fn promote_after_boot(child: Option<&Child>) -> io::Result<PriorityPromotion> {
    platform::promote_after_boot(child)
}

pub(super) fn sanitize_priority_error(error: &io::Error) -> Option<String> {
    let sanitized = sanitize_public_diagnostic_text(
        &error.to_string(),
        RedactionAudience::UserVisible,
        MAX_PRIORITY_ERROR_CHARS,
        "",
    );
    let sanitized = sanitized.trim();
    (!sanitized.is_empty()).then(|| sanitized.to_string())
}

#[cfg(windows)]
mod platform {
    use super::{LaunchPriorityMode, PriorityPromotion};
    use std::io;
    use tokio::process::{Child, Command};
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, SetPriorityClass,
    };

    pub(super) fn configure_start_priority(
        command: &mut Command,
    ) -> io::Result<LaunchPriorityMode> {
        command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        Ok(LaunchPriorityMode::BelowNormalUntilBoot)
    }

    pub(super) fn promote_after_boot(child: Option<&Child>) -> io::Result<PriorityPromotion> {
        let Some(handle) = child.and_then(Child::raw_handle) else {
            return Ok(PriorityPromotion::MissingHandle);
        };

        // SAFETY: raw_handle is borrowed from the locked Tokio Child and remains valid for this
        // call. The launcher does not own or close it. A child process handle supports setting
        // its priority class without reopening by PID.
        let result = unsafe { SetPriorityClass(handle, NORMAL_PRIORITY_CLASS) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(PriorityPromotion::Promoted)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{LaunchPriorityMode, PriorityPromotion};
    use std::io;
    use tokio::process::{Child, Command};

    /// Unix demotion via nice is intentionally disabled until Axial has a
    /// reliable unprivileged restore path; leaving a game deprioritized after
    /// boot would be worse than skipping the startup priority sandwich.
    pub(super) fn configure_start_priority(
        _command: &mut Command,
    ) -> io::Result<LaunchPriorityMode> {
        Ok(LaunchPriorityMode::Noop)
    }

    pub(super) fn promote_after_boot(_child: Option<&Child>) -> io::Result<PriorityPromotion> {
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
            promote_after_boot(None).expect("priority promotion"),
            PriorityPromotion::Noop
        );
    }

    #[test]
    fn priority_error_text_is_bounded_and_path_scrubbed() {
        let error = io::Error::other(" /tmp\\secret priority setup failed");

        assert_eq!(sanitize_priority_error(&error), None);

        let error = io::Error::other(format!(
            " priority setup failed {}",
            "x".repeat(MAX_PRIORITY_ERROR_CHARS + 40)
        ));
        let sanitized = sanitize_priority_error(&error).expect("safe sanitized error");

        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains('\\'));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.chars().count() <= MAX_PRIORITY_ERROR_CHARS);
    }
}
