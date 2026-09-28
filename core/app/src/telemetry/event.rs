//! Closed telemetry vocabulary. Raw diagnostics never become event properties.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) struct QueuedEvent {
    event: TelemetryEvent,
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl QueuedEvent {
    #[cfg(test)]
    pub(super) fn event_for_test(&self) -> TelemetryEvent {
        self.event
    }

    pub(super) fn new(event: TelemetryEvent) -> Self {
        Self {
            event,
            timestamp: chrono::Utc::now(),
        }
    }

    pub(super) fn batch_item(self, identity: &str, environment: &str) -> Value {
        let mut item = self.event.batch_item(identity, environment);
        item["timestamp"] = json!(
            self.timestamp
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
        item
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryLoader {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl TelemetryLoader {
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "vanilla" => Some(Self::Vanilla),
            "fabric" => Some(Self::Fabric),
            "quilt" => Some(Self::Quilt),
            "forge" => Some(Self::Forge),
            "neoforge" => Some(Self::NeoForge),
            _ => None,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryLaunchOutcome {
    Success,
    Failure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TelemetryErrorKind {
    LaunchSpawnFailed,
    LaunchStartupFailed,
    InstallFailed,
    ConfigSaveFailed,
    StartupFailed,
    Panic,
    FrontendError,
}

impl TelemetryErrorKind {
    fn details(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::LaunchSpawnFailed => (
                "launch_spawn_failed",
                "launch",
                "Game process could not start.",
            ),
            Self::LaunchStartupFailed => {
                ("launch_startup_failed", "launch", "Game startup failed.")
            }
            Self::InstallFailed => ("install_failed", "install", "Installation failed."),
            Self::ConfigSaveFailed => (
                "config_save_failed",
                "config",
                "Settings could not be saved.",
            ),
            Self::StartupFailed => ("startup_failed", "startup", "Application startup failed."),
            Self::Panic => ("panic", "panic", "Process panicked."),
            Self::FrontendError => ("frontend_error", "frontend", "Frontend error occurred."),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryEvent {
    AppStarted { state_inspector: bool },
    LaunchStarted { loader: Option<TelemetryLoader> },
    LaunchCompleted { outcome: TelemetryLaunchOutcome },
    InstanceCreated { loader: Option<TelemetryLoader> },
    ErrorCaptured { kind: TelemetryErrorKind },
}

impl TelemetryEvent {
    pub(super) fn error_kind(self) -> Option<TelemetryErrorKind> {
        match self {
            Self::ErrorCaptured { kind } => Some(kind),
            _ => None,
        }
    }

    pub(super) fn batch_item(self, identity: &str, environment: &str) -> Value {
        let mut properties = json!({
            "distinct_id": identity,
            "$process_person_profile": false,
            "environment": environment,
        });
        let event = match self {
            Self::AppStarted { state_inspector } => {
                properties["app_version"] = json!(env!("CARGO_PKG_VERSION"));
                properties["os"] = json!(std::env::consts::OS);
                properties["arch"] = json!(std::env::consts::ARCH);
                properties["active_flags"] = if state_inspector && cfg!(debug_assertions) {
                    json!(["dev.state-inspector"])
                } else {
                    json!([])
                };
                "app_started"
            }
            Self::LaunchStarted { loader } | Self::InstanceCreated { loader } => {
                if let Some(loader) = loader {
                    properties["loader_key"] = json!(loader.key());
                }
                if matches!(self, Self::LaunchStarted { .. }) {
                    "launch_started"
                } else {
                    "instance_created"
                }
            }
            Self::LaunchCompleted { outcome } => {
                properties["outcome"] = json!(match outcome {
                    TelemetryLaunchOutcome::Success => "success",
                    TelemetryLaunchOutcome::Failure => "failure",
                });
                "launch_completed"
            }
            Self::ErrorCaptured { kind } => {
                let (fingerprint, area, summary) = kind.details();
                properties["area"] = json!(area);
                properties["$exception_list"] = json!([{ "type": fingerprint, "value": summary }]);
                properties["$exception_fingerprint"] = json!(fingerprint);
                properties["$exception_level"] = json!(if kind == TelemetryErrorKind::Panic {
                    "fatal"
                } else {
                    "error"
                });
                "$exception"
            }
        };
        json!({ "event": event, "properties": properties })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontendErrorKind {
    Error,
    Unhandledrejection,
    Render,
}

/// Retains the existing frontend request shape. The source text is discarded;
/// it is neither logged nor copied into public errors or telemetry.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendErrorReportRequest {
    pub kind: FrontendErrorKind,
    pub name: String,
    pub message: String,
}

impl FrontendErrorReportRequest {
    pub fn is_bounded(&self) -> bool {
        self.name.len() <= 64 && self.message.chars().take(201).count() <= 200
    }
}
