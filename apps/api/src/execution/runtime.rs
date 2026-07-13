//! Execution-owned Java/runtime capabilities.
//!
//! This module reports primitive runtime facts. It does not select fallbacks,
//! rewrite JVM arguments, or decide Guardian repair policy.

use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_token,
};
use crate::state::contracts::{OperationId, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::ownership::{classify_managed_runtime_root, protection_for};
use axial_minecraft::{
    ManagedRuntimeCache, RuntimeOverride, managed_runtime_contents_verified_without_probe,
    parse_runtime_override, runtime_executable_ready_without_probe,
};
use std::fmt;
use std::fs;
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
pub struct ManagedRuntimeVerificationRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub runtime_root: &'a Path,
    pub java_executable: &'a Path,
    pub require_ready_marker: bool,
}

impl std::fmt::Debug for ManagedRuntimeVerificationRequest<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeVerificationRequest")
            .field("operation_id", &self.operation_id)
            .field("target", &self.target)
            .field("require_ready_marker", &self.require_ready_marker)
            .finish_non_exhaustive()
    }
}

impl<'a> ManagedRuntimeVerificationRequest<'a> {
    pub fn new(
        target: TargetDescriptor,
        runtime_root: &'a Path,
        java_executable: &'a Path,
    ) -> Self {
        Self {
            operation_id: None,
            target,
            runtime_root,
            java_executable,
            require_ready_marker: true,
        }
    }

    pub fn without_ready_marker_requirement(mut self) -> Self {
        self.require_ready_marker = false;
        self
    }
}

#[derive(Clone)]
pub struct ManagedRuntimeRepairRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub runtime_root: ManagedRuntimeRoot<'a>,
    pub primitive: ManagedRuntimeRepairPrimitive,
}

impl std::fmt::Debug for ManagedRuntimeRepairRequest<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeRepairRequest")
            .field("operation_id", &self.operation_id)
            .field("target", &self.target)
            .field("runtime_root", &self.runtime_root)
            .field("primitive", &self.primitive)
            .finish()
    }
}

impl<'a> ManagedRuntimeRepairRequest<'a> {
    pub fn new(
        target: TargetDescriptor,
        runtime_root: ManagedRuntimeRoot<'a>,
        primitive: ManagedRuntimeRepairPrimitive,
    ) -> Self {
        Self {
            operation_id: None,
            target,
            runtime_root,
            primitive,
        }
    }
}

#[derive(Clone)]
pub struct ManagedRuntimeRoot<'a> {
    _runtime_cache: &'a ManagedRuntimeCache,
    target: TargetDescriptor,
    runtime_root: &'a Path,
    java_executable: &'a Path,
}

impl std::fmt::Debug for ManagedRuntimeRoot<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeRoot")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl<'a> ManagedRuntimeRoot<'a> {
    pub fn from_managed_root(
        runtime_cache: &'a ManagedRuntimeCache,
        runtime_root: &'a Path,
        java_executable: &'a Path,
    ) -> Result<Self, ManagedRuntimeRootError> {
        let classification = classify_managed_runtime_root(runtime_cache, runtime_root)
            .ok_or(ManagedRuntimeRootError::UnsupportedRoot)?;
        if !classification.allows_automatic_managed_mutation() {
            return Err(ManagedRuntimeRootError::UnsupportedRoot);
        }
        if path_has_parent_component(java_executable) || !java_executable.starts_with(runtime_root)
        {
            return Err(ManagedRuntimeRootError::JavaExecutableOutsideRoot);
        }

        Ok(Self {
            _runtime_cache: runtime_cache,
            target: classification.target,
            runtime_root,
            java_executable,
        })
    }

    pub fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    pub fn path(&self) -> &Path {
        self.runtime_root
    }

    pub fn java_executable(&self) -> &Path {
        self.java_executable
    }
}

