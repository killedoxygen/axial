//! Execution-owned Java/runtime capabilities.
//!
//! This module reports primitive runtime facts. It does not select fallbacks,
//! rewrite JVM arguments, or decide Guardian repair policy.

use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_token,
};
use crate::state::contracts::{OperationId, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::ownership::{classify_managed_runtime_component, protection_for};
use axial_minecraft::{
    ManagedRuntimeCache, ManagedRuntimeComponent, ManagedRuntimeMarkerState, RuntimeOverride,
    parse_runtime_override, runtime_executable_ready_without_probe,
};
use std::fmt;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct RuntimeProbeRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub java_path: &'a Path,
    pub id_hint: Option<&'a str>,
    pub required_major: Option<u32>,
    pub required_min_update: Option<u32>,
}

impl<'a> RuntimeProbeRequest<'a> {
    pub fn new(target: TargetDescriptor, java_path: &'a Path) -> Self {
        Self {
            operation_id: None,
            target,
            java_path,
            id_hint: None,
            required_major: None,
            required_min_update: None,
        }
    }

    pub fn with_id_hint(mut self, id_hint: &'a str) -> Self {
        self.id_hint = Some(id_hint);
        self
    }

    pub fn with_required_major(mut self, required_major: u32) -> Self {
        self.required_major = Some(required_major);
        self
    }

    pub fn with_required_min_update(mut self, required_min_update: u32) -> Self {
        self.required_min_update = Some(required_min_update);
        self
    }
}

#[derive(Clone)]
pub(crate) struct ManagedRuntimeVerificationRequest {
    operation_id: Option<OperationId>,
    runtime_root: ManagedRuntimeRoot,
}

impl std::fmt::Debug for ManagedRuntimeVerificationRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeVerificationRequest")
            .field("operation_id", &self.operation_id)
            .field("target", self.runtime_root.target())
            .finish_non_exhaustive()
    }
}

impl ManagedRuntimeVerificationRequest {
    pub(crate) fn new(runtime_root: ManagedRuntimeRoot) -> Self {
        Self {
            operation_id: None,
            runtime_root,
        }
    }
}

pub(crate) struct ManagedRuntimeRepairRequest {
    operation_id: Option<OperationId>,
    runtime_root: ManagedRuntimeRoot,
}

impl std::fmt::Debug for ManagedRuntimeRepairRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeRepairRequest")
            .field("operation_id", &self.operation_id)
            .field("target", self.runtime_root.target())
            .field("runtime_root", &self.runtime_root)
            .finish()
    }
}

impl ManagedRuntimeRepairRequest {
    pub(crate) fn new(runtime_root: ManagedRuntimeRoot) -> Self {
        Self {
            operation_id: None,
            runtime_root,
        }
    }

    pub(crate) fn with_operation_id(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }
}

#[derive(Clone)]
pub(crate) struct ManagedRuntimeRoot {
    authority: ManagedRuntimeComponent,
    target: TargetDescriptor,
}

