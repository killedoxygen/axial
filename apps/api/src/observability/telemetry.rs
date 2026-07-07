use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_public_json_value};
use croopor_config::{AppConfig, ConfigStore, FEATURE_FLAGS};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc as std_mpsc,
};
use std::thread;
use std::time::Duration;
use url::Url;

pub const POSTHOG_API_KEY_ENV: &str = "CROOPOR_POSTHOG_API_KEY";
pub const POSTHOG_HOST_ENV: &str = "CROOPOR_POSTHOG_HOST";
pub const POSTHOG_ENVIRONMENT_ENV: &str = "CROOPOR_POSTHOG_ENVIRONMENT";
pub const DEFAULT_POSTHOG_HOST: &str = "https://eu.i.posthog.com";
pub const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

const TELEMETRY_QUEUE_CAP: usize = 64;
const TELEMETRY_BATCH_CAP: usize = 20;
const TELEMETRY_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const TELEMETRY_SYNC_HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const TELEMETRY_SYNC_JOIN_TIMEOUT: Duration = Duration::from_millis(3_500);
const TELEMETRY_USER_AGENT: &str = concat!("croopor/", env!("CARGO_PKG_VERSION"), " telemetry");
const MAX_PROPERTY_TEXT_CHARS: usize = 128;
const MAX_PROPERTY_TOKEN_CHARS: usize = 64;
pub(crate) const MAX_EXCEPTION_SUMMARY_CHARS: usize = 200;
const MAX_HOST_CHARS: usize = 2048;
const MAX_POSTHOG_ENVIRONMENT_CHARS: usize = 32;
const MIN_POSTHOG_KEY_CHARS: usize = 8;
const MAX_POSTHOG_KEY_CHARS: usize = 128;
const MAX_LOGGED_FAILURE_COUNT: u64 = 9_999;
const MAX_ERROR_EVENTS_PER_PROCESS: usize = 30;
const MAX_ERROR_EVENTS_PER_FINGERPRINT: usize = 5;

const EVENT_APP_STARTED: &str = "app_started";
const EVENT_LAUNCH_STARTED: &str = "launch_started";
const EVENT_LAUNCH_COMPLETED: &str = "launch_completed";
const EVENT_INSTANCE_CREATED: &str = "instance_created";
const EVENT_EXCEPTION: &str = "$exception";

const PROP_DISTINCT_ID: &str = "distinct_id";
const PROP_PROCESS_PERSON_PROFILE: &str = "$process_person_profile";
const PROP_EXCEPTION_LIST: &str = "$exception_list";
const PROP_EXCEPTION_FINGERPRINT: &str = "$exception_fingerprint";
const PROP_EXCEPTION_LEVEL: &str = "$exception_level";
const EXCEPTION_VALUE_REDACTED: &str = "[redacted]";

static PANIC_CAPTURE_HUB: OnceLock<Mutex<Option<Arc<TelemetryHub>>>> = OnceLock::new();
static PANIC_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

fn default_posthog_environment() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "production"
    }
}

pub fn configured_posthog_key() -> Option<String> {
    let raw = std::env::var(POSTHOG_API_KEY_ENV)
        .ok()
        .or_else(|| option_env!("CROOPOR_POSTHOG_API_KEY").map(str::to_string))?;
    sanitize_posthog_key(&raw).ok()
}

pub fn configured_posthog_host() -> String {
    let raw = std::env::var(POSTHOG_HOST_ENV)
        .ok()
        .or_else(|| option_env!("CROOPOR_POSTHOG_HOST").map(str::to_string));
    raw.as_deref()
        .and_then(sanitize_posthog_host)
        .unwrap_or_else(|| DEFAULT_POSTHOG_HOST.to_string())
}

pub fn configured_posthog_environment() -> String {
    let raw = std::env::var(POSTHOG_ENVIRONMENT_ENV)
        .ok()
        .or_else(|| option_env!("CROOPOR_POSTHOG_ENVIRONMENT").map(str::to_string));
    raw.as_deref()
        .and_then(sanitize_posthog_environment)
        .unwrap_or_else(|| default_posthog_environment().to_string())
}

#[derive(Clone, Debug)]
pub enum TelemetryEvent {
    AppStarted {
        app_version: String,
        active_flags: Vec<String>,
    },
    LaunchStarted {
        loader_key: Option<String>,
    },
    LaunchCompleted {
        outcome: TelemetryLaunchOutcome,
    },
    InstanceCreated {
        loader_key: Option<String>,
    },
    ErrorCaptured {
        kind: TelemetryErrorKind,
        area: TelemetryErrorArea,
        level: TelemetryErrorLevel,
        summary: String,
    },
}

impl TelemetryEvent {
    pub fn app_started(app_version: impl Into<String>, config: &AppConfig) -> Self {
        Self::AppStarted {
            app_version: app_version.into(),
            active_flags: active_flag_keys(config),
        }
    }

    pub fn launch_started(loader_key: Option<String>) -> Self {
        Self::LaunchStarted { loader_key }
    }

    pub fn launch_completed(outcome: TelemetryLaunchOutcome) -> Self {
        Self::LaunchCompleted { outcome }
    }