fn path_has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedRuntimeRootError {
    UnsupportedRoot,
    JavaExecutableOutsideRoot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedRuntimeRepairPrimitive {
    QuarantineBrokenRuntime,
    RemoveBrokenRuntime,
    RecreateReadyMarker,
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

pub fn verify_managed_runtime(
    request: ManagedRuntimeVerificationRequest<'_>,
) -> Result<RuntimeCapabilityReport, RuntimeCapabilityError> {
    let mut facts = Vec::new();
    validate_managed_runtime_target(&request.target, request.operation_id.as_ref(), &mut facts)?;

    let ready_marker = request.runtime_root.join(".axial-ready");
    if request.require_ready_marker && !ready_marker.is_file() {
        let kind = if ready_marker.exists() {
            RuntimeCapabilityErrorKind::RuntimeCorrupt
        } else {
            RuntimeCapabilityErrorKind::ReadyMarkerMissing
        };
        facts.push(runtime_fact(
            if ready_marker.exists() {
                ExecutionFactKind::RuntimeCorrupt
            } else {
                ExecutionFactKind::RuntimeReadyMarkerMissing
            },
            request.operation_id.clone(),
            &request.target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(kind, facts));
    }

    if !runtime_executable_ready_without_probe(request.java_executable) {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            request.operation_id.clone(),
            &request.target,
            Vec::new(),
        ));
        if request.runtime_root.exists() {
            facts.push(runtime_fact(
                ExecutionFactKind::RuntimeCorrupt,
                request.operation_id.clone(),
                &request.target,
                Vec::new(),
            ));
        }
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::MissingExecutable,
            facts,
        ));
    }

    if !managed_runtime_contents_verified_without_probe(request.runtime_root) {
        facts.push(runtime_fact(
            ExecutionFactKind::RuntimeCorrupt,
            request.operation_id.clone(),
            &request.target,
            Vec::new(),
        ));
        return Err(RuntimeCapabilityError::new(
            RuntimeCapabilityErrorKind::RuntimeCorrupt,
            facts,
        ));
    }

    Ok(RuntimeCapabilityReport {
        target: request.target,
        facts,
        probe: None,
    })
}

pub fn validate_managed_runtime_repair(
    request: ManagedRuntimeRepairRequest<'_>,
) -> Result<RuntimeCapabilityReport, RuntimeCapabilityError> {
    let mut facts = Vec::new();
    validate_managed_runtime_target(&request.target, request.operation_id.as_ref(), &mut facts)?;
    validate_managed_runtime_root_target(
        &request.target,
        request.runtime_root.target(),
        request.operation_id.as_ref(),
        &mut facts,
    )?;
    let _primitive = request.primitive;
    let _runtime_root = request.runtime_root.path();
    let _java_executable = request.runtime_root.java_executable();

    Ok(RuntimeCapabilityReport {
        target: request.target,
        facts,
        probe: None,
    })
}