impl std::fmt::Debug for ManagedRuntimeRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeRoot")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl ManagedRuntimeRoot {
    pub(crate) fn from_component(
        authority: ManagedRuntimeComponent,
    ) -> Result<Self, ManagedRuntimeRootError> {
        authority
            .validate_projection()
            .map_err(|_| ManagedRuntimeRootError::UnsupportedRoot)?;
        let classification = classify_managed_runtime_component(&authority);
        if !classification.allows_automatic_managed_mutation() {
            return Err(ManagedRuntimeRootError::UnsupportedRoot);
        }

        Ok(Self {
            authority,
            target: classification.target,
        })
    }

    pub(crate) fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    fn authority(&self) -> &ManagedRuntimeComponent {
        &self.authority
    }

    pub(crate) fn belongs_to(&self, runtime_cache: &ManagedRuntimeCache) -> bool {
        self.authority.belongs_to(runtime_cache)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedRuntimeRootError {
    UnsupportedRoot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCapabilityReport {
    pub target: TargetDescriptor,
    pub facts: Vec<ExecutionFact>,
    pub probe: Option<RuntimeProbeInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JavaOverrideInspection {
    pub target: TargetDescriptor,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeProbeInfo {
    pub id: String,
    pub major: u32,
    pub update: u32,
    pub distribution: String,
}

impl RuntimeProbeInfo {
    pub fn new(
        id: impl AsRef<str>,
        major: u32,
        update: u32,
        distribution: impl AsRef<str>,
    ) -> Self {
        Self {
            id: sanitize_runtime_token(id.as_ref(), "runtime"),
            major,
            update,
            distribution: sanitize_runtime_token(distribution.as_ref(), "unknown"),
        }
    }
}

#[derive(Debug)]
pub struct RuntimeCapabilityError {
    pub kind: RuntimeCapabilityErrorKind,
    pub facts: Vec<ExecutionFact>,
}

impl RuntimeCapabilityError {
    fn new(kind: RuntimeCapabilityErrorKind, facts: Vec<ExecutionFact>) -> Self {
        Self { kind, facts }
    }
}

impl fmt::Display for RuntimeCapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            RuntimeCapabilityErrorKind::OwnershipRefused => {
                formatter.write_str("runtime capability refused target ownership")
            }
            RuntimeCapabilityErrorKind::UnsupportedTarget => {
                formatter.write_str("runtime capability refused unsupported target")
            }
            RuntimeCapabilityErrorKind::MissingExecutable => {
                formatter.write_str("java executable is missing")
            }
            RuntimeCapabilityErrorKind::ProbeFailed => {
                formatter.write_str("java runtime probe failed")
            }
            RuntimeCapabilityErrorKind::WrongMajor => {
                formatter.write_str("java runtime major version mismatch")
            }
            RuntimeCapabilityErrorKind::WrongUpdate => {
                formatter.write_str("java runtime update version is too old")
            }
            RuntimeCapabilityErrorKind::ReadyMarkerMissing => {
                formatter.write_str("managed runtime ready marker is missing")
            }
            RuntimeCapabilityErrorKind::RuntimeCorrupt => {
                formatter.write_str("managed runtime is corrupt")
            }
            RuntimeCapabilityErrorKind::RepairFailed => {
                formatter.write_str("managed runtime repair failed")
            }
        }
    }
}

impl std::error::Error for RuntimeCapabilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCapabilityErrorKind {
    OwnershipRefused,
    UnsupportedTarget,
    MissingExecutable,
    ProbeFailed,
    WrongMajor,
    WrongUpdate,
    ReadyMarkerMissing,
    RuntimeCorrupt,
    RepairFailed,
}

pub trait JavaProbeRunner {
    fn probe(
        &self,
        java_path: &Path,
        id_hint: Option<&str>,
    ) -> Result<RuntimeProbeInfo, RuntimeProbeFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProbeFailure {
    SpawnFailed,
    TimedOut,
    OutputParseFailed,
    Unknown,
}

pub fn inspect_java_override_value(
    operation_id: Option<OperationId>,
    target: TargetDescriptor,
    raw_value: &str,
) -> JavaOverrideInspection {
    let mut facts = Vec::new();
    let trimmed = raw_value.trim();
    if !raw_value.is_empty() && trimmed.is_empty() {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeJavaOverrideEmpty,
            operation_id,
            &target,
            Vec::new(),
        ));
    } else if java_override_is_undefined_sentinel(trimmed) {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel,
            operation_id,
            &target,
            vec![EvidenceField::new(
                "sentinel",
                trimmed.to_ascii_lowercase(),
                EvidenceSensitivity::Public,
            )],
        ));
    } else if let RuntimeOverride::ExecutablePath(path) = parse_runtime_override(trimmed)
        && !runtime_executable_ready_without_probe(&path)
    {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            operation_id,
            &target,
            Vec::new(),
        ));
    }

    JavaOverrideInspection { target, facts }
}

pub fn missing_java_override(
    operation_id: Option<OperationId>,
    target: TargetDescriptor,
) -> JavaOverrideInspection {
    JavaOverrideInspection {
        facts: vec![runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            operation_id,
            &target,
            Vec::new(),
        )],
        target,
    }
}

pub fn java_override_is_undefined_sentinel(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "undefined" | "null"
    )
}

