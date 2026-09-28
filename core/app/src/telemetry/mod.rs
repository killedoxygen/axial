//! Consent-fenced, bounded telemetry. Composition owns the flush task lifetime.
//!
//! Settings holds `consent_change()` while persisting a consent/identity change,
//! then publishes the committed values. The admission fence waits for an already
//! admitted, time-bounded send before persistence can make revocation visible.

mod event;
mod sink;

pub use event::{
    FrontendErrorKind, FrontendErrorReportRequest, TelemetryErrorKind, TelemetryEvent,
    TelemetryLaunchOutcome, TelemetryLoader,
};
pub use sink::{CollectorConfig, CollectorConfigurationError, TelemetryEnvironment};

use event::QueuedEvent;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, OwnedRwLockWriteGuard, RwLock, RwLockWriteGuard, watch};

pub const QUEUE_CAPACITY: usize = 64;
pub const BATCH_CAPACITY: usize = 20;
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const ERROR_CAPACITY: usize = 30;
const ERROR_KIND_CAPACITY: usize = 5;

pub struct Telemetry {
    collector: Option<CollectorConfig>,
    admission: Arc<RwLock<()>>,
    state: Mutex<State>,
    urgent: Notify,
}

#[derive(Default)]
struct State {
    identity: Option<String>,
    queue: VecDeque<QueuedEvent>,
    errors: usize,
    errors_by_kind: HashMap<TelemetryErrorKind, usize>,
}

impl Telemetry {
    /// Starts with consent disabled. No environment or user profile is read.
    pub fn new(collector: Option<CollectorConfig>) -> Self {
        Self {
            collector,
            admission: Arc::new(RwLock::new(())),
            state: Mutex::new(State::default()),
            urgent: Notify::new(),
        }
    }

    pub fn export_configured(&self) -> bool {
        self.collector.is_some()
    }

    /// Domain producer fixtures inspect real admission without starting export.
    #[cfg(test)]
    pub(crate) fn configured_for_test() -> Arc<Self> {
        Arc::new(Self::new(Some(
            CollectorConfig::new(
                "phc_domain_fixture",
                "http://127.0.0.1:9",
                TelemetryEnvironment::Test,
            )
            .expect("fixed local collector configuration"),
        )))
    }

    #[cfg(test)]
    pub(crate) fn queued_events_for_test(&self) -> Vec<TelemetryEvent> {
        self.state
            .lock()
            .expect("test telemetry state is not poisoned")
            .queue
            .iter()
            .map(QueuedEvent::event_for_test)
            .collect()
    }