pub fn repair_managed_runtime(
    request: ManagedRuntimeRepairRequest<'_>,
) -> Result<RuntimeCapabilityReport, RuntimeCapabilityError> {
    let mut report = validate_managed_runtime_repair(ManagedRuntimeRepairRequest {
        operation_id: request.operation_id.clone(),
        target: request.target.clone(),
        runtime_root: request.runtime_root.clone(),
        primitive: request.primitive,
    })?;

    match request.primitive {
        ManagedRuntimeRepairPrimitive::RecreateReadyMarker => {
            if !runtime_executable_ready_without_probe(request.runtime_root.java_executable())
                || !request
                    .runtime_root
                    .path()
                    .join(".axial-runtime-manifest.json")
                    .is_file()
            {
                report.facts.push(runtime_fact(
                    ExecutionFactKind::RuntimeCorrupt,
                    request.operation_id.clone(),
                    &request.target,
                    Vec::new(),
                ));
                return Err(RuntimeCapabilityError::new(
                    RuntimeCapabilityErrorKind::RuntimeCorrupt,
                    report.facts,
                ));
            }
            recreate_ready_marker(request.runtime_root.path()).map_err(|_| {
                let mut facts = report.facts.clone();
                facts.push(runtime_fact(
                    ExecutionFactKind::PrimitiveRefused,
                    request.operation_id.clone(),
                    &request.target,
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
                &request.target,
                vec![EvidenceField::new(
                    "primitive",
                    "recreate_ready_marker",
                    EvidenceSensitivity::Public,
                )],
            ));
            match verify_managed_runtime(ManagedRuntimeVerificationRequest {
                operation_id: request.operation_id.clone(),
                target: request.target.clone(),
                runtime_root: request.runtime_root.path(),
                java_executable: request.runtime_root.java_executable(),
                require_ready_marker: true,
            }) {
                Ok(verification) => {
                    report.facts.extend(verification.facts);
                    Ok(report)
                }
                Err(error) => {
                    report.facts.extend(error.facts);
                    Err(RuntimeCapabilityError::new(error.kind, report.facts))
                }
            }
        }
        ManagedRuntimeRepairPrimitive::QuarantineBrokenRuntime
        | ManagedRuntimeRepairPrimitive::RemoveBrokenRuntime => Ok(report),
    }
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

fn validate_managed_runtime_root_target(
    target: &TargetDescriptor,
    runtime_root_target: &TargetDescriptor,
    operation_id: Option<&OperationId>,
    facts: &mut Vec<ExecutionFact>,
) -> Result<(), RuntimeCapabilityError> {
    validate_managed_runtime_target(runtime_root_target, operation_id, facts)?;
    if target.id == runtime_root_target.id
        && target.system == runtime_root_target.system
        && target.kind == runtime_root_target.kind
        && target.ownership == runtime_root_target.ownership
    {
        return Ok(());
    }

    facts.push(runtime_fact(
        ExecutionFactKind::PrimitiveRefused,
        operation_id.cloned(),
        runtime_root_target,
        Vec::new(),
    ));
    Err(RuntimeCapabilityError::new(
        RuntimeCapabilityErrorKind::UnsupportedTarget,
        facts.clone(),
    ))
}

fn recreate_ready_marker(runtime_root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(runtime_root)?;
    let ready_marker = runtime_root.join(".axial-ready");
    if ready_marker.is_dir() {
        fs::remove_dir_all(&ready_marker)?;
    } else if ready_marker.exists() {
        fs::remove_file(&ready_marker)?;
    }
    fs::write(ready_marker, b"ready")
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
        JavaProbeRunner, ManagedRuntimeRepairPrimitive, ManagedRuntimeRepairRequest,
        ManagedRuntimeRoot, ManagedRuntimeRootError, ManagedRuntimeVerificationRequest,
        RuntimeCapabilityErrorKind, RuntimeProbeFailure, RuntimeProbeInfo, RuntimeProbeRequest,
        inspect_java_override_value, java_override_is_undefined_sentinel,
        probe_java_runtime_with_runner, repair_managed_runtime, validate_managed_runtime_repair,
        verify_managed_runtime,
    };
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
    use axial_minecraft::ManagedRuntimeCache;
    use sha1::{Digest, Sha1};
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
        let root = test_root("ready-marker-missing");
        let runtime_root = root.join("java-runtime-delta");
        let java_path = runtime_root.join("bin").join("java");
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");

        let error = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(
            managed_runtime_target(),
            &runtime_root,
            &java_path,
        ))
        .expect_err("missing ready marker should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::ReadyMarkerMissing);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeReadyMarkerMissing
        ));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_verification_reports_corrupt_marker_shape() {
        let root = test_root("corrupt-marker");
        let runtime_root = root.join("java-runtime-delta");
        let java_path = runtime_root.join("bin").join("java");
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        fs::create_dir(runtime_root.join(".axial-ready")).expect("bad ready marker");

        let error = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(
            managed_runtime_target(),
            &runtime_root,
            &java_path,
        ))
        .expect_err("corrupt marker should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::RuntimeCorrupt);
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_verification_reports_corrupt_missing_executable() {
        let root = test_root("corrupt-missing-executable");
        let runtime_root = root.join("java-runtime-delta");
        let java_path = runtime_root.join("bin").join("java");
        fs::create_dir_all(&runtime_root).expect("runtime root");
        fs::write(runtime_root.join(".axial-ready"), b"ready").expect("ready marker");

        let error = verify_managed_runtime(ManagedRuntimeVerificationRequest::new(
            managed_runtime_target(),
            &runtime_root,
            &java_path,
        ))
        .expect_err("missing executable should fail");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::MissingExecutable);
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeMissingExecutable
        ));
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn user_java_override_and_unknown_targets_refuse_managed_repair() {
        let root = test_root("repair-refused");
        let runtime_cache = managed_runtime_cache();
        let user_target = classify_current_artifact(
            CurrentArtifact::UserJavaOverride,
            r"C:\Users\Alice\AppData\Local\java.exe",
        )
        .target;
        let unknown_target =
            classify_current_artifact(CurrentArtifact::UnknownFilesystemPath, "/home/alice/java")
                .target;

        for target in [user_target, unknown_target] {
            let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
            let java_path = runtime_root.join("bin").join("java");
            let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);
            let error = validate_managed_runtime_repair(ManagedRuntimeRepairRequest::new(
                target,
                runtime_root,
                ManagedRuntimeRepairPrimitive::RemoveBrokenRuntime,
            ))
            .expect_err("protected runtime target should refuse repair");

            assert_eq!(error.kind, RuntimeCapabilityErrorKind::OwnershipRefused);
            assert!(has_fact(&error.facts, ExecutionFactKind::PrimitiveRefused));
            assert_no_sensitive_runtime_material(&error.facts);
        }
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_repair_refuses_unsupported_target_shapes() {
        let root = test_root("repair-unsupported-target");
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = runtime_root.join("bin").join("java");
        for target in [
            TargetDescriptor::new(
                StabilizationSystem::Guardian,
                TargetKind::Runtime,
                "java-runtime-delta",
                OwnershipClass::LauncherManaged,
            ),
            TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "java-runtime-delta",
                OwnershipClass::LauncherManaged,
            ),
        ] {
            let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);
            let error = repair_managed_runtime(ManagedRuntimeRepairRequest::new(
                target,
                runtime_root,
                ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
            ))
            .expect_err("unsupported runtime target should refuse repair");

            assert_eq!(error.kind, RuntimeCapabilityErrorKind::UnsupportedTarget);
            assert!(has_fact(&error.facts, ExecutionFactKind::PrimitiveRefused));
            assert_no_sensitive_runtime_material(&error.facts);
        }
        assert!(!runtime_root.join(".axial-ready").exists());
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_repair_refuses_mismatched_root_target() {
        let root = test_root("repair-mismatched-root-target");
        let runtime_cache = managed_runtime_cache();
        let target = managed_runtime_target();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-epsilon");
        let java_path = runtime_root.join("bin").join("java");
        let runtime_root_binding = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);

        let error = repair_managed_runtime(ManagedRuntimeRepairRequest::new(
            target,
            runtime_root_binding,
            ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
        ))
        .expect_err("mismatched runtime root target should refuse repair");

        assert_eq!(error.kind, RuntimeCapabilityErrorKind::UnsupportedTarget);
        assert!(has_fact(&error.facts, ExecutionFactKind::PrimitiveRefused));
        assert!(!runtime_root.join(".axial-ready").exists());
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_root_binding_refuses_paths_outside_owned_runtime_root() {
        let root = test_root("repair-root-binding");
        let runtime_cache_a = managed_runtime_cache();
        let runtime_cache_b = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache_a, "java-runtime-delta");
        let other_cache_root = managed_runtime_root(&runtime_cache_b, "java-runtime-delta");
        let java_path = runtime_root.join("bin").join("java");
        let outside_root = root.join("user-runtime");
        let outside_java = root.join("other").join("bin").join("java");
        let escaping_java = runtime_root
            .join("..")
            .join("other")
            .join("bin")
            .join("java");

        assert_eq!(
            ManagedRuntimeRoot::from_managed_root(
                &runtime_cache_a,
                &outside_root,
                &outside_root.join("java"),
            )
            .expect_err("outside root"),
            ManagedRuntimeRootError::UnsupportedRoot
        );
        assert_eq!(
            ManagedRuntimeRoot::from_managed_root(
                &runtime_cache_a,
                &other_cache_root,
                &other_cache_root.join("bin").join("java"),
            )
            .expect_err("root from another managed runtime cache"),
            ManagedRuntimeRootError::UnsupportedRoot
        );
        assert_eq!(
            ManagedRuntimeRoot::from_managed_root(&runtime_cache_a, &runtime_root, &outside_java)
                .expect_err("java outside root"),
            ManagedRuntimeRootError::JavaExecutableOutsideRoot
        );
        assert_eq!(
            ManagedRuntimeRoot::from_managed_root(&runtime_cache_a, &runtime_root, &escaping_java)
                .expect_err("java with parent component"),
            ManagedRuntimeRootError::JavaExecutableOutsideRoot
        );
        assert!(
            ManagedRuntimeRoot::from_managed_root(&runtime_cache_a, &runtime_root, &java_path)
                .is_ok()
        );
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_request_debug_redacts_paths() {
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root);
        let target = managed_runtime_target();
        let bound = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);
        let bound_debug = format!("{bound:?}");
        let verification_debug = format!(
            "{:?}",
            ManagedRuntimeVerificationRequest::new(target.clone(), &runtime_root, &java_path)
        );
        let repair_debug = format!(
            "{:?}",
            ManagedRuntimeRepairRequest::new(
                target,
                bound,
                ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
            )
        );
        let runtime_root = runtime_root.to_string_lossy();
        let java_path = java_path.to_string_lossy();

        for debug in [bound_debug, verification_debug, repair_debug] {
            assert!(!debug.contains(runtime_root.as_ref()));
            assert!(!debug.contains(java_path.as_ref()));
        }
    }

    #[test]
    fn managed_runtime_repair_recreates_ready_marker() {
        let root = test_root("repair-ready-marker");
        let runtime_cache = managed_runtime_cache();
        let runtime_root_path = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = managed_runtime_java_path(&runtime_root_path);
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        make_executable(&java_path);
        write_runtime_manifest_proof(&runtime_root_path, &java_path);
        fs::create_dir_all(runtime_root_path.join(".axial-ready")).expect("bad marker dir");
        let runtime_root = runtime_root_binding(&runtime_cache, &runtime_root_path, &java_path);

        let report = repair_managed_runtime(ManagedRuntimeRepairRequest::new(
            managed_runtime_target(),
            runtime_root,
            ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
        ))
        .expect("repair ready marker");

        assert!(runtime_root_path.join(".axial-ready").is_file());
        assert!(has_fact(
            &report.facts,
            ExecutionFactKind::RuntimeRepairApplied
        ));
        assert_no_sensitive_runtime_material(&report.facts);
        cleanup(&root);
    }

    #[test]
    fn managed_runtime_repair_fails_when_postcondition_is_unverified() {
        let root = test_root("repair-postcondition-failed");
        let runtime_cache = managed_runtime_cache();
        let runtime_root = managed_runtime_root(&runtime_cache, "java-runtime-delta");
        let java_path = runtime_root.join("bin").join("java");
        let runtime_root_binding = runtime_root_binding(&runtime_cache, &runtime_root, &java_path);

        let error = repair_managed_runtime(ManagedRuntimeRepairRequest::new(
            managed_runtime_target(),
            runtime_root_binding,
            ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
        ))
        .expect_err("missing executable should fail post-repair verification");

        assert!(!runtime_root.join(".axial-ready").exists());
        assert_eq!(error.kind, RuntimeCapabilityErrorKind::RuntimeCorrupt);
        assert!(!has_fact(
            &error.facts,
            ExecutionFactKind::RuntimeRepairApplied
        ));
        assert!(has_fact(&error.facts, ExecutionFactKind::RuntimeCorrupt));
        assert_no_sensitive_runtime_material(&error.facts);
        cleanup(&root);
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

    fn managed_runtime_target() -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            "java-runtime-delta",
            OwnershipClass::LauncherManaged,
        )
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

    fn write_runtime_manifest_proof(runtime_root: &Path, java_path: &Path) {
        let bytes = fs::read(java_path).expect("read fake java");
        let relative_path = java_path
            .strip_prefix(runtime_root)
            .expect("java under runtime root")
            .to_string_lossy()
            .replace('\\', "/");
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        let sha1 = format!("{:x}", hasher.finalize());
        let manifest = serde_json::json!({
            "files": {
                relative_path: {
                    "type": "file",
                    "downloads": {
                        "raw": {
                            "url": "https://example.invalid/java",
                            "sha1": sha1,
                            "size": bytes.len()
                        }
                    }
                }
            }
        });
        fs::write(
            runtime_root.join(".axial-runtime-manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest json"),
        )
        .expect("runtime manifest proof");
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
            .component_root(runtime_id)
            .expect("known managed runtime component")
    }

    fn runtime_root_binding<'a>(
        runtime_cache: &'a ManagedRuntimeCache,
        runtime_root: &'a Path,
        java_path: &'a Path,
    ) -> ManagedRuntimeRoot<'a> {
        ManagedRuntimeRoot::from_managed_root(runtime_cache, runtime_root, java_path)
            .expect("managed runtime root binding")
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
