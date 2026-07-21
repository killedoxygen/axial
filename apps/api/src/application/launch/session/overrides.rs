use crate::execution::ExecutionFact;
use crate::execution::jvm::{JvmArgsInspection, JvmArgsInspectionRequest, inspect_jvm_args};
use crate::execution::runtime::{
    JavaProbeRunner, RuntimeProbeFailure, RuntimeProbeInfo, RuntimeProbeRequest,
    inspect_java_override_value, java_override_is_undefined_sentinel, missing_java_override,
    probe_java_runtime_with_runner,
};
use crate::guardian::GuardianPreflightOverrideSignals;
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::{
    AppState, IntegrityForegroundLease, JavaProbeFailureClaim, JavaProbeFailureKey,
    JavaProbeFailureKind, JavaProbeFailureOwner,
};
use axial_config::{AppConfig, Instance};
use axial_launcher::LaunchGuardianContext;
use axial_minecraft::{
    JavaRuntimeLookupError, JavaRuntimeProbeReceipt, JavaRuntimeProbeResolution,
    JavaRuntimeProbeResolutionError, RuntimeOverride, RuntimeProbeSource, parse_runtime_override,
    resolve_java_runtime_probe, snapshot_java_runtime,
};

#[derive(Clone, Copy, Default)]
pub(super) enum PreflightJavaProbeSource {
    #[default]
    None,
    Fresh,
    Receipt,
    FailureCache,
}

impl PreflightJavaProbeSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fresh => "fresh",
            Self::Receipt => "receipt",
            Self::FailureCache => "failure_cache",
        }
    }
}

pub(super) struct ExplicitJavaOverrideInspection {
    pub(super) facts: Vec<ExecutionFact>,
    pub(super) receipt: Option<JavaRuntimeProbeReceipt>,
    pub(super) probe_count: u8,
    pub(super) probe_source: PreflightJavaProbeSource,
}

pub(super) async fn inspect_explicit_java_override(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    integrity_foreground: &IntegrityForegroundLease,
    instance: &Instance,
    config: &AppConfig,
    required_java_major: Option<u32>,
    prior_receipt: Option<JavaRuntimeProbeReceipt>,
) -> Option<ExplicitJavaOverrideInspection> {
    let (target_id, raw_value) = if !instance.java_path.trim().is_empty() {
        ("instance_java_override", instance.java_path.as_str())
    } else if !config.java_path_override.trim().is_empty() {
        ("global_java_override", config.java_path_override.as_str())
    } else {
        return None;
    };
    Some(
        inspect_java_override(
            state,
            producer,
            integrity_foreground,
            target_id,
            raw_value,
            required_java_major,
            prior_receipt,
        )
        .await,
    )
}