    pub fn instance_created(loader_key: Option<String>) -> Self {
        Self::InstanceCreated { loader_key }
    }

    pub fn error_captured(
        kind: TelemetryErrorKind,
        area: TelemetryErrorArea,
        level: TelemetryErrorLevel,
        summary: impl Into<String>,
    ) -> Self {
        Self::ErrorCaptured {
            kind,
            area,
            level,
            summary: summary.into(),
        }
    }

    fn event_name(&self) -> &'static str {
        match self {
            Self::AppStarted { .. } => EVENT_APP_STARTED,
            Self::LaunchStarted { .. } => EVENT_LAUNCH_STARTED,
            Self::LaunchCompleted { .. } => EVENT_LAUNCH_COMPLETED,
            Self::InstanceCreated { .. } => EVENT_INSTANCE_CREATED,
            Self::ErrorCaptured { .. } => EVENT_EXCEPTION,
        }
    }

    fn error_kind(&self) -> Option<TelemetryErrorKind> {
        match self {
            Self::ErrorCaptured { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    fn append_properties(&self, properties: &mut Map<String, Value>) {
        match self {
            Self::AppStarted {
                app_version,
                active_flags,
            } => {
                insert_property(
                    properties,
                    TelemetryProperty::AppVersion,
                    json!(app_version),
                );
                insert_property(
                    properties,
                    TelemetryProperty::Os,
                    json!(std::env::consts::OS),
                );
                insert_property(
                    properties,
                    TelemetryProperty::Arch,
                    json!(std::env::consts::ARCH),
                );
                insert_property(
                    properties,
                    TelemetryProperty::ActiveFlags,
                    json!(active_flags),
                );
            }
            Self::LaunchStarted { loader_key } | Self::InstanceCreated { loader_key } => {
                if let Some(loader_key) = loader_key {
                    insert_property(properties, TelemetryProperty::LoaderKey, json!(loader_key));
                }
            }
            Self::LaunchCompleted { outcome } => {
                insert_property(
                    properties,
                    TelemetryProperty::Outcome,
                    json!(outcome.as_str()),
                );
            }
            Self::ErrorCaptured {
                kind,
                area,
                level,
                summary,
            } => {
                let kind = kind.as_str();
                let summary = sanitize_exception_summary(summary);
                insert_property(
                    properties,
                    TelemetryProperty::ExceptionList,
                    json!([{ "type": kind, "value": summary }]),
                );
                insert_property(
                    properties,
                    TelemetryProperty::ExceptionFingerprint,
                    json!(kind),
                );
                insert_property(
                    properties,
                    TelemetryProperty::ExceptionLevel,
                    json!(level.as_str()),
                );
                insert_property(properties, TelemetryProperty::Area, json!(area.as_str()));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryLaunchOutcome {
    Success,
    Failure,
}

impl TelemetryLaunchOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TelemetryErrorArea {
    Launch,
    Install,
    Guardian,
    Config,
    Startup,
    Panic,
    Frontend,
}

impl TelemetryErrorArea {
    fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Install => "install",
            Self::Guardian => "guardian",
            Self::Config => "config",
            Self::Startup => "startup",
            Self::Panic => "panic",
            Self::Frontend => "frontend",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TelemetryErrorKind {
    LaunchSpawnFailed,
    LaunchStartupFailed,
    InstallFailed,
    GuardianRepairFailed,
    ConfigSaveFailed,
    StartupFailed,
    Panic,
    FrontendError,
}

impl TelemetryErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::LaunchSpawnFailed => "launch_spawn_failed",
            Self::LaunchStartupFailed => "launch_startup_failed",
            Self::InstallFailed => "install_failed",
            Self::GuardianRepairFailed => "guardian_repair_failed",
            Self::ConfigSaveFailed => "config_save_failed",
            Self::StartupFailed => "startup_failed",
            Self::Panic => "panic",
            Self::FrontendError => "frontend_error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryErrorLevel {
    Error,
    Fatal,
}

impl TelemetryErrorLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TelemetryProperty {
    AppVersion,
    Os,
    Arch,
    ActiveFlags,
    Environment,
    LoaderKey,
    Outcome,
    ExceptionList,
    ExceptionFingerprint,
    ExceptionLevel,
    Area,
}

impl TelemetryProperty {
    fn as_str(self) -> &'static str {
        match self {
            Self::AppVersion => "app_version",
            Self::Os => "os",
            Self::Arch => "arch",
            Self::ActiveFlags => "active_flags",
            Self::Environment => "environment",
            Self::LoaderKey => "loader_key",
            Self::Outcome => "outcome",
            Self::ExceptionList => PROP_EXCEPTION_LIST,
            Self::ExceptionFingerprint => PROP_EXCEPTION_FINGERPRINT,
            Self::ExceptionLevel => PROP_EXCEPTION_LEVEL,
            Self::Area => "area",
        }
    }
}

#[derive(Default)]
struct TelemetryErrorStormState {
    total: usize,
    by_fingerprint: HashMap<&'static str, usize>,
}

impl TelemetryErrorStormState {
    fn allow(&mut self, kind: TelemetryErrorKind) -> bool {
        if self.total >= MAX_ERROR_EVENTS_PER_PROCESS {
            return false;
        }

        let fingerprint = kind.as_str();
        let count = self.by_fingerprint.entry(fingerprint).or_default();
        if *count >= MAX_ERROR_EVENTS_PER_FINGERPRINT {
            return false;
        }

        *count += 1;
        self.total += 1;
        true
    }
}

#[derive(Clone, Debug)]
struct QueuedTelemetryEvent {
    event: &'static str,
    properties: Map<String, Value>,
    timestamp: String,
}

impl QueuedTelemetryEvent {
    fn from_event(event: TelemetryEvent, distinct_id: &str) -> Option<Self> {
        let timestamp = sanitize_evidence_text(
            &chrono::Utc::now().to_rfc3339(),
            RedactionAudience::TelemetryExport,
            MAX_PROPERTY_TEXT_CHARS,
        )?;
        let mut properties = Map::new();
        properties.insert(
            PROP_DISTINCT_ID.to_string(),
            sanitize_distinct_id_property_value(distinct_id)?,
        );
        properties.insert(
            PROP_PROCESS_PERSON_PROFILE.to_string(),
            sanitize_property_value(json!(false))?,
        );
        insert_property(
            &mut properties,
            TelemetryProperty::Environment,
            json!(configured_posthog_environment()),
        );
        event.append_properties(&mut properties);

        Some(Self {
            event: event.event_name(),
            properties,
            timestamp,
        })
    }

    fn to_batch_item(&self) -> Value {
        json!({
            "event": self.event,
            "properties": self.properties,
            "timestamp": self.timestamp,
        })
    }
}

pub struct TelemetryHub {
    config: Arc<ConfigStore>,
    key: Option<String>,
    host: String,
    queue: Mutex<VecDeque<QueuedTelemetryEvent>>,
    error_storm: Mutex<TelemetryErrorStormState>,
    failed_batches: AtomicU64,
}

impl TelemetryHub {
    pub fn from_env(config: Arc<ConfigStore>) -> Self {
        Self::new(config, configured_posthog_key(), configured_posthog_host())
    }

    pub fn new(config: Arc<ConfigStore>, key: Option<String>, host: String) -> Self {
        Self {
            config,
            key: key.and_then(|value| sanitize_posthog_key(&value).ok()),
            host: sanitize_posthog_host(&host).unwrap_or_else(|| DEFAULT_POSTHOG_HOST.to_string()),
            queue: Mutex::new(VecDeque::new()),
            error_storm: Mutex::new(TelemetryErrorStormState::default()),
            failed_batches: AtomicU64::new(0),
        }
    }

    pub fn emit(&self, event: TelemetryEvent) {
        if self.key.is_none() {
            return;
        }

        let config = self.config.current();
        if !config.telemetry_enabled {
            return;
        }

        let Some(distinct_id) = self.telemetry_install_id(config) else {
            self.record_local_drop(1);
            return;
        };
        if !self.allow_event_for_export(&event) {
            return;
        }
        let Some(queued) = QueuedTelemetryEvent::from_event(event, &distinct_id) else {
            self.record_local_drop(1);
            return;
        };

        let mut queue = self.queue_guard();
        while queue.len() >= TELEMETRY_QUEUE_CAP {
            queue.pop_front();
        }
        queue.push_back(queued);
    }

    pub fn emit_sync_best_effort(&self, event: TelemetryEvent) -> bool {
        if self.key.is_none() {
            return false;
        }

        let config = self.config.current();
        if !config.telemetry_enabled {
            return false;
        }

        let Some(distinct_id) = self.telemetry_install_id(config) else {
            return false;
        };
        if !self.allow_event_for_export(&event) {
            return false;
        }
        let Some(queued) = QueuedTelemetryEvent::from_event(event, &distinct_id) else {
            return false;
        };

        self.send_single_event_sync_best_effort(queued)
    }

    pub fn export_configured(&self) -> bool {
        self.key.is_some()
    }

    pub fn configured_posthog_key(&self) -> Option<String> {
        self.key.clone()
    }

    pub fn configured_posthog_host(&self) -> String {
        self.host.clone()
    }

    pub fn current_telemetry_install_id(&self) -> Option<String> {
        let config = self.config.current();
        if !config.telemetry_enabled {
            return None;
        }

        self.canonicalize_existing_telemetry_install_id(config)
    }

    pub fn clear_queue(&self) {
        self.queue_guard().clear();
    }

    pub async fn flush_once(&self) -> usize {
        if !self.can_send_now() {
            self.clear_queue();
            return 0;
        }

        let batch = self.drain_batch(TELEMETRY_BATCH_CAP);
        if batch.is_empty() {
            return 0;
        }
        if !self.can_send_now() {
            return 0;
        }

        let Some(key) = self.key.as_deref() else {
            return 0;
        };
        let event_count = batch.len();
        let body = json!({
            "api_key": key,
            "batch": batch
                .iter()
                .map(QueuedTelemetryEvent::to_batch_item)
                .collect::<Vec<_>>(),
        });
        let url = format!("{}/batch/", self.host);

        match telemetry_client().post(url).json(&body).send().await {
            Ok(response) if response.status().is_success() => event_count,
            Ok(_) | Err(_) => {
                self.record_failed_batch(event_count);
                0
            }
        }
    }

    fn telemetry_install_id(&self, mut config: AppConfig) -> Option<String> {
        if let Some(install_id) = self.canonicalize_existing_telemetry_install_id(config.clone()) {
            return Some(install_id);
        }

        let install_id = uuid::Uuid::new_v4().to_string();
        config.telemetry_install_id = install_id.clone();
        match self.config.update(config) {
            Ok(_) => Some(install_id),
            Err(_) => None,
        }
    }

    fn canonicalize_existing_telemetry_install_id(&self, mut config: AppConfig) -> Option<String> {
        let raw = config.telemetry_install_id.trim();
        if raw.is_empty() {
            return None;
        }

        let install_id = sanitize_distinct_id(raw)?;
        if install_id != raw {
            config.telemetry_install_id = install_id.clone();
            let _ = self.config.update(config);
        }

        Some(install_id)
    }

    fn can_send_now(&self) -> bool {
        self.key.is_some() && self.config.current().telemetry_enabled
    }

    fn drain_batch(&self, max: usize) -> Vec<QueuedTelemetryEvent> {
        let mut queue = self.queue_guard();
        let count = queue.len().min(max);
        queue.drain(..count).collect()
    }

    fn allow_event_for_export(&self, event: &TelemetryEvent) -> bool {
        let Some(kind) = event.error_kind() else {
            return true;
        };
        self.error_storm_guard().allow(kind)
    }

    fn queue_guard(&self) -> MutexGuard<'_, VecDeque<QueuedTelemetryEvent>> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn error_storm_guard(&self) -> MutexGuard<'_, TelemetryErrorStormState> {
        self.error_storm
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn send_single_event_sync_best_effort(&self, event: QueuedTelemetryEvent) -> bool {
        let Some(key) = self.key.clone() else {
            return false;
        };
        let host = self.host.clone();
        let (tx, rx) = std_mpsc::channel();
        let handle = thread::spawn(move || {
            let sent = std::panic::catch_unwind(AssertUnwindSafe(|| {
                send_blocking_batch(key, host, event)
            }))
            .unwrap_or(false);
            let _ = tx.send(sent);
        });

        match rx.recv_timeout(TELEMETRY_SYNC_JOIN_TIMEOUT) {
            Ok(sent) => {
                let _ = handle.join();
                sent
            }
            Err(_) => false,
        }
    }

    fn record_failed_batch(&self, event_count: usize) {
        let failures = self
            .failed_batches
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            .min(MAX_LOGGED_FAILURE_COUNT);
        tracing::warn!(
            event_count = event_count.min(TELEMETRY_BATCH_CAP),
            failed_batches = failures,
            "telemetry batch export failed"
        );
    }

    fn record_local_drop(&self, event_count: usize) {
        tracing::warn!(
            event_count = event_count.min(TELEMETRY_BATCH_CAP),
            "telemetry event dropped before queueing"
        );
    }

    #[cfg(test)]
    pub(crate) fn queue_len_for_test(&self) -> usize {
        self.queue_guard().len()
    }

    #[cfg(test)]
    pub(crate) fn queued_batch_for_test(&self) -> Vec<Value> {
        self.queue_guard()
            .iter()
            .map(QueuedTelemetryEvent::to_batch_item)
            .collect()
    }
}

pub async fn run_telemetry_flush_loop(hub: Arc<TelemetryHub>) {
    loop {
        tokio::time::sleep(TELEMETRY_FLUSH_INTERVAL).await;
        hub.flush_once().await;
    }
}

pub fn install_panic_capture(hub: Arc<TelemetryHub>) {
    let hub_slot = PANIC_CAPTURE_HUB.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = hub_slot.lock() {
        *guard = Some(hub);
    }

    if PANIC_HOOK_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if PANIC_HOOK_ACTIVE.swap(true, Ordering::AcqRel) {
            return;
        }
        let _guard = PanicHookGuard;

        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if let Some(hub) = PANIC_CAPTURE_HUB
                .get()
                .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
            {
                hub.emit_sync_best_effort(TelemetryEvent::error_captured(
                    TelemetryErrorKind::Panic,
                    TelemetryErrorArea::Panic,
                    TelemetryErrorLevel::Fatal,
                    panic_summary(info),
                ));
            }
        }));

        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| previous_hook(info)));
    }));
}

