use super::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairStatus,
    GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
    GuardianPerformanceSupervisionRejection, GuardianRepairStatus, GuardianUserOutcome,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::OperationPhase;

const MAX_SUMMARY_BYTES: usize = 180;
const MAX_LINE_BYTES: usize = 240;
const MAX_COLLECTION_LINES: usize = 6;
const MAX_DYNAMIC_TOKEN_BYTES: usize = 64;

#[derive(Clone, Copy, Debug)]
pub(crate) struct GuardianCopyRequest<'a> {
    diagnosis_id: Option<DiagnosisId>,
    context: GuardianCopyContext<'a>,
}

#[derive(Clone, Copy, Debug)]
enum GuardianCopyContext<'a> {
    RuntimeRepair {
        status: GuardianRepairStatus,
    },
    ArtifactRepair {
        status: GuardianArtifactRepairStatus,
    },
    InstallFailure {
        decision: GuardianActionKind,
        dynamics: InstallCopyDynamics<'a>,
    },
    PerformanceRejection {
        rejection: GuardianPerformanceSupervisionRejection,
        phase: OperationPhase,
    },
    PersistedStateLoad {
        decision: GuardianActionKind,
    },
}

#[derive(Clone, Copy, Debug)]
enum InstallCopyDynamics<'a> {
    None,
    RuntimeUnavailable {
        component: Option<&'a str>,
        platform: Option<&'a str>,
    },
    Rosetta {
        component: Option<&'a str>,
    },
}

impl<'a> GuardianCopyRequest<'a> {
    pub(crate) fn runtime_repair(
        diagnosis_id: Option<DiagnosisId>,
        status: GuardianRepairStatus,
    ) -> Self {
        Self {
            diagnosis_id,
            context: GuardianCopyContext::RuntimeRepair { status },
        }
    }