async fn inspect_java_override(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    integrity_foreground: &IntegrityForegroundLease,
    target_id: &str,
    raw_value: &str,
    required_java_major: Option<u32>,
    prior_receipt: Option<JavaRuntimeProbeReceipt>,
) -> ExplicitJavaOverrideInspection {
    let target = java_override_target(target_id);
    if java_override_is_undefined_sentinel(raw_value) {
        let inspection = inspect_java_override_value(None, target, raw_value);
        return ExplicitJavaOverrideInspection {
            facts: inspection.facts,
            receipt: None,
            probe_count: 0,
            probe_source: PreflightJavaProbeSource::None,
        };
    }
    let RuntimeOverride::ExecutablePath(path) = parse_runtime_override(raw_value.trim()) else {
        let inspection = inspect_java_override_value(None, target, raw_value);
        return ExplicitJavaOverrideInspection {
            facts: inspection.facts,
            receipt: None,
            probe_count: 0,
            probe_source: PreflightJavaProbeSource::None,
        };
    };
    let required_min_update = (required_java_major == Some(8)).then_some(312);
    let snapshot_path = path.clone();
    let snapshot = match spawn_integrity_blocking(integrity_foreground, move || {
        snapshot_java_runtime(&snapshot_path)
    })
    .await
    {
        Ok(Ok(snapshot)) => snapshot,
        _ => {
            return cached_failure_inspection(
                target,
                &path,
                required_java_major,
                JavaProbeFailureKind::SpawnFailed,
            )
            .with_fresh_probe(0);
        }
    };
    let cache_key =
        JavaProbeFailureKey::new(snapshot.clone(), required_java_major, required_min_update);

    let mut cache_owner = None;
    if prior_receipt.is_none() {
        match state.java_probe_failures().claim(cache_key).await {
            JavaProbeFailureClaim::Hit(kind) => {
                return cached_failure_inspection(target, &path, required_java_major, kind);
            }
            JavaProbeFailureClaim::Owner(owner) => cache_owner = Some(owner),
            JavaProbeFailureClaim::Uncached => {}
        }
    }
    let resolution = resolve_probe(
        producer,
        integrity_foreground,
        snapshot,
        prior_receipt,
        cache_owner,
    )
    .await;
    let resolution = match resolution {
        Ok(resolution) => resolution,
        Err(error) => {
            let kind = failure_kind_from_lookup_error(&error.error);
            return cached_failure_inspection(target, &path, required_java_major, kind)
                .with_fresh_probe(error.usage.spawn_count);
        }
    };

    let runner = FixedProbeRunner::Success(RuntimeProbeInfo::new(
        "runtime",
        resolution.major,
        resolution.update,
        "unknown",
    ));
    let report = probe_java_runtime_with_runner(
        runtime_probe_request(target, &path, required_java_major),
        &runner,
    );
    if resolution.major == 0 {
        return ExplicitJavaOverrideInspection {
            facts: report.err().map(|error| error.facts).unwrap_or_default(),
            receipt: None,
            probe_count: resolution.usage.spawn_count,
            probe_source: preflight_source(resolution.usage.source),
        };
    }
    let facts = match report {
        Ok(report) => report.facts,
        Err(error) => error.facts,
    };
    ExplicitJavaOverrideInspection {
        facts,
        receipt: Some(resolution.receipt),
        probe_count: resolution.usage.spawn_count,
        probe_source: preflight_source(resolution.usage.source),
    }
}

async fn resolve_probe(
    producer: &crate::state::ProducerLease,
    integrity_foreground: &IntegrityForegroundLease,
    snapshot: axial_minecraft::JavaRuntimeProbeSnapshot,
    prior_receipt: Option<JavaRuntimeProbeReceipt>,
    cache_owner: Option<Box<JavaProbeFailureOwner>>,
) -> Result<JavaRuntimeProbeResolution, JavaRuntimeProbeResolutionError> {
    resolve_probe_with(producer, integrity_foreground, cache_owner, move || {
        resolve_java_runtime_probe(snapshot, prior_receipt, None)
    })
    .await
}

async fn resolve_probe_with<Resolve>(
    producer: &crate::state::ProducerLease,
    integrity_foreground: &IntegrityForegroundLease,
    cache_owner: Option<Box<JavaProbeFailureOwner>>,
    resolve: Resolve,
) -> Result<JavaRuntimeProbeResolution, JavaRuntimeProbeResolutionError>
where
    Resolve: FnOnce() -> Result<JavaRuntimeProbeResolution, JavaRuntimeProbeResolutionError>
        + Send
        + 'static,
{
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let retained_foreground = integrity_foreground.retained();
    producer.spawn_child(async move {
        let result = spawn_integrity_blocking(&retained_foreground, resolve)
            .await
            .unwrap_or_else(|_| {
                Err(JavaRuntimeProbeResolutionError {
                    error: JavaRuntimeLookupError::Probe(
                        "java runtime probe task stopped unexpectedly".to_string(),
                    ),
                    usage: axial_minecraft::RuntimeProbeUsage::default(),
                })
            });
        if let Some(owner) = cache_owner {
            match &result {
                Ok(resolution) if resolution.major == 0 => {
                    owner.finish(JavaProbeFailureKind::OutputParseFailed);
                }
                Ok(_) => owner.dismiss(),
                Err(error) => owner.finish(failure_kind_from_lookup_error(&error.error)),
            }
        }
        let _ = result_tx.send(result);
        drop(retained_foreground);
    });
    result_rx.await.unwrap_or_else(|_| {
        Err(JavaRuntimeProbeResolutionError {
            error: JavaRuntimeLookupError::Probe(
                "java runtime probe owner stopped unexpectedly".to_string(),
            ),
            usage: axial_minecraft::RuntimeProbeUsage::default(),
        })
    })
}