struct PanicHookGuard;

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        PANIC_HOOK_ACTIVE.store(false, Ordering::Release);
    }
}

fn insert_property(properties: &mut Map<String, Value>, key: TelemetryProperty, value: Value) {
    if let Some(value) = sanitize_property_value(value) {
        properties.insert(key.as_str().to_string(), value);
    }
}

fn sanitize_exception_summary(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = truncate_chars(&normalized, MAX_EXCEPTION_SUMMARY_CHARS);
    sanitize_evidence_text(
        &bounded,
        RedactionAudience::TelemetryExport,
        MAX_EXCEPTION_SUMMARY_CHARS,
    )
    .unwrap_or_else(|| EXCEPTION_VALUE_REDACTED.to_string())
}

fn sanitize_property_value(value: Value) -> Option<Value> {
    sanitize_public_json_value(
        value,
        RedactionAudience::TelemetryExport,
        MAX_PROPERTY_TEXT_CHARS,
        MAX_PROPERTY_TOKEN_CHARS,
    )
}

fn sanitize_distinct_id_property_value(value: &str) -> Option<Value> {
    sanitize_distinct_id(value).map(Value::String)
}

fn sanitize_distinct_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() != 36 {
        return None;
    }

    let canonical = uuid::Uuid::parse_str(value).ok()?.to_string();
    if canonical.eq_ignore_ascii_case(value) {
        Some(canonical)
    } else {
        None
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn active_flag_keys(config: &AppConfig) -> Vec<String> {
    FEATURE_FLAGS
        .iter()
        .filter(|flag| !flag.dev_only || cfg!(debug_assertions))
        .filter(|flag| {
            config
                .feature_overrides
                .get(flag.key)
                .copied()
                .unwrap_or(flag.default_enabled)
                != flag.default_enabled
        })
        .map(|flag| flag.key.to_string())
        .collect()
}

fn sanitize_posthog_key(raw: &str) -> Result<String, &'static str> {
    let value = raw.trim();
    if value.len() < MIN_POSTHOG_KEY_CHARS || value.len() > MAX_POSTHOG_KEY_CHARS {
        return Err("invalid length");
    }
    if !value.starts_with("phc_") {
        return Err("invalid prefix");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("invalid characters");
    }
    Ok(value.to_string())
}