    pub(crate) fn artifact_repair(
        diagnosis_id: DiagnosisId,
        status: GuardianArtifactRepairStatus,
    ) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::ArtifactRepair { status },
        }
    }

    pub(crate) fn install_failure(
        diagnosis_id: DiagnosisId,
        decision: GuardianActionKind,
        evidence: &'a [GuardianInstallArtifactFailureEvidence],
    ) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::InstallFailure {
                decision,
                dynamics: install_copy_dynamics(diagnosis_id, evidence),
            },
        }
    }

    pub(crate) fn performance_rejection(
        rejection: GuardianPerformanceSupervisionRejection,
        phase: OperationPhase,
    ) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::PerformanceRejection { rejection, phase },
        }
    }

    pub(crate) fn persisted_state_load(
        diagnosis_id: DiagnosisId,
        decision: GuardianActionKind,
    ) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::PersistedStateLoad { decision },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyContextKey {
    RuntimeRepaired,
    RuntimeBlocked,
    RuntimeFailed,
    RuntimeSuppressed,
    ArtifactRepaired,
    ArtifactBlocked,
    ArtifactFailed,
    ArtifactSuppressed,
    InstallFailure,
    PerformanceUnsafeOwnership,
    PerformanceMissingJournal,
    PerformanceUnsafePublicBoundary,
    PerformanceGuardianBlocked,
    PerformanceFallbackUnavailable,
    PerformanceRollbackUnavailable,
    PersistedStateLoad,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CopyRuleKey {
    diagnosis_id: Option<DiagnosisId>,
    decision: GuardianActionKind,
    context: CopyContextKey,
}

#[derive(Clone, Copy)]
enum CopyPhase {
    Fixed(OperationPhase),
    PerformanceContext,
}

#[derive(Clone, Copy)]
enum CopyLine {
    Static(&'static str),
    RuntimeUnavailableDetail,
    RuntimeRosettaDetail,
}

#[derive(Clone, Copy)]
struct GuardianCopyRule {
    key: CopyRuleKey,
    phase: CopyPhase,
    summary: &'static str,
    details: &'static [CopyLine],
    guidance: &'static [CopyLine],
}

const fn key(
    diagnosis_id: Option<DiagnosisId>,
    decision: GuardianActionKind,
    context: CopyContextKey,
) -> CopyRuleKey {
    CopyRuleKey {
        diagnosis_id,
        decision,
        context,
    }
}

const fn fixed_rule(
    key: CopyRuleKey,
    phase: OperationPhase,
    summary: &'static str,
    details: &'static [CopyLine],
    guidance: &'static [CopyLine],
) -> GuardianCopyRule {
    GuardianCopyRule {
        key,
        phase: CopyPhase::Fixed(phase),
        summary,
        details,
        guidance,
    }
}

const PERFORMANCE_SUMMARY: &str = "performance update was blocked by Guardian safety supervision";

const GUARDIAN_COPY_RULES: &[GuardianCopyRule] = &[
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Repair,
            CopyContextKey::RuntimeRepaired,
        ),
        OperationPhase::Repairing,
        "Guardian repaired launch state before launch.",
        &[CopyLine::Static(
            "Guardian repaired the managed Java runtime before launch.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::RuntimeBlocked,
        ),
        OperationPhase::Repairing,
        "Guardian blocked launch preflight.",
        &[CopyLine::Static(
            "Guardian blocked managed Java runtime repair because it was not safe to apply.",
        )],
        &[CopyLine::Static(
            "Reinstall or repair the affected version/runtime before launching again.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::RuntimeFailed,
        ),
        OperationPhase::Repairing,
        "Guardian blocked launch preflight.",
        &[CopyLine::Static(
            "Guardian could not repair the managed Java runtime automatically.",
        )],
        &[CopyLine::Static(
            "Reinstall or repair the affected version/runtime before launching again.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::RuntimeSuppressed,
        ),
        OperationPhase::Repairing,
        "Guardian blocked launch preflight.",
        &[CopyLine::Static(
            "Guardian suppressed managed Java runtime repair because the same repair failed recently.",
        )],
        &[CopyLine::Static(
            "Reinstall or repair the affected version/runtime before launching again.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Repair,
            CopyContextKey::ArtifactRepaired,
        ),
        OperationPhase::Repairing,
        "Guardian repaired a launcher-managed install artifact.",
        &[CopyLine::Static(
            "Retry the install to continue from the repaired state.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::ArtifactBlocked,
        ),
        OperationPhase::Repairing,
        "Guardian blocked automatic install repair because it was unsafe.",
        &[CopyLine::Static(
            "The launcher did not mutate files that were not proven launcher-managed.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::ArtifactFailed,
        ),
        OperationPhase::Repairing,
        "Guardian could not repair the launcher-managed install artifact.",
        &[CopyLine::Static(
            "Check connection and storage permissions before trying again.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::ArtifactSuppressed,
        ),
        OperationPhase::Repairing,
        "Guardian paused automatic install repair after repeated failure.",
        &[CopyLine::Static(
            "Check connection and storage permissions before trying again.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::DownloadUnavailable),
            GuardianActionKind::Retry,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian classified the install download failure as retryable.",
        &[CopyLine::Static(
            "The install stopped because a provider or network download was unavailable or interrupted.",
        )],
        &[CopyLine::Static(
            "Retry the install after checking connection and storage availability.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::DownloadUnavailable),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian paused install retry after repeated provider failure.",
        &[CopyLine::Static(
            "The install stopped because the same provider or network download failure repeated within the retry cooldown.",
        )],
        &[CopyLine::Static(
            "Wait a few minutes, then retry after checking connection and storage availability.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::InstallArtifactMetadataInvalid),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because provider metadata could not be trusted.",
        &[CopyLine::Static(
            "The install did not continue with invalid provider metadata.",
        )],
        &[CopyLine::Static(
            "Retry later or choose another version source.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::InstallDependencyFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked loader install because the required base install failed.",
        &[CopyLine::Static(
            "The loader install did not continue after the base Minecraft install failed.",
        )],
        &[CopyLine::Static(
            "Retry the base version install, then retry the loader install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeUnavailableForPlatform),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "This Minecraft version needs a Java runtime that is not available for this device.",
        &[CopyLine::RuntimeUnavailableDetail],
        &[CopyLine::Static(
            "This version cannot be installed on this device.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeRosettaRequired),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.",
        &[CopyLine::RuntimeRosettaDetail],
        &[CopyLine::Static(
            "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::FilesystemPermissionDenied),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because Axial could not write launcher-managed files safely.",
        &[CopyLine::Static(
            "The install did not mutate files after the filesystem refused the operation.",
        )],
        &[CopyLine::Static(
            "Check app data permissions and retry the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::TempFileLeftover),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because temporary download state could not be written safely.",
        &[CopyLine::Static(
            "The install did not continue after temporary download state could not be written or cleaned safely.",
        )],
        &[CopyLine::Static(
            "Check app data permissions and disk availability before retrying the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::AtomicPromotionFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because verified download data could not be promoted safely.",
        &[CopyLine::Static(
            "The install did not replace launcher-managed files after atomic promotion failed.",
        )],
        &[CopyLine::Static(
            "Check app data permissions and retry the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ArtifactOwnershipUnsafe),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install to protect user-owned or unknown files.",
        &[CopyLine::Static(
            "The install did not automatically mutate a target whose ownership was unsafe.",
        )],
        &[CopyLine::Static(
            "Move the affected files or choose a launcher-managed library location before retrying.",
        )],
    ),
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceUnsafeOwnership,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceMissingJournal,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceUnsafePublicBoundary,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceGuardianBlocked,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceFallbackUnavailable,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceRollbackUnavailable,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    fixed_rule(
        key(
            Some(DiagnosisId::PersistedStateSchemaInvalid),
            GuardianActionKind::Warn,
            CopyContextKey::PersistedStateLoad,
        ),
        OperationPhase::Startup,
        "Guardian kept Axial running after persisted operation state could not be trusted.",
        &[CopyLine::Static(
            "Some restart-resume records were ignored instead of resuming unsafe work.",
        )],
        &[CopyLine::Static(
            "Retry the affected performance or benchmark operation if it is still needed.",
        )],
    ),
];

pub(crate) fn author_guardian_copy(
    request: GuardianCopyRequest<'_>,
) -> Option<GuardianUserOutcome> {
    let decision = request.context.decision();
    let rule_key = CopyRuleKey {
        diagnosis_id: request.diagnosis_id,
        decision,
        context: request.context.key(),
    };
    let rule = GUARDIAN_COPY_RULES
        .iter()
        .find(|rule| rule.key == rule_key)?;
    let phase = match rule.phase {
        CopyPhase::Fixed(phase) => phase,
        CopyPhase::PerformanceContext => request.context.performance_phase()?,
    };
    let summary = trusted_line(rule.summary, MAX_SUMMARY_BYTES);
    let details = finalize_lines(
        rule.details
            .iter()
            .map(|line| render_line(*line, request.context)),
    );
    let guidance = finalize_lines(
        rule.guidance
            .iter()
            .map(|line| render_line(*line, request.context)),
    );

    Some(GuardianUserOutcome {
        decision,
        phase,
        summary,
        details,
        guidance,
    })
}

impl GuardianCopyContext<'_> {
    fn decision(self) -> GuardianActionKind {
        match self {
            Self::RuntimeRepair {
                status: GuardianRepairStatus::Repaired,
            }
            | Self::ArtifactRepair {
                status: GuardianArtifactRepairStatus::Repaired,
            } => GuardianActionKind::Repair,
            Self::RuntimeRepair { .. }
            | Self::ArtifactRepair { .. }
            | Self::PerformanceRejection { .. } => GuardianActionKind::Block,
            Self::InstallFailure { decision, .. } | Self::PersistedStateLoad { decision } => {
                decision
            }
        }
    }

    fn key(self) -> CopyContextKey {
        match self {
            Self::RuntimeRepair { status } => match status {
                GuardianRepairStatus::Repaired => CopyContextKey::RuntimeRepaired,
                GuardianRepairStatus::Blocked => CopyContextKey::RuntimeBlocked,
                GuardianRepairStatus::Failed => CopyContextKey::RuntimeFailed,
                GuardianRepairStatus::Suppressed => CopyContextKey::RuntimeSuppressed,
            },
            Self::ArtifactRepair { status } => match status {
                GuardianArtifactRepairStatus::Repaired => CopyContextKey::ArtifactRepaired,
                GuardianArtifactRepairStatus::Blocked => CopyContextKey::ArtifactBlocked,
                GuardianArtifactRepairStatus::Failed => CopyContextKey::ArtifactFailed,
                GuardianArtifactRepairStatus::Suppressed => CopyContextKey::ArtifactSuppressed,
            },
            Self::InstallFailure { .. } => CopyContextKey::InstallFailure,
            Self::PerformanceRejection { rejection, .. } => match rejection {
                GuardianPerformanceSupervisionRejection::UnsafeOwnership => {
                    CopyContextKey::PerformanceUnsafeOwnership
                }
                GuardianPerformanceSupervisionRejection::MissingJournal => {
                    CopyContextKey::PerformanceMissingJournal
                }
                GuardianPerformanceSupervisionRejection::UnsafePublicBoundary => {
                    CopyContextKey::PerformanceUnsafePublicBoundary
                }
                GuardianPerformanceSupervisionRejection::GuardianBlocked => {
                    CopyContextKey::PerformanceGuardianBlocked
                }
                GuardianPerformanceSupervisionRejection::FallbackUnavailable => {
                    CopyContextKey::PerformanceFallbackUnavailable
                }
                GuardianPerformanceSupervisionRejection::RollbackUnavailable => {
                    CopyContextKey::PerformanceRollbackUnavailable
                }
            },
            Self::PersistedStateLoad { .. } => CopyContextKey::PersistedStateLoad,
        }
    }

    fn performance_phase(self) -> Option<OperationPhase> {
        match self {
            Self::PerformanceRejection { phase, .. } => Some(phase),
            _ => None,
        }
    }
}

fn render_line(line: CopyLine, context: GuardianCopyContext<'_>) -> String {
    match line {
        CopyLine::Static(value) => trusted_line(value, MAX_LINE_BYTES),
        CopyLine::RuntimeUnavailableDetail => {
            let (component, platform) = match install_dynamics(context) {
                InstallCopyDynamics::RuntimeUnavailable {
                    component,
                    platform,
                } => (
                    sanitize_dynamic_token(component),
                    sanitize_dynamic_token(platform),
                ),
                InstallCopyDynamics::None | InstallCopyDynamics::Rosetta { .. } => (None, None),
            };
            let component = component.unwrap_or_else(|| "the required runtime".to_string());
            let platform = platform.unwrap_or_else(|| "this device".to_string());
            checked_rendered_line(format!(
                "Java runtime component {component} is not available for {platform}."
            ))
        }
        CopyLine::RuntimeRosettaDetail => {
            let component = match install_dynamics(context) {
                InstallCopyDynamics::Rosetta { component } => sanitize_dynamic_token(component),
                InstallCopyDynamics::None | InstallCopyDynamics::RuntimeUnavailable { .. } => None,
            }
            .unwrap_or_else(|| "the required runtime".to_string());
            checked_rendered_line(format!(
                "Java runtime component {component} needs Rosetta 2 on this Mac."
            ))
        }
    }
}

fn install_dynamics(context: GuardianCopyContext<'_>) -> InstallCopyDynamics<'_> {
    match context {
        GuardianCopyContext::InstallFailure { dynamics, .. } => dynamics,
        _ => InstallCopyDynamics::None,
    }
}

fn install_copy_dynamics<'a>(
    diagnosis_id: DiagnosisId,
    evidence: &'a [GuardianInstallArtifactFailureEvidence],
) -> InstallCopyDynamics<'a> {
    let kind = match diagnosis_id {
        DiagnosisId::ManagedRuntimeUnavailableForPlatform => {
            GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform
        }
        DiagnosisId::ManagedRuntimeRosettaRequired => {
            GuardianInstallArtifactFailureKind::RuntimeRosettaRequired
        }
        _ => return InstallCopyDynamics::None,
    };
    let Some(evidence) = evidence.iter().find(|evidence| evidence.kind == kind) else {
        return InstallCopyDynamics::None;
    };
    let field = |key| {
        evidence
            .fields
            .iter()
            .find(|(field_key, _)| field_key == key)
            .map(|(_, value)| value.as_str())
    };
    match kind {
        GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform => {
            InstallCopyDynamics::RuntimeUnavailable {
                component: field("component"),
                platform: field("platform"),
            }
        }
        GuardianInstallArtifactFailureKind::RuntimeRosettaRequired => {
            InstallCopyDynamics::Rosetta {
                component: field("component"),
            }
        }
        _ => InstallCopyDynamics::None,
    }
}

fn sanitize_dynamic_token(value: Option<&str>) -> Option<String> {
    sanitize_evidence_token(
        value?,
        RedactionAudience::UserVisible,
        MAX_DYNAMIC_TOKEN_BYTES,
    )
    .filter(|value| value.len() <= MAX_DYNAMIC_TOKEN_BYTES)
}

fn trusted_line(value: &'static str, max_bytes: usize) -> String {
    assert!(!value.is_empty() && value.len() <= max_bytes);
    value.to_string()
}

fn checked_rendered_line(value: String) -> String {
    assert!(!value.is_empty() && value.len() <= MAX_LINE_BYTES);
    value
}

fn finalize_lines(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = Vec::new();
    for line in lines {
        assert!(!line.is_empty() && line.len() <= MAX_LINE_BYTES);
        if values.iter().any(|existing| existing == &line) {
            continue;
        }
        values.push(line);
        if values.len() == MAX_COLLECTION_LINES {
            break;
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::{
        CopyContextKey, GUARDIAN_COPY_RULES, GuardianCopyRequest, MAX_COLLECTION_LINES,
        MAX_LINE_BYTES, MAX_SUMMARY_BYTES, author_guardian_copy, finalize_lines,
    };
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianInstallArtifactFailureEvidence,
        GuardianInstallArtifactFailureKind, GuardianPerformanceSupervisionRejection,
        GuardianRepairStatus,
    };
    use crate::state::contracts::OperationPhase;

    #[test]
    fn copy_rule_table_is_unique_and_covers_the_five_migrated_families() {
        assert_eq!(GUARDIAN_COPY_RULES.len(), 25);
        for (index, rule) in GUARDIAN_COPY_RULES.iter().enumerate() {
            assert!(
                GUARDIAN_COPY_RULES[index + 1..]
                    .iter()
                    .all(|other| other.key != rule.key),
                "duplicate copy rule at {index}"
            );
        }

        let mut counts = [0_usize; 5];
        for rule in GUARDIAN_COPY_RULES {
            let index = match rule.key.context {
                CopyContextKey::RuntimeRepaired
                | CopyContextKey::RuntimeBlocked
                | CopyContextKey::RuntimeFailed
                | CopyContextKey::RuntimeSuppressed => 0,
                CopyContextKey::ArtifactRepaired
                | CopyContextKey::ArtifactBlocked
                | CopyContextKey::ArtifactFailed
                | CopyContextKey::ArtifactSuppressed => 1,
                CopyContextKey::InstallFailure => 2,
                CopyContextKey::PerformanceUnsafeOwnership
                | CopyContextKey::PerformanceMissingJournal
                | CopyContextKey::PerformanceUnsafePublicBoundary
                | CopyContextKey::PerformanceGuardianBlocked
                | CopyContextKey::PerformanceFallbackUnavailable
                | CopyContextKey::PerformanceRollbackUnavailable => 3,
                CopyContextKey::PersistedStateLoad => 4,
            };
            counts[index] += 1;
            assert!(rule.summary.len() <= MAX_SUMMARY_BYTES);
            assert!(rule.details.len() <= MAX_COLLECTION_LINES);
            assert!(rule.guidance.len() <= MAX_COLLECTION_LINES);
        }
        assert_eq!(counts, [4, 4, 10, 6, 1]);
    }

    #[test]
    fn hostile_dynamic_install_fields_are_redacted_and_byte_bounded() {
        let evidence = [GuardianInstallArtifactFailureEvidence::launcher_managed(
            None,
            "artifact",
            GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
        )
        .with_field("component", "/home/alice/java --accessToken secret")
        .with_field("component", "ignored-second-value")
        .with_field("platform", "界".repeat(64))];
        let outcome = author_guardian_copy(GuardianCopyRequest::install_failure(
            DiagnosisId::ManagedRuntimeUnavailableForPlatform,
            GuardianActionKind::Block,
            &evidence,
        ))
        .expect("runtime unavailable copy rule");
        let encoded = serde_json::to_string(&outcome).expect("outcome JSON");

        assert_eq!(
            outcome.details,
            ["Java runtime component the required runtime is not available for this device."]
        );
        assert!(outcome.summary.len() <= MAX_SUMMARY_BYTES);
        assert!(
            outcome
                .details
                .iter()
                .chain(&outcome.guidance)
                .all(|line| line.len() <= MAX_LINE_BYTES)
        );
        for sensitive in ["/home", "alice", "accessToken", "secret", "ignored-second"] {
            assert!(
                !encoded.contains(sensitive),
                "leaked {sensitive}: {encoded}"
            );
        }
    }

    #[test]
    fn install_dynamics_use_the_first_matching_evidence_and_field() {
        let evidence = [
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "first",
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
            )
            .with_field("component", "jre-first")
            .with_field("component", "ignored-field")
            .with_field("platform", "platform-first"),
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "second",
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
            )
            .with_field("component", "jre-second")
            .with_field("platform", "platform-second"),
        ];

        let outcome = author_guardian_copy(GuardianCopyRequest::install_failure(
            DiagnosisId::ManagedRuntimeUnavailableForPlatform,
            GuardianActionKind::Block,
            &evidence,
        ))
        .expect("runtime unavailable copy rule");

        assert_eq!(
            outcome.details,
            ["Java runtime component jre-first is not available for platform-first."]
        );
    }

    #[test]
    fn unsupported_copy_coordinate_returns_none() {
        assert_eq!(
            author_guardian_copy(GuardianCopyRequest::runtime_repair(
                Some(DiagnosisId::PersistedStateSchemaInvalid),
                GuardianRepairStatus::Repaired,
            )),
            None
        );
    }

    #[test]
    fn performance_rejection_preserves_rolling_back_phase() {
        let outcome = author_guardian_copy(GuardianCopyRequest::performance_rejection(
            GuardianPerformanceSupervisionRejection::RollbackUnavailable,
            OperationPhase::RollingBack,
        ))
        .expect("performance rejection copy rule");

        assert_eq!(outcome.decision, GuardianActionKind::Block);
        assert_eq!(outcome.phase, OperationPhase::RollingBack);
    }

    #[test]
    fn line_finalization_deduplicates_stably_and_caps_collections() {
        let values = finalize_lines([
            "first".to_string(),
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
            "fourth".to_string(),
            "fifth".to_string(),
            "sixth".to_string(),
            "seventh".to_string(),
        ]);
        assert_eq!(
            values,
            ["first", "second", "third", "fourth", "fifth", "sixth"]
        );
    }
}