async fn spawn_integrity_blocking<Work, Output>(
    integrity_foreground: &IntegrityForegroundLease,
    work: Work,
) -> Result<Output, tokio::task::JoinError>
where
    Work: FnOnce() -> Output + Send + 'static,
    Output: Send + 'static,
{
    let retained_foreground = integrity_foreground.retained();
    tokio::task::spawn_blocking(move || {
        let _retained_foreground = retained_foreground;
        work()
    })
    .await
}

impl ExplicitJavaOverrideInspection {
    fn with_fresh_probe(mut self, spawn_count: u8) -> Self {
        self.probe_count = spawn_count;
        self.probe_source = PreflightJavaProbeSource::Fresh;
        self
    }
}

fn cached_failure_inspection(
    target: TargetDescriptor,
    path: &std::path::Path,
    required_java_major: Option<u32>,
    kind: JavaProbeFailureKind,
) -> ExplicitJavaOverrideInspection {
    let runner = match kind {
        JavaProbeFailureKind::Missing => {
            let inspection = missing_java_override(None, target);
            return ExplicitJavaOverrideInspection {
                facts: inspection.facts,
                receipt: None,
                probe_count: 0,
                probe_source: PreflightJavaProbeSource::FailureCache,
            };
        }
        JavaProbeFailureKind::SpawnFailed => {
            FixedProbeRunner::Failure(RuntimeProbeFailure::SpawnFailed)
        }
        JavaProbeFailureKind::TimedOut => FixedProbeRunner::Failure(RuntimeProbeFailure::TimedOut),
        JavaProbeFailureKind::OutputParseFailed => {
            FixedProbeRunner::Success(RuntimeProbeInfo::new("runtime", 0, 0, "unknown"))
        }
    };
    let facts = probe_java_runtime_with_runner(
        runtime_probe_request(target, path, required_java_major),
        &runner,
    )
    .err()
    .map(|error| error.facts)
    .unwrap_or_default();
    ExplicitJavaOverrideInspection {
        facts,
        receipt: None,
        probe_count: 0,
        probe_source: PreflightJavaProbeSource::FailureCache,
    }
}

fn runtime_probe_request(
    target: TargetDescriptor,
    path: &std::path::Path,
    required_java_major: Option<u32>,
) -> RuntimeProbeRequest<'_> {
    let mut request = RuntimeProbeRequest::new(target, path);
    if let Some(required_java_major) = required_java_major.filter(|major| *major > 0) {
        request = request.with_required_major(required_java_major);
        if required_java_major == 8 {
            request = request.with_required_min_update(312);
        }
    }
    request
}

fn failure_kind_from_lookup_error(error: &JavaRuntimeLookupError) -> JavaProbeFailureKind {
    match error {
        JavaRuntimeLookupError::ProbeTimedOut => JavaProbeFailureKind::TimedOut,
        JavaRuntimeLookupError::NotFound { .. } => JavaProbeFailureKind::Missing,
        _ => JavaProbeFailureKind::SpawnFailed,
    }
}

fn preflight_source(source: RuntimeProbeSource) -> PreflightJavaProbeSource {
    match source {
        RuntimeProbeSource::Receipt => PreflightJavaProbeSource::Receipt,
        RuntimeProbeSource::Fresh | RuntimeProbeSource::FreshAfterReceiptMismatch => {
            PreflightJavaProbeSource::Fresh
        }
        RuntimeProbeSource::None => PreflightJavaProbeSource::None,
    }
}