pub fn probe_java_runtime_with_runner(
    request: RuntimeProbeRequest<'_>,
    runner: &impl JavaProbeRunner,
) -> Result<RuntimeCapabilityReport, RuntimeCapabilityError> {
    let mut facts = Vec::new();

    if !request.java_path.is_file() {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            request.operation_id.clone(),
            &request.target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::MissingExecutable,
            facts,
        ));
    }

    let info = match runner.probe(request.java_path, request.id_hint) {
        Ok(info) => info,
        Err(error) => {
            facts.push(runtime_fact(
                ExecutionFactKind::RuntimeProbeFailed,
                request.operation_id.clone(),
                &request.target,
                vec![EvidenceField::new(
                    "probe_failure",
                    probe_failure_label(error),
                    EvidenceSensitivity::Public,
                )],
            ));
            return Err(RuntimeCapabilityError::new(
                RuntimeCapabilityErrorKind::ProbeFailed,
                facts,
            ));
        }
    };

    if info.major == 0 {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeProbeFailed,
            request.operation_id.clone(),
            &request.target,
            vec![EvidenceField::new(
                "probe_failure",
                probe_failure_label(RuntimeProbeFailure::OutputParseFailed),
                EvidenceSensitivity::Public,
            )],
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::ProbeFailed,
            facts,
        ));
    }

    if let Some(required_major) = request.required_major
        && required_major > 0
        && info.major != required_major
    {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeWrongMajor,
            request.operation_id.clone(),
            &request.target,
            vec![
                EvidenceField::new(
                    "required_major",
                    required_major.to_string(),
                    EvidenceSensitivity::Public,
                ),
                EvidenceField::new(
                    "actual_major",
                    info.major.to_string(),
                    EvidenceSensitivity::Public,
                ),
            ],
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::WrongMajor,
            facts,
        ));
    }

    if let Some(required_min_update) = request.required_min_update
        && required_min_update > 0
        && info.update > 0
        && info.update < required_min_update
        && request
            .required_major
            .is_none_or(|required_major| required_major == info.major)
    {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeWrongUpdate,
            request.operation_id.clone(),
            &request.target,
            vec![
                EvidenceField::new(
                    "required_min_update",
                    required_min_update.to_string(),
                    EvidenceSensitivity::Public,
                ),
                EvidenceField::new(
                    "actual_update",
                    info.update.to_string(),
                    EvidenceSensitivity::Public,
                ),
            ],
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::WrongUpdate,
            facts,
        ));
    }

    Ok(RuntimeCapabilityReport {
        target: request.target,
        facts,
        probe: Some(info),
    })
}

pub(crate) fn verify_managed_runtime(
    request: ManagedRuntimeVerificationRequest,
) -> Result<RuntimeCapabilityReport, RuntimeCapabilityError> {
    let mut facts = Vec::new();
    let target = request.runtime_root.target().clone();
    validate_managed_runtime_target(&target, request.operation_id.as_ref(), &mut facts)?;

    let marker_state = request.runtime_root.authority().marker_state();
    if marker_state != ManagedRuntimeMarkerState::Ready {
        let kind = match marker_state {
            ManagedRuntimeMarkerState::Missing => RuntimeCapabilityErrorKind::ReadyMarkerMissing,
            ManagedRuntimeMarkerState::Ready | ManagedRuntimeMarkerState::Corrupt => {
                RuntimeCapabilityErrorKind::RuntimeCorrupt
            }
        };
        facts.push(runtime_fact(
            match marker_state {
                ManagedRuntimeMarkerState::Missing => ExecutionFactKind::RuntimeReadyMarkerMissing,
                ManagedRuntimeMarkerState::Ready | ManagedRuntimeMarkerState::Corrupt => {
                    ExecutionFactKind::RuntimeCorrupt
                }
            },
            request.operation_id.clone(),
            &target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(kind, facts));
    }

    if !request.runtime_root.authority().executable_ready() {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            request.operation_id.clone(),
            &target,
            Vec::new(),
        ));
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeCorrupt,
            request.operation_id.clone(),
            &target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::MissingExecutable,
            facts,
        ));
    }

    if !request.runtime_root.authority().contents_verified() {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeCorrupt,
            request.operation_id.clone(),
            &target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::RuntimeCorrupt,
            facts,
        ));
    }

    Ok(RuntimeCapabilityReport {
        target,
        facts,
        probe: None,
    })
}

fn validate_managed_runtime_repair(
    request: &ManagedRuntimeRepairRequest,
) -> Result<Vec<ExecutionFact>, RuntimeCapabilityError> {
    let mut facts = Vec::new();
    validate_managed_runtime_target(
        request.runtime_root.target(),
        request.operation_id.as_ref(),
        &mut facts,
    )?;
    Ok(facts)
}