fn sanitize_posthog_host(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > MAX_HOST_CHARS {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }

    Some(url.as_str().trim_end_matches('/').to_string())
}

fn sanitize_posthog_environment(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || value.len() > MAX_POSTHOG_ENVIRONMENT_CHARS {
        return None;
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(value)
}

fn telemetry_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(TELEMETRY_USER_AGENT)
                .timeout(TELEMETRY_HTTP_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

fn send_blocking_batch(key: String, host: String, event: QueuedTelemetryEvent) -> bool {
    let body = json!({
        "api_key": key,
        "batch": [event.to_batch_item()],
    });
    let url = format!("{}/batch/", host);
    let Ok(client) = reqwest::blocking::Client::builder()
        .user_agent(TELEMETRY_USER_AGENT)
        .timeout(TELEMETRY_SYNC_HTTP_TIMEOUT)
        .build()
    else {
        return false;
    };

    client
        .post(url)
        .json(&body)
        .send()
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

fn panic_summary(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic payload unavailable");
    let Some(location) = info.location() else {
        return payload.to_string();
    };
    format!("{payload} at {}", panic_location_summary(location))
}

fn panic_location_summary(location: &std::panic::Location<'_>) -> String {
    let file = location
        .file()
        .chars()
        .map(|value| {
            if matches!(value, '/' | '\\') {
                ':'
            } else {
                value
            }
        })
        .collect::<String>();
    format!("{file}:{}", location.line())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::default_frontend_dir;
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axum::{Json, Router, extract::State, http::StatusCode, http::Uri, routing::post};
    use croopor_config::{AppPaths, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::fs;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    const TEST_KEY: &str = "phc_test";
    const TEST_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";

    struct TestConfig {
        root: PathBuf,
        paths: AppPaths,
        store: Arc<ConfigStore>,
    }

    impl TestConfig {
        fn new(name: &str, config: AppConfig) -> Self {
            let root = std::env::temp_dir().join(format!(
                "croopor-api-telemetry-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_nanos())
                    .unwrap_or_default()
            ));
            fs::create_dir_all(&root).expect("create telemetry test root");
            let paths = test_paths(&root);
            let store = ConfigStore::load_from(paths.clone()).expect("load config store");
            store.update(config).expect("seed config");

            Self {
                root,
                paths,
                store: Arc::new(store),
            }
        }
    }

    impl Drop for TestConfig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn posthog_key_sanitizer_accepts_public_project_keys_only() {
        assert_eq!(
            sanitize_posthog_key(" phc_abc_123 "),
            Ok("phc_abc_123".to_string())
        );
        assert!(sanitize_posthog_key("abc_123").is_err());
        assert!(sanitize_posthog_key("phc_bad-key").is_err());
        assert!(sanitize_posthog_key("phc_").is_err());
    }

    #[test]
    fn posthog_host_sanitizer_accepts_http_urls_and_strips_trailing_slash() {
        assert_eq!(
            sanitize_posthog_host(" https://eu.i.posthog.com/ "),
            Some("https://eu.i.posthog.com".to_string())
        );
        assert_eq!(
            sanitize_posthog_host("http://127.0.0.1:43123/custom/"),
            Some("http://127.0.0.1:43123/custom".to_string())
        );
        assert_eq!(sanitize_posthog_host("ftp://example.test"), None);
        assert_eq!(
            sanitize_posthog_host("https://example.test/path?token=x"),
            None
        );
    }

    #[test]
    fn posthog_environment_sanitizer_accepts_safe_lowercase_values_only() {
        assert_eq!(
            sanitize_posthog_environment(" Release_Candidate-1 "),
            Some("release_candidate-1".to_string())
        );
        assert_eq!(sanitize_posthog_environment(""), None);
        assert_eq!(sanitize_posthog_environment("prod.us"), None);
        assert_eq!(
            sanitize_posthog_environment("environment-name-that-is-too-long"),
            None
        );
    }

    #[test]
    fn invalid_posthog_environment_env_falls_back_to_build_default() {
        let output =
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .arg("--exact")
                .arg("observability::telemetry::tests::invalid_posthog_environment_env_probe")
                .arg("--ignored")
                .env(POSTHOG_ENVIRONMENT_ENV, "prod.us")
                .output()
                .expect("run env probe test");

        assert!(
            output.status.success(),
            "env probe failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore]
    fn invalid_posthog_environment_env_probe() {
        assert_eq!(
            configured_posthog_environment(),
            default_posthog_environment()
        );
    }

    #[test]
    fn no_key_disables_emit_without_queue_growth() {
        let fixture = TestConfig::new("no-key", enabled_config_with_install_id());
        let hub = TelemetryHub::new(
            fixture.store.clone(),
            None,
            DEFAULT_POSTHOG_HOST.to_string(),
        );

        hub.emit(TelemetryEvent::launch_completed(
            TelemetryLaunchOutcome::Success,
        ));

        assert_eq!(hub.queue_len_for_test(), 0);
    }

    #[test]
    fn consent_off_disables_emit_without_queue_growth() {
        let fixture = TestConfig::new(
            "consent-off",
            AppConfig {
                telemetry_enabled: false,
                telemetry_install_id: TEST_INSTALL_ID.to_string(),
                ..AppConfig::default()
            },
        );
        let hub = test_hub(fixture.store.clone());

        hub.emit(TelemetryEvent::launch_completed(
            TelemetryLaunchOutcome::Success,
        ));

        assert_eq!(hub.queue_len_for_test(), 0);
    }

    #[test]
    fn consent_on_queues_allowlisted_event_with_anonymous_posthog_properties() {
        let fixture = TestConfig::new("consent-on", enabled_config_with_install_id());
        let hub = test_hub(fixture.store.clone());

        hub.emit(TelemetryEvent::launch_completed(
            TelemetryLaunchOutcome::Success,
        ));

        let queued = hub.queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], EVENT_LAUNCH_COMPLETED);
        assert_eq!(queued[0]["properties"][PROP_DISTINCT_ID], TEST_INSTALL_ID);
        assert_eq!(queued[0]["properties"][PROP_PROCESS_PERSON_PROFILE], false);
        assert_eq!(
            queued[0]["properties"]["environment"],
            configured_posthog_environment()
        );
        assert_eq!(queued[0]["properties"]["outcome"], "success");
        assert!(queued[0]["properties"]["loader_key"].is_null());
    }

    #[test]
    fn exception_event_queues_posthog_error_tracking_shape() {
        let fixture = TestConfig::new("exception-shape", enabled_config_with_install_id());
        let hub = test_hub(fixture.store.clone());

        hub.emit(TelemetryEvent::error_captured(
            TelemetryErrorKind::LaunchSpawnFailed,
            TelemetryErrorArea::Launch,
            TelemetryErrorLevel::Error,
            "launch spawn failed",
        ));

        let queued = hub.queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], EVENT_EXCEPTION);
        assert_eq!(queued[0]["properties"][PROP_DISTINCT_ID], TEST_INSTALL_ID);
        assert_eq!(queued[0]["properties"][PROP_PROCESS_PERSON_PROFILE], false);
        assert_eq!(
            queued[0]["properties"]["environment"],
            configured_posthog_environment()
        );
        assert_eq!(
            queued[0]["properties"][PROP_EXCEPTION_FINGERPRINT],
            "launch_spawn_failed"
        );
        assert_eq!(queued[0]["properties"][PROP_EXCEPTION_LEVEL], "error");
        assert_eq!(queued[0]["properties"]["area"], "launch");
        let exception_list = queued[0]["properties"][PROP_EXCEPTION_LIST]
            .as_array()
            .expect("exception list should be an array");
        assert_eq!(exception_list.len(), 1);
        assert_eq!(exception_list[0]["type"], "launch_spawn_failed");
        assert_eq!(exception_list[0]["value"], "launch spawn failed");
        assert!(
            !exception_list[0]
                .as_object()
                .expect("exception should be an object")
                .contains_key("stacktrace")
        );
        assert!(queued[0]["timestamp"].as_str().is_some());
    }

    #[test]
    fn exception_summary_redaction_keeps_event_with_redacted_value() {
        let fixture = TestConfig::new("exception-redaction", enabled_config_with_install_id());
        let hub = test_hub(fixture.store.clone());

        hub.emit(TelemetryEvent::error_captured(
            TelemetryErrorKind::ConfigSaveFailed,
            TelemetryErrorArea::Config,
            TelemetryErrorLevel::Error,
            "failed writing /Users/alice/.croopor/config.json",
        ));

        let queued = hub.queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0]["properties"][PROP_EXCEPTION_LIST][0]["type"],
            "config_save_failed"
        );
        assert_eq!(
            queued[0]["properties"][PROP_EXCEPTION_LIST][0]["value"],
            EXCEPTION_VALUE_REDACTED
        );
    }

    #[test]
    fn exception_storm_caps_total_and_duplicate_fingerprints() {
        let duplicate_fixture =
            TestConfig::new("exception-duplicate-cap", enabled_config_with_install_id());
        let duplicate_hub = test_hub(duplicate_fixture.store.clone());

        for _ in 0..6 {
            duplicate_hub.emit(TelemetryEvent::error_captured(
                TelemetryErrorKind::InstallFailed,
                TelemetryErrorArea::Install,
                TelemetryErrorLevel::Error,
                "install failed",
            ));
        }

        assert_eq!(duplicate_hub.queue_len_for_test(), 5);

        let total_fixture =
            TestConfig::new("exception-total-cap", enabled_config_with_install_id());
        let total_hub = test_hub(total_fixture.store.clone());
        for kind in [
            TelemetryErrorKind::LaunchSpawnFailed,
            TelemetryErrorKind::LaunchStartupFailed,
            TelemetryErrorKind::InstallFailed,
            TelemetryErrorKind::GuardianRepairFailed,
            TelemetryErrorKind::ConfigSaveFailed,
            TelemetryErrorKind::StartupFailed,
        ] {
            for _ in 0..5 {
                total_hub.emit(TelemetryEvent::error_captured(
                    kind,
                    TelemetryErrorArea::Startup,
                    TelemetryErrorLevel::Error,
                    "bounded failure",
                ));
            }
        }
        total_hub.emit(TelemetryEvent::error_captured(
            TelemetryErrorKind::Panic,
            TelemetryErrorArea::Panic,
            TelemetryErrorLevel::Fatal,
            "panic after cap",
        ));

        assert_eq!(total_hub.queue_len_for_test(), 30);
    }

    #[test]
    fn panic_hook_without_key_does_not_recurse_or_deadlock() {
        let fixture = TestConfig::new("panic-no-key", enabled_config_with_install_id());
        let hub = Arc::new(TelemetryHub::new(
            fixture.store.clone(),
            None,
            DEFAULT_POSTHOG_HOST.to_string(),
        ));
        install_panic_capture(hub.clone());

        let result = std::thread::spawn(|| {
            std::panic::catch_unwind(|| {
                panic!("panic hook safety probe");
            })
        })
        .join()
        .expect("panic hook test thread should finish");

        assert!(result.is_err());
        assert_eq!(hub.queue_len_for_test(), 0);
    }

    #[test]
    fn queue_is_bounded_and_drops_oldest_events() {
        let fixture = TestConfig::new("bounded", enabled_config_with_install_id());
        let hub = test_hub(fixture.store.clone());

        for index in 0..70 {
            hub.emit(TelemetryEvent::launch_started(Some(format!(
                "loader{index}"
            ))));
        }

        let queued = hub.queued_batch_for_test();
        assert_eq!(queued.len(), 64);
        assert_eq!(queued[0]["properties"]["loader_key"], "loader6");
        assert_eq!(queued[63]["properties"]["loader_key"], "loader69");
    }

    #[test]
    fn sensitive_property_values_are_dropped_from_events() {
        let fixture = TestConfig::new("redaction", enabled_config_with_install_id());
        let hub = test_hub(fixture.store.clone());

        hub.emit(TelemetryEvent::launch_started(Some(
            "/Users/alice/.minecraft/token.txt".to_string(),
        )));

        let queued = hub.queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert!(queued[0]["properties"]["loader_key"].is_null());
        assert_eq!(queued[0]["properties"][PROP_DISTINCT_ID], TEST_INSTALL_ID);
    }

    #[test]
    fn distinct_id_sanitizer_allows_only_canonical_uuid_identity() {
        assert_eq!(
            sanitize_distinct_id_property_value("  123E4567-E89B-12D3-A456-426614174000  "),
            Some(json!(TEST_INSTALL_ID))
        );
        assert_eq!(
            sanitize_distinct_id_property_value("123e4567e89b12d3a456426614174000"),
            None
        );
        assert_eq!(
            sanitize_distinct_id_property_value(
                "/Users/alice/123e4567-e89b-12d3-a456-426614174000"
            ),
            None
        );
        assert_eq!(sanitize_distinct_id_property_value("not-a-uuid"), None);
    }

    #[test]
    fn queued_event_rejects_non_uuid_distinct_id() {
        assert!(
            QueuedTelemetryEvent::from_event(
                TelemetryEvent::launch_completed(TelemetryLaunchOutcome::Success),
                "not-a-uuid",
            )
            .is_none()
        );
    }

    #[test]
    fn current_telemetry_install_id_canonicalizes_and_repairs_uppercase_uuid() {
        let fixture = TestConfig::new(
            "canonical-install-id",
            AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: TEST_INSTALL_ID.to_ascii_uppercase(),
                ..AppConfig::default()
            },
        );
        let hub = test_hub(fixture.store.clone());

        assert_eq!(
            hub.current_telemetry_install_id().as_deref(),
            Some(TEST_INSTALL_ID)
        );
        assert_eq!(
            fixture.store.current().telemetry_install_id,
            TEST_INSTALL_ID
        );

        hub.emit(TelemetryEvent::launch_completed(
            TelemetryLaunchOutcome::Success,
        ));
        assert_eq!(
            hub.queued_batch_for_test()[0]["properties"][PROP_DISTINCT_ID],
            TEST_INSTALL_ID
        );
    }

    #[test]
    fn emit_assigns_and_persists_install_id_when_consent_is_on() {
        let fixture = TestConfig::new(
            "assign-install-id",
            AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: String::new(),
                ..AppConfig::default()
            },
        );
        let hub = test_hub(fixture.store.clone());

        hub.emit(TelemetryEvent::launch_completed(
            TelemetryLaunchOutcome::Success,
        ));

        let config = fixture.store.current();
        assert_eq!(hub.queue_len_for_test(), 1);
        assert_eq!(config.telemetry_install_id.len(), 36);
        assert_eq!(
            hub.queued_batch_for_test()[0]["properties"][PROP_DISTINCT_ID],
            config.telemetry_install_id
        );
    }

    #[test]
    fn consent_off_transition_clears_queue_and_persisted_install_id() {
        let fixture = TestConfig::new("consent-transition", enabled_config_with_install_id());
        let telemetry = Arc::new(test_hub(fixture.store.clone()));
        let state = test_state(&fixture, telemetry.clone());

        telemetry.emit(TelemetryEvent::launch_completed(
            TelemetryLaunchOutcome::Failure,
        ));
        assert_eq!(telemetry.queue_len_for_test(), 1);

        let mut next = state.config().current();
        next.telemetry_enabled = false;
        state.update_config(next).expect("disable telemetry");

        assert_eq!(telemetry.queue_len_for_test(), 0);
        assert!(state.config().current().telemetry_install_id.is_empty());
    }

    #[tokio::test]
    async fn flush_posts_posthog_batch_body_shape() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping socket telemetry flush test: bind denied");
                return;
            }
            Err(error) => panic!("bind telemetry test server: {error}"),
        };
        let addr = listener.local_addr().expect("test listener addr");
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
        let app = Router::new()
            .route("/batch/", post(capture_batch))
            .with_state(tx);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let fixture = TestConfig::new("flush-body", enabled_config_with_install_id());
        let hub = TelemetryHub::new(
            fixture.store.clone(),
            Some(TEST_KEY.to_string()),
            format!("http://{addr}"),
        );
        hub.emit(TelemetryEvent::instance_created(Some("fabric".to_string())));

        assert_eq!(hub.flush_once().await, 1);

        let (path, body) = rx.recv().await.expect("captured batch");
        server.abort();

        assert_eq!(path, "/batch/");
        assert_eq!(body["api_key"], TEST_KEY);
        assert_eq!(body["batch"][0]["event"], EVENT_INSTANCE_CREATED);
        assert_eq!(body["batch"][0]["properties"]["loader_key"], "fabric");
        assert_eq!(
            body["batch"][0]["properties"][PROP_DISTINCT_ID],
            TEST_INSTALL_ID
        );
        assert_eq!(
            body["batch"][0]["properties"][PROP_PROCESS_PERSON_PROFILE],
            false
        );
        assert_eq!(
            body["batch"][0]["properties"]["environment"],
            configured_posthog_environment()
        );
        assert!(body["batch"][0]["timestamp"].as_str().is_some());
    }

    async fn capture_batch(
        State(tx): State<mpsc::UnboundedSender<(String, Value)>>,
        uri: Uri,
        Json(body): Json<Value>,
    ) -> StatusCode {
        let _ = tx.send((uri.path().to_string(), body));
        StatusCode::OK
    }

    fn test_hub(config: Arc<ConfigStore>) -> TelemetryHub {
        TelemetryHub::new(
            config,
            Some(TEST_KEY.to_string()),
            DEFAULT_POSTHOG_HOST.to_string(),
        )
    }

    fn enabled_config_with_install_id() -> AppConfig {
        AppConfig {
            telemetry_enabled: true,
            telemetry_install_id: TEST_INSTALL_ID.to_string(),
            ..AppConfig::default()
        }
    }

    fn test_state(fixture: &TestConfig, telemetry: Arc<TelemetryHub>) -> AppState {
        let instances =
            Arc::new(InstanceStore::load_from(fixture.paths.clone()).expect("load instances"));
        AppState::new_with_telemetry(
            AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config: fixture.store.clone(),
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::new_with_config_dir(&fixture.paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
                frontend_dir: default_frontend_dir(),
            },
            telemetry,
        )
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