    pub async fn consent_change(&self) -> ConsentChange<'_> {
        ConsentChange {
            telemetry: self,
            _admission: self.admission.write().await,
        }
    }

    /// Move this guard into the accepted settings write so HTTP cancellation
    /// cannot release export admission before its database commit settles.
    pub async fn consent_change_owned(self: &Arc<Self>) -> OwnedConsentChange {
        OwnedConsentChange {
            telemetry: self.clone(),
            _admission: self.admission.clone().write_owned().await,
        }
    }

    /// Admission is fail-closed when the state is poisoned or consent is absent.
    pub fn emit(&self, event: TelemetryEvent) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        self.enqueue(&mut state, event)
    }

    pub fn report_frontend_error(&self, request: FrontendErrorReportRequest) -> bool {
        request.is_bounded()
            && self.emit(TelemetryEvent::ErrorCaptured {
                kind: TelemetryErrorKind::FrontendError,
            })
    }

    fn enqueue(&self, state: &mut State, event: TelemetryEvent) -> bool {
        if self.collector.is_none() || state.identity.is_none() {
            return false;
        }
        if let Some(kind) = event.error_kind() {
            let count = state.errors_by_kind.entry(kind).or_default();
            if state.errors >= ERROR_CAPACITY || *count >= ERROR_KIND_CAPACITY {
                return false;
            }
            *count += 1;
            state.errors += 1;
        }
        if state.queue.len() == QUEUE_CAPACITY {
            state.queue.pop_front();
        }
        state.queue.push_back(QueuedEvent::new(event));
        true
    }

    /// Does not acquire a blocking lock or inspect a panic payload/location.
    pub fn capture_panic(&self) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if self.enqueue(
            &mut state,
            TelemetryEvent::ErrorCaptured {
                kind: TelemetryErrorKind::Panic,
            },
        ) {
            self.urgent.notify_one();
        }
    }

    pub async fn flush_once(&self) -> usize {
        let Some(collector) = &self.collector else {
            return 0;
        };
        // This lease deliberately survives network I/O. Consent changes wait for
        // the bounded request before publishing their persisted state.
        let _admission = self.admission.read().await;
        let Some((identity, events)) = self.drain_batch() else {
            return 0;
        };
        let count = events.len();
        if collector.send(&identity, events).await {
            count
        } else {
            0
        }
    }

    fn drain_batch(&self) -> Option<(String, Vec<QueuedEvent>)> {
        let mut state = self.state.lock().ok()?;
        let identity = state.identity.clone()?;
        let count = state.queue.len().min(BATCH_CAPACITY);
        if count == 0 {
            return None;
        }
        Some((identity, state.queue.drain(..count).collect()))
    }

    /// No detached tasks. The composition task owner must await this on shutdown.
    /// Shutdown attempts the bounded queue's remaining batches. Failed exports
    /// are discarded, and every request is time bounded.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(FLUSH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow_and_update() {
                self.flush_remaining().await;
                return;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() { self.flush_remaining().await; return; }
                }
                _ = self.urgent.notified() => { self.flush_once().await; }
                _ = interval.tick() => { self.flush_once().await; }
            }
        }
    }

    async fn flush_remaining(&self) {
        for _ in 0..QUEUE_CAPACITY.div_ceil(BATCH_CAPACITY) {
            // A failed request still drains its batch. Continue to the next
            // batch, while bounding shutdown even if producers are still active.
            if self
                .state
                .lock()
                .map_or(true, |state| state.queue.is_empty())
            {
                return;
            }
            self.flush_once().await;
        }
    }

    fn state_for_update(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish_consent(&self, enabled: bool, identity: Option<&str>) {
        let identity = identity.filter(|_| enabled).and_then(canonical_identity);
        let mut state = self.state_for_update();
        if state.identity != identity || !enabled {
            state.queue.clear();
        }
        state.identity = identity;
    }
}

pub struct ConsentChange<'a> {
    telemetry: &'a Telemetry,
    _admission: RwLockWriteGuard<'a, ()>,
}

impl ConsentChange<'_> {
    /// Call only after the settings commit succeeds, before releasing this guard.
    /// The identity belongs to the replacement profile; this never creates one.
    pub fn publish(&self, enabled: bool, identity: Option<&str>) {
        self.telemetry.publish_consent(enabled, identity);
    }
}

pub struct OwnedConsentChange {
    telemetry: Arc<Telemetry>,
    _admission: OwnedRwLockWriteGuard<()>,
}

impl OwnedConsentChange {
    pub fn publish(&self, enabled: bool, identity: Option<&str>) {
        self.telemetry.publish_consent(enabled, identity);
    }
}

fn canonical_identity(identity: &str) -> Option<String> {
    // Canonical shape prevents paths, credentials, usernames and alternate UUID
    // representations from becoming an analytics distinct ID.
    if identity.len() != 36 {
        return None;
    }
    let canonical = uuid::Uuid::parse_str(identity).ok()?.to_string();
    canonical
        .eq_ignore_ascii_case(identity)
        .then_some(canonical)
}

static PANIC_TARGET: OnceLock<Mutex<Weak<Telemetry>>> = OnceLock::new();
static PANIC_INSTALLED: AtomicBool = AtomicBool::new(false);

/// The process-global hook holds a weak reference and always chains its predecessor.
/// No panic source data enters the exporter, including on error paths.
pub fn install_panic_capture(telemetry: &Arc<Telemetry>) {
    let slot = PANIC_TARGET.get_or_init(|| Mutex::new(Weak::new()));
    if let Ok(mut target) = slot.lock() {
        *target = Arc::downgrade(telemetry);
    }
    if PANIC_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(telemetry) = PANIC_TARGET
            .get()
            .and_then(|slot| slot.try_lock().ok().and_then(|target| target.upgrade()))
        {
            telemetry.capture_panic();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests;