pub(crate) fn repair_managed_runtime(
    request: ManagedRuntimeRepairRequest,
) -> Result<RuntimeCapabilityReport, RuntimeCapabilityError> {
    let facts = validate_managed_runtime_repair(&request)?;
    let target = request.runtime_root.target().clone();
    let mut report = RuntimeCapabilityReport {
        target: target.clone(),
        facts,
        probe: None,
    };

    if request.runtime_root.authority().marker_state() != ManagedRuntimeMarkerState::Missing
        || !request.runtime_root.authority().executable_ready()
        || !request.runtime_root.authority().manifest_proof_valid()
    {
        report.facts.push(runtime_fact(
            ExecutionFactKind::RuntimeCorrupt,
            request.operation_id.clone(),
            &target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::RuntimeCorrupt,
            report.facts,
        ));
    }
    request
        .runtime_root
        .authority()
        .repair_ready_marker()
        .map_err(|_| {
            let mut facts = report.facts.clone();
            facts.push(runtime_fact(
                ExecutionFactKind::PrimitiveRefused,
                request.operation_id.clone(),
                &target,
                vec![EvidenceField::new(
                    "primitive",
                    "recreate_ready_marker",
                    EvidenceSensitivity::Public,
                )],
            ));
            RuntimeCapabilityError::new(RuntimeCapabilityErrorKind::RepairFailed, facts)
        })?;
    report.facts.push(runtime_fact(
        ExecutionFactKind::RuntimeRepairApplied,
        request.operation_id.clone(),
        &target,
        vec![EvidenceField::new(
            "primitive",
            "recreate_ready_marker",
            EvidenceSensitivity::Public,
        )],
    ));
    Ok(report)
}

pub fn runtime_fact(
    kind: ExecutionFactKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    extra_fields: Vec<EvidenceField>,
) -> ExecutionFact {
    let mut fields = vec![EvidenceField::new(
        "target",
        target.id.clone(),
        EvidenceSensitivity::Public,
    )];
    fields.extend(extra_fields);
    ExecutionFact {
        operation_id,
        kind,
        target: Some(target.clone()),
        fields,
    }
}

fn validate_managed_runtime_target(
    target: &TargetDescriptor,
    operation_id: Option<&OperationId>,
    facts: &mut Vec<ExecutionFact>,
) -> Result<(), RuntimeCapabilityError> {
    if !protection_for(target.ownership).allows_automatic_managed_mutation() {
        facts.push(runtime_fact(
            ExecutionFactKind::PrimitiveRefused,
            operation_id.cloned(),
            target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::OwnershipRefused,
            facts.clone(),
        ));
    }

    if target.system != StabilizationSystem::Execution || target.kind != TargetKind::Runtime {
        facts.push(runtime_fact(
            ExecutionFactKind::PrimitiveRefused,
            operation_id.cloned(),
            target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::UnsupportedTarget,
            facts.clone(),
        ));
    }

    Ok(())
}

fn sanitize_runtime_token(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| fallback.to_string())
}

