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
    AppState, JavaProbeFailureClaim, JavaProbeFailureKey, JavaProbeFailureKind,
    JavaProbeFailureOwner,
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
    let snapshot =
        match tokio::task::spawn_blocking(move || snapshot_java_runtime(&snapshot_path)).await {
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
    let resolution = resolve_probe(producer, snapshot, prior_receipt, cache_owner).await;
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
    snapshot: axial_minecraft::JavaRuntimeProbeSnapshot,
    prior_receipt: Option<JavaRuntimeProbeReceipt>,
    cache_owner: Option<Box<JavaProbeFailureOwner>>,
) -> Result<JavaRuntimeProbeResolution, JavaRuntimeProbeResolutionError> {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.spawn_child(async move {
        let result = tokio::task::spawn_blocking(move || {
            resolve_java_runtime_probe(snapshot, prior_receipt, None)
        })
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