enum FixedProbeRunner {
    Success(RuntimeProbeInfo),
    Failure(RuntimeProbeFailure),
}

impl JavaProbeRunner for FixedProbeRunner {
    fn probe(
        &self,
        _java_path: &std::path::Path,
        _id_hint: Option<&str>,
    ) -> Result<RuntimeProbeInfo, RuntimeProbeFailure> {
        match self {
            Self::Success(info) => Ok(info.clone()),
            Self::Failure(error) => Err(*error),
        }
    }
}

fn java_override_target(id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Config,
        id,
        OwnershipClass::UserOwned,
    )
}

pub(super) fn inspect_explicit_jvm_args(raw_args: &str) -> JvmArgsInspection {
    if raw_args.trim().is_empty() {
        return JvmArgsInspection {
            args: Vec::new(),
            facts: Vec::new(),
        };
    }
    inspect_jvm_args(JvmArgsInspectionRequest::new(
        explicit_jvm_args_target(),
        raw_args,
    ))
}

fn explicit_jvm_args_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Config,
        "explicit_jvm_args",
        OwnershipClass::UserOwned,
    )
}

pub(super) fn preflight_override_signals(
    guardian: &LaunchGuardianContext,
) -> GuardianPreflightOverrideSignals {
    GuardianPreflightOverrideSignals {
        explicit_java_override: guardian.has_java_override(),
        explicit_jvm_preset: guardian.has_named_preset(),
        explicit_jvm_args: guardian.has_raw_jvm_args(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn cancelled_snapshot_waiter_retains_foreground_until_blocking_work_finishes() {
        let (state, root) = test_state("snapshot-cancellation");
        let foreground = state
            .register_integrity_foreground()
            .expect("register snapshot foreground")
            .wait_for_settlement()
            .await;
        let task_foreground = foreground.retained();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(async move {
            spawn_integrity_blocking(&task_foreground, move || {
                let _ = started_tx.send(());
                release_rx.recv().expect("release snapshot worker");
            })
            .await
        });
        started_rx.await.expect("snapshot worker started");

        drop(foreground);
        waiter.abort();
        assert!(
            waiter
                .await
                .expect_err("cancel snapshot waiter")
                .is_cancelled()
        );
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "blocking snapshot must retain foreground after waiter cancellation"
        );

        release_tx.send(()).expect("finish snapshot worker");
        wait_for_integrity_idle(&state).await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_probe_waiter_retains_foreground_until_child_and_blocking_work_finish() {
        let (state, root) = test_state("probe-cancellation");
        let foreground = state
            .register_integrity_foreground()
            .expect("register probe foreground")
            .wait_for_settlement()
            .await;
        let task_foreground = foreground.retained();
        let producer = state.try_claim_producer().expect("claim probe producer");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(async move {
            resolve_probe_with(&producer, &task_foreground, None, move || {
                let _ = started_tx.send(());
                release_rx.recv().expect("release probe worker");
                Err(JavaRuntimeProbeResolutionError {
                    error: JavaRuntimeLookupError::Probe("injected probe failure".to_string()),
                    usage: axial_minecraft::RuntimeProbeUsage::default(),
                })
            })
            .await
        });
        started_rx.await.expect("probe worker started");

        drop(foreground);
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "probe child must retain foreground after waiter cancellation"
        );

        release_tx.send(()).expect("finish probe worker");
        wait_for_integrity_idle(&state).await;
        let _ = fs::remove_dir_all(root);
    }

    async fn wait_for_integrity_idle(state: &AppState) {
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut idle = state.subscribe_integrity_idle();
            loop {
                if idle.borrow_and_update().is_stably_idle() {
                    return;
                }
                idle.changed().await.expect("integrity idle channel");
            }
        })
        .await
        .expect("blocking integrity owner release");
    }

    fn test_state(name: &str) -> (AppState, PathBuf) {
        let root = test_root(name);
        let paths = test_paths(&root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        });
        (state, root)
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-java-override-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