fn probe_failure_label(failure: RuntimeProbeFailure) -> &'static str {
    match failure {
        RuntimeProbeFailure::SpawnFailed => "spawn_failed",
        RuntimeProbeFailure::TimedOut => "timed_out",
        RuntimeProbeFailure::OutputParseFailed => "output_parse_failed",
        RuntimeProbeFailure::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JavaProbeRunner, ManagedRuntimeRepairRequest, ManagedRuntimeRoot,
        ManagedRuntimeVerificationRequest, RuntimeCapabilityErrorKind, RuntimeProbeFailure,
        RuntimeProbeInfo, RuntimeProbeRequest, inspect_java_override_value,
        java_override_is_undefined_sentinel, probe_java_runtime_with_runner,
        repair_managed_runtime, verify_managed_runtime,
    };
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
    use axial_minecraft::ManagedRuntimeCache;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn missing_executable_emits_redacted_runtime_fact() {
        let root = test_root("missing-executable");
        let java_path = root.join("secret-user").join("bin").join("java");
        let target = classify_current_artifact(
            CurrentArtifact::UserJavaOverride,
            java_path.to_string_lossy(),
        )
        .target;

        let error = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path).with_required_major(21),
            &SuccessfulProbe { major: 21 },
        )
        .expect_err("missing executable should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::MissingExecutable);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeMissingExecutable
        ));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn probe_failure_emits_probe_failed_fact_without_path() {
        let root = test_root("probe-failed");
        let java_path = write_fake_java(&root);
        let target =
            classify_current_artifact(CurrentArtifact::UserJavaOverride, "manual_java").target;

        let error = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path).with_id_hint("java-runtime-delta"),
            &FailingProbe,
        )
        .expect_err("probe failure should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::ProbeFailed);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeProbeFailed
        ));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn probe_timeout_emits_bounded_probe_failure_reason() {
        let root = test_root("probe-timeout");
        let java_path = write_fake_java(&root);
        let target =
            classify_current_artifact(CurrentArtifact::UserJavaOverride, "manual_java").target;

        let error = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path).with_id_hint("java-runtime-delta"),
            &TimedOutProbe,
        )
        .expect_err("probe timeout should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::ProbeFailed);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeProbeFailed
        ));
        let encoded = serde_json::to_string(&error.facts).expect("facts json");
        assert!(encoded.contains("timed_out"));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn wrong_major_emits_expected_and_actual_without_java_path() {
        let root = test_root("wrong-major");
        let java_path = write_fake_java(&root);
        let target =
            classify_current_artifact(CurrentArtifact::ManagedRuntimeCache, "java-runtime-delta")
                .target;

        let error = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path)
                .with_id_hint("java-runtime-delta")
                .with_required_major(21),
            &SuccessfulProbe { major: 17 },
        )
        .expect_err("wrong major should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::WrongMajor);
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeWrongMajor));
        let encoded = serde_json::to_string(&error.facts).expect("facts json");
        assert!(encoded.contains("required_major"));
        assert!(encoded.contains("actual_major"));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn zero_major_probe_result_is_probe_failure_not_wrong_major() {
        let root = test_root("zero-major");
        let java_path = write_fake_java(&root);
        let target =
            classify_current_artifact(CurrentArtifact::ManagedRuntimeCache, "java-runtime-delta")
                .target;

        let error = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path)
                .with_id_hint("java-runtime-delta")
                .with_required_major(21),
            &SuccessfulProbe { major: 0 },
        )
        .expect_err("zero major should be a failed probe");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::ProbeFailed);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeProbeFailed
        ));
        assert!(!has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeWrongMajor
        ));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn wrong_update_emits_required_and_actual_without_java_path() {
        let root = test_root("wrong-update");
        let java_path = write_fake_java(&root);
        let target =
            classify_current_artifact(CurrentArtifact::UserJavaOverride, "manual_java").target;

        let error = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path)
                .with_required_major(8)
                .with_required_min_update(312),
            &SuccessfulProbeWithUpdate {
                major: 8,
                update: 311,
            },
        )
        .expect_err("old update should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::WrongUpdate);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeWrongUpdate
        ));
        let encoded = serde_json::to_string(&error.facts).expect("facts json");
        assert!(encoded.contains("required_min_update"));
        assert!(encoded.contains("actual_update"));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn unknown_update_does_not_emit_wrong_update_fact() {
        let root = test_root("unknown-update");
        let java_path = write_fake_java(&root);
        let target =
            classify_current_artifact(CurrentArtifact::UserJavaOverride, "manual_java").target;

        let report = probe_java_runtime_with_runner(
            RuntimeProbeRequest::new(target, &java_path)
                .with_required_major(8)
                .with_required_min_update(312),
            &SuccessfulProbeWithUpdate {
                major: 8,
                update: 0,
            },
        )
        .expect("unknown update should not fail");

        assert!(report.facts.is_empty());
        cleanup(&root);
    }

    #[test]
    fn explicit_empty_java_override_emits_redacted_runtime_fact() {
        let target = user_java_override_target("instance_java_override");

        let inspection = inspect_java_override_value(None, target.clone(), "   \t");

        assert_eq!(inspection.target, target);
        assert!(has_fact(
            &inspection.facts,
            ExecutionFactKind::RuntimeJavaOverrideEmpty
        ));
        assert_no_sensitive_runtime_material(&inspection.facts);
    }

    #[test]
    fn undefined_java_override_sentinels_emit_redacted_runtime_fact() {
        let target = user_java_override_target("global_java_override");

        for raw_value in ["undefined", " Undefined ", "null", " NULL "] {
            let inspection = inspect_java_override_value(None, target.clone(), raw_value);

            assert_eq!(inspection.target, target);
            assert!(has_fact(
                &inspection.facts,
                ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel
            ));
            assert_no_sensitive_runtime_material(&inspection.facts);
        }
    }

    #[test]
    fn missing_java_override_path_emits_missing_executable_fact_without_raw_path() {
        let target = user_java_override_target("instance_java_override");
        let inspection = inspect_java_override_value(
            None,
            target.clone(),
            "/Users/SecretUser/.jdks/missing/bin/java",
        );

        assert_eq!(inspection.target, target);
        assert!(has_fact(
            &inspection.facts,
            ExecutionFactKind::RuntimeMissingExecutable
        ));
        assert_no_sensitive_runtime_material(&inspection.facts);
    }

    #[test]
    fn absent_component_or_existing_java_override_values_do_not_emit_override_facts() {
        let target = user_java_override_target("instance_java_override");
        let root = test_root("existing-java-override");
        let java_path = write_fake_java(&root);

        for raw_value in [
            "",
            "java-runtime-delta",
            java_path.to_string_lossy().as_ref(),
        ] {
            let inspection = inspect_java_override_value(None, target.clone(), raw_value);

            assert!(inspection.facts.is_empty());
        }
        assert!(java_override_is_undefined_sentinel(" null "));
        assert!(!java_override_is_undefined_sentinel("/opt/null/bin/java"));
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_verification_reports_missing_ready_marker() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);

        let error = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(runtime_root))
            .expect_err("missing ready marker should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::ReadyMarkerMissing);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeReadyMarkerMissing
        ));
        assert_no_sensitive_runtime_material(&error.facts);
    }

    #[test]
    fn managed_runtime_verification_reports_corrupt_marker_shape() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        fs::create_dir(runtime_root.join(".axial-ready")).expect("bad ready marker");
        let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);

        let error = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(runtime_root))
            .expect_err("corrupt marker should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::RuntimeCorrupt);
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
    }

    #[test]
    fn managed_runtime_verification_reports_corrupt_missing_executable() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(&runtime_root).expect("runtime root");
        fs::write(runtime_root.join(".axial-ready"), b"ready").expect("ready marker");
        let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);

        let error = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(runtime_root))
            .expect_err("missing executable should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::MissingExecutable);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeMissingExecutable
        ));
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
    }

    #[test]
    fn managed_runtime_request_debug_redacts_paths() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(&runtime_root).expect("managed runtime component root");
        let bound = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);
        let bound_debug = format!("{bound:?}");
        let verification_debug = format!(
            "{:?}",
            ManagedRuntimeVerificationRequest::new(bound.clone())
        );
        let repair_debug = format!("{:?}", ManagedRuntimeRepairRequest::new(bound));
        let runtime_root = runtime_root.to_string_lossy();
        let java_path = java_path.to_string_lossy();

        for debug in [bound_debug, verification_debug, repair_debug] {
            assert!(!debug.contains(runtime_root.as_ref()));
            assert!(!debug.contains(java_path.as_ref()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_runtime_repair_recreates_ready_marker() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root_path = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root_path);
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        make_executable(&java_path);
        axial_minecraft::persist_managed_runtime_source_fixture_for_test(
            &runtime_cache,
            axial_minecraft::RuntimeId::from("java-runtime-delta"),
            "https://example.invalid/java".to_string(),
            b"java",
        )
        .expect("persist canonical runtime manifest proof");
        let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root_path, &java_path);

        let report = repair_managed_runtime(ManagedRuntimeRepairRequest::new(runtime_root))
            .expect("repair ready marker");

        assert!(runtime_root_path.join(".axial-ready").is_file());
        assert!(has_fact(
            &report.facts,
            ExecutionFactKind::RuntimeRepairApplied
        ));
        assert_no_sensitive_runtime_material(&report.facts);
    }

    #[test]
    fn managed_runtime_repair_refuses_corrupt_existing_marker() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root_path = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root_path);
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        make_executable(&java_path);
        let marker = runtime_root_path.join(".axial-ready");
        fs::create_dir(&marker).expect("corrupt marker directory");
        let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root_path, &java_path);

        let error = repair_managed_runtime(ManagedRuntimeRepairRequest::new(runtime_root))
            .expect_err("corrupt marker must not be replaced");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::RuntimeCorrupt);
        assert!(marker.is_dir());
        assert!(!has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeRepairApplied
        ));
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
    }

    #[test]
    fn managed_runtime_repair_refuses_missing_executable() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(&runtime_root).expect("runtime root");
        let runtime_root_binding = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);

        let error = repair_managed_runtime(ManagedRuntimeRepairRequest::new(runtime_root_binding))
            .expect_err("missing executable should fail repair admission");

        assert!(!runtime_root.join(".axial-ready").exists());
        assert_eq!(error.kind, RuntimeCapabilityErrorKind::RuntimeCorrupt);
        assert!(!has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeRepairApplied
        ));
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
    }

    struct SuccessfulProbe {
        major: u32,
    }

    impl JavaProbeRunner for SuccessfulProbe {
        fn probe(
            &self,
            _java_path: &Path,
            id_hint: Option<&str>,
        ) -> Result<RuntimeProbeInfo, RuntimeProbeFailure> {
            Ok(RuntimeProbeInfo::new(
                id_hint.unwrap_or("java-runtime-delta"),
                self.major,
                0,
                "openjdk",
            ))
        }
    }

    struct SuccessfulProbeWithUpdate {
        major: u32,
        update: u32,
    }

    impl JavaProbeRunner for SuccessfulProbeWithUpdate {
        fn probe(
            &self,
            _java_path: &Path,
            id_hint: Option<&str>,
        ) -> Result<RuntimeProbeInfo, RuntimeProbeFailure> {
            Ok(RuntimeProbeInfo::new(
                id_hint.unwrap_or("java-runtime-delta"),
                self.major,
                self.update,
                "openjdk",
            ))
        }
    }

    struct FailingProbe;

    impl JavaProbeRunner for FailingProbe {
        fn probe(
            &self,
            _java_path: &Path,
            _id_hint: Option<&str>,
        ) -> Result<RuntimeProbeInfo, RuntimeProbeFailure> {
            Err(RuntimeProbeFailure::SpawnFailed)
        }
    }

    struct TimedOutProbe;

    impl JavaProbeRunner for TimedOutProbe {
        fn probe(
            &self,
            _java_path: &Path,
            _id_hint: Option<&str>,
        ) -> Result<RuntimeProbeInfo, RuntimeProbeFailure> {
            Err(RuntimeProbeFailure::TimedOut)
        }
    }

    fn has_fact(facts: &[crate::execution::ExecutionFact], kind: ExecutionFactKind) -> bool {
        facts.iter().any(|fact| fact.kind == kind)
    }

    fn assert_no_sensitive_runtime_material(facts: &[crate::execution::ExecutionFact]) {
        let encoded = serde_json::to_string(facts).expect("facts json");
        let lower = encoded.to_ascii_lowercase();
        assert!(!lower.contains("/home/"));
        assert!(!lower.contains("users\\\\alice"));
        assert!(!lower.contains("appdata"));
        assert!(!lower.contains("secret-user"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("--classpath"));
    }

    fn user_java_override_target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Config,
            id,
            OwnershipClass::UserOwned,
        )
    }

    fn write_fake_java(root: &Path) -> PathBuf {
        let java_path = root.join("secret-user").join("bin").join("java");
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("java parent");
        fs::write(&java_path, b"java").expect("fake java");
        make_executable(&java_path);
        java_path
    }

    fn managed_runtime_java_path(runtime_root: &Path) -> PathBuf {
        if cfg!(target_os = "macos") {
            return runtime_root
                .join("jre.bundle")
                .join("Contents")
                .join("Home")
                .join("bin")
                .join("java");
        }

        runtime_root
            .join("bin")
            .join(if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            })
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("java metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("java executable");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    fn managed_runtime_cache() -> ManagedRuntimeCache {
        ManagedRuntimeCache::isolated_for_test().expect("isolated managed runtime cache")
    }

    fn managed_runtime_root(runtime_cache: &ManagedRuntimeCache, runtime_id: &str) -> PathBuf {
        runtime_cache
            .component_root_for_test(runtime_id)
            .expect("known managed runtime component")
    }

    fn runtime_root_binding(
        runtime_cache: &ManagedRuntimeCache,
        runtime_root: &Path,
        java_path: &Path,
    ) -> ManagedRuntimeRoot {
        let component = runtime_root
            .file_name()
            .and_then(|component| component.to_str())
            .expect("runtime component name");
        let authority = runtime_cache
            .admit_component(component)
            .expect("runtime component admission")
            .expect("runtime component");
        assert_eq!(authority.root_path(), runtime_root);
        assert_eq!(authority.java_executable_path(), java_path);
        ManagedRuntimeRoot::from_component(authority).expect("managed runtime root binding")
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-runtime-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
