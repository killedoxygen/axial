//! Application commands for Performance-owned workflows.
//!
//! Application orchestrates command handling here while `croopor-performance`
//! keeps ownership of rules refresh mechanics and validation.

use super::{ApplicationCommand, PerformancePlanSummaryViewModel, ViewModelTone};
use crate::guardian::{
    GuardianFact, performance_failure_memory_guardian_fact, performance_rules_guardian_facts,
};
use crate::observability::evidence_text_looks_sensitive;
use crate::state::{
    AppState,
    contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStepResult, OwnershipClass, RollbackState,
        StabilizationSystem, TargetDescriptor,
    },
    failure_memory::GuardianFailureMemoryEntry,
    ownership::{CurrentArtifact, classify_current_artifact},
};
use croopor_performance::{
    BundleHealth, CompositionPlan, CompositionTier, PerformanceMode, PerformanceRulesStatus,
    RulesRefreshError,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Serialize)]
pub struct PerformanceRulesStatusResponse {
    #[serde(flatten)]
    pub status: PerformanceRulesStatus,
    pub guardian_facts: Vec<GuardianFact>,
}

pub fn performance_plan_summary_view_model(
    mode: PerformanceMode,
    plan: Option<&CompositionPlan>,
    health: BundleHealth,
    health_tier: Option<CompositionTier>,
    managed_artifact_count: usize,
    warnings: &[String],
) -> PerformancePlanSummaryViewModel {
    if !matches!(mode, PerformanceMode::Managed) || matches!(health, BundleHealth::Disabled) {
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_disabled".to_string(),
            title: "No managed bundle".to_string(),
            detail: "Memory allocation and Java detection are shown below.".to_string(),
            tone: ViewModelTone::Mute,
            health: Some(bundle_health_token(health).to_string()),
            composition_id: plan.map(|plan| plan.composition_id.clone()),
            managed_artifact_count,
            actions: Vec::new(),
        };
    }

    if matches!(health, BundleHealth::Invalid) && plan.is_none() {
        let warning = warnings
            .first()
            .and_then(|warning| public_performance_notice(warning, false));
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_invalid".to_string(),
            title: "Bundle needs attention".to_string(),
            detail: warning
                .unwrap_or_else(|| "Managed performance state needs attention.".to_string()),
            tone: ViewModelTone::Err,
            health: Some(bundle_health_token(health).to_string()),
            composition_id: None,
            managed_artifact_count,
            actions: Vec::new(),
        };
    }

    let Some(plan) = plan else {
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_unavailable".to_string(),
            title: "Bundle status unavailable".to_string(),
            detail: "Plan details are unavailable.".to_string(),
            tone: ViewModelTone::Mute,
            health: Some(bundle_health_token(health).to_string()),
            composition_id: None,
            managed_artifact_count,
            actions: Vec::new(),
        };
    };

    let tier = composition_tier_label(plan.tier);
    let mod_count = plan.mods.len();
    let fallback =
        !plan.fallback_reason.trim().is_empty() || matches!(health, BundleHealth::Fallback);
    let warning = plan
        .fallback_reason
        .trim()
        .split_once('\n')
        .map(|(first, _)| first)
        .unwrap_or_else(|| plan.fallback_reason.trim())
        .to_string();
    let warning = public_performance_notice(
        first_nonempty([
            warning.as_str(),
            warnings.first().map(String::as_str).unwrap_or_default(),
            plan.warnings
                .first()
                .map(String::as_str)
                .unwrap_or_default(),
        ]),
        fallback,
    );
    let launcher_tuning = matches!(plan.tier, CompositionTier::VanillaEnhanced) || mod_count == 0;

    if matches!(health, BundleHealth::Fallback) {
        let fallback_tier = health_tier
            .map(composition_tier_label)
            .unwrap_or_else(|| composition_tier_label(CompositionTier::VanillaEnhanced));
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_fallback".to_string(),
            title: if fallback_tier == "Launcher tuning" {
                "Using launcher tuning".to_string()
            } else {
                "Using fallback bundle".to_string()
            },
            detail: warning.unwrap_or_else(|| {
                format!(
                    "Croopor chose {} because the preferred bundle could not be applied.",
                    fallback_tier.to_ascii_lowercase()
                )
            }),
            tone: health_view_tone(health),
            health: Some(bundle_health_token(health).to_string()),
            composition_id: Some(plan.composition_id.clone()),
            managed_artifact_count,
            actions: Vec::new(),
        };
    }

    if launcher_tuning {
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_launcher_tuning".to_string(),
            title: "Launcher tuning".to_string(),
            detail: warning.unwrap_or_else(|| {
                "Croopor will tune Java and memory for this version; no performance mod bundle is available."
                    .to_string()
            }),
            tone: health_view_tone(health),
            health: Some(bundle_health_token(health).to_string()),
            composition_id: Some(plan.composition_id.clone()),
            managed_artifact_count,
            actions: Vec::new(),
        };
    }

    let health_text = format!("bundle {}", health_label(health));
    PerformancePlanSummaryViewModel {
        state_id: format!("performance_summary_{}", bundle_health_token(health)),
        title: tier,
        detail: warning.unwrap_or_else(|| {
            format!(
                "{} performance mod{} selected; {}.",
                mod_count,
                if mod_count == 1 { "" } else { "s" },
                health_text
            )
        }),
        tone: health_view_tone(health),
        health: Some(bundle_health_token(health).to_string()),
        composition_id: Some(plan.composition_id.clone()),
        managed_artifact_count,
        actions: Vec::new(),
    }
}

fn composition_tier_label(tier: CompositionTier) -> String {
    match tier {
        CompositionTier::Extended => "Full mod bundle",
        CompositionTier::Core => "Core mod bundle",
        CompositionTier::VanillaEnhanced => "Launcher tuning",
    }
    .to_string()
}

fn health_label(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "healthy",
        BundleHealth::Degraded => "degraded",
        BundleHealth::Fallback => "fallback",
        BundleHealth::Invalid => "needs attention",
        BundleHealth::Disabled => "not installed",
    }
}

fn health_view_tone(health: BundleHealth) -> ViewModelTone {
    match health {
        BundleHealth::Healthy => ViewModelTone::Ok,
        BundleHealth::Degraded | BundleHealth::Fallback | BundleHealth::Disabled => {
            ViewModelTone::Warn
        }
        BundleHealth::Invalid => ViewModelTone::Err,
    }
}

fn bundle_health_token(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "healthy",
        BundleHealth::Degraded => "degraded",
        BundleHealth::Fallback => "fallback",
        BundleHealth::Disabled => "disabled",
        BundleHealth::Invalid => "invalid",
    }
}

fn public_performance_notice(raw: &str, fallback: bool) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || evidence_text_looks_sensitive(raw) {
        return None;
    }
    let notice = if raw.contains("temporarily unavailable")
        || raw.contains("not compatible with this instance")
    {
        raw.to_string()
    } else if raw.contains("skipped by emergency disable") {
        if fallback {
            "A performance bundle is temporarily unavailable, so Croopor chose the safest available option."
                .to_string()
        } else {
            "One performance mod is temporarily unavailable, so Croopor left it out.".to_string()
        }
    } else if raw.contains("not enough compatible performance mods") {
        "A faster performance bundle is not compatible with this instance, so Croopor chose a safer option."
            .to_string()
    } else if raw.contains("no NVIDIA Turing+ GPU detected") {
        "Nvidium was left out because this device does not have a supported NVIDIA GPU.".to_string()
    } else {
        raw.replace("managed mod", "installed mod")
    };
    Some(notice)
}

fn first_nonempty<'a>(values: impl IntoIterator<Item = &'a str>) -> &'a str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

pub fn performance_rules_status(state: &AppState) -> PerformanceRulesStatusResponse {
    performance_rules_status_response(state, state.performance().rules_status())
}

pub async fn refresh_performance_rules(
    state: &AppState,
) -> Result<PerformanceRulesStatusResponse, RefreshPerformanceRulesError> {
    handle_refresh_performance_rules(
        state,
        ApplicationCommand::new(CommandKind::RefreshPerformanceRules),
    )
    .await
}

async fn handle_refresh_performance_rules(
    state: &AppState,
    command: ApplicationCommand,
) -> Result<PerformanceRulesStatusResponse, RefreshPerformanceRulesError> {
    if command.kind != CommandKind::RefreshPerformanceRules {
        return Err(RefreshPerformanceRulesError::UnsupportedCommand {
            actual: command.kind,
        });
    }

    let operation_id = new_refresh_rules_operation_id();
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RefreshPerformanceRules,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry
        .planned_steps
        .push(refresh_rules_step(OperationStepResult::Planned));
    entry.targets = refresh_rules_targets();
    state.journals().create(entry);

    match state.performance().refresh_rules().await {
        Ok(status) => {
            state.journals().record_success(
                &operation_id,
                refresh_rules_step(OperationStepResult::Completed),
                OperationOutcome::Succeeded,
            );
            Ok(performance_rules_status_response(state, status))
        }
        Err(error) => {
            state.journals().record_failure(
                &operation_id,
                refresh_rules_step(OperationStepResult::Failed),
                "refresh_remote_rules",
                OperationOutcome::Failed,
            );
            Err(RefreshPerformanceRulesError::from(error))
        }
    }
}

fn performance_rules_status_response(
    state: &AppState,
    status: PerformanceRulesStatus,
) -> PerformanceRulesStatusResponse {
    let failure_memory = state.failure_memory().list();
    performance_rules_status_response_from_memory(status, failure_memory.iter())
}

fn performance_rules_status_response_from_memory<'a>(
    status: PerformanceRulesStatus,
    failure_memory: impl IntoIterator<Item = &'a GuardianFailureMemoryEntry>,
) -> PerformanceRulesStatusResponse {
    let mut guardian_facts = performance_rules_guardian_facts(&status, OperationPhase::Validating);
    guardian_facts.extend(performance_rules_failure_memory_facts(failure_memory));
    PerformanceRulesStatusResponse {
        status,
        guardian_facts,
    }
}

fn performance_rules_failure_memory_facts<'a>(
    failure_memory: impl IntoIterator<Item = &'a GuardianFailureMemoryEntry>,
) -> Vec<GuardianFact> {
    failure_memory
        .into_iter()
        .filter_map(|entry| {
            performance_failure_memory_guardian_fact(entry, OperationPhase::Validating)
        })
        .collect()
}

fn new_refresh_rules_operation_id() -> OperationId {
    OperationId::new(format!(
        "performance-rules-refresh-{}",
        uuid::Uuid::new_v4()
    ))
}

fn refresh_rules_step(result: OperationStepResult) -> OperationJournalStep {
    let mut step = OperationJournalStep::new("refresh_remote_rules", OperationPhase::Running);
    step.result = result;
    if result == OperationStepResult::Completed {
        step.changed_target = Some(
            classify_current_artifact(
                CurrentArtifact::PerformanceRulesCache,
                "performance_rules_cache",
            )
            .target,
        );
    }
    step
}

fn refresh_rules_targets() -> Vec<TargetDescriptor> {
    vec![
        classify_current_artifact(
            CurrentArtifact::ExternalPerformanceRules,
            "performance_rules_remote_source",
        )
        .target,
        classify_current_artifact(
            CurrentArtifact::PerformanceRulesCache,
            "performance_rules_cache",
        )
        .target,
    ]
}

#[derive(Debug, Error)]
pub enum RefreshPerformanceRulesError {
    #[error("performance remote rules url is not configured")]
    Unconfigured,
    #[error("unsupported application command for performance rules refresh: {actual:?}")]
    UnsupportedCommand { actual: CommandKind },
    #[error(transparent)]
    Refresh(RulesRefreshError),
}

impl From<RulesRefreshError> for RefreshPerformanceRulesError {
    fn from(error: RulesRefreshError) -> Self {
        match error {
            RulesRefreshError::Unconfigured => Self::Unconfigured,
            error => Self::Refresh(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ViewModelTone, performance_plan_summary_view_model,
        performance_rules_status_response_from_memory,
    };
    use crate::guardian::{DiagnosisId, GuardianDomain, GuardianMode};
    use crate::state::{
        contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind},
        failure_memory::GuardianFailureMemoryEntry,
    };
    use croopor_performance::{
        BundleHealth, CompositionPlan, CompositionTier, ModCondition, PerformanceMode, RuleChannel,
        RuleSource, RulesCacheState, RulesCacheStatus, RulesValidation, builtin_manifest,
        rules_status, rules_status_for,
        types::{ManagedMod, VersionFamily},
    };

    #[test]
    fn performance_summary_view_model_matches_managed_healthy_meaning() {
        let plan = test_plan(
            "core",
            CompositionTier::Core,
            vec![test_mod("sodium", "Sodium")],
            "",
            Vec::new(),
        );

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            Some(&plan),
            BundleHealth::Healthy,
            Some(CompositionTier::Core),
            1,
            &[],
        );

        assert_eq!(summary.state_id, "performance_summary_healthy");
        assert_eq!(summary.title, "Core mod bundle");
        assert_eq!(
            summary.detail,
            "1 performance mod selected; bundle healthy."
        );
        assert_eq!(summary.tone, ViewModelTone::Ok);
        assert_eq!(summary.managed_artifact_count, 1);
    }

    #[test]
    fn performance_summary_view_model_normalizes_fallback_warning() {
        let plan = test_plan(
            "fallback",
            CompositionTier::VanillaEnhanced,
            Vec::new(),
            "not enough compatible performance mods for preferred composition",
            Vec::new(),
        );

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            Some(&plan),
            BundleHealth::Fallback,
            Some(CompositionTier::VanillaEnhanced),
            0,
            &[],
        );

        assert_eq!(summary.state_id, "performance_summary_fallback");
        assert_eq!(summary.title, "Using launcher tuning");
        assert_eq!(
            summary.detail,
            "A faster performance bundle is not compatible with this instance, so Croopor chose a safer option."
        );
        assert_eq!(summary.tone, ViewModelTone::Warn);
    }

    #[test]
    fn performance_summary_view_model_drops_sensitive_warning_material() {
        let warnings = vec![
            r"managed.jar missing from C:\Users\Alice\.minecraft\mods with -Xmx8192M".to_string(),
        ];

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            None,
            BundleHealth::Invalid,
            None,
            0,
            &warnings,
        );

        assert_eq!(summary.title, "Bundle needs attention");
        assert_eq!(summary.detail, "Managed performance state needs attention.");
        assert!(!summary.detail.contains("Alice"));
        assert!(!summary.detail.contains("-Xmx"));
    }

    #[test]
    fn performance_rules_status_response_authors_invalid_rules_guardian_fact() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let mut cache = RulesCacheStatus::unavailable();
        cache.state = RulesCacheState::Invalid;
        cache.warning = Some("missing signature".to_string());
        let status = rules_status_for(
            &manifest,
            RuleSource::Remote,
            RuleChannel::Remote,
            cache,
            true,
            None,
            RulesValidation::Invalid,
        );

        let response = performance_rules_status_response_from_memory(status, std::iter::empty());

        assert_eq!(response.status.validation, RulesValidation::Invalid);
        let fact = response
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "performance_rules_invalid")
            .expect("invalid rules Guardian fact");
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("performance_rules_remote_source")
        );
    }

    #[test]
    fn performance_rules_status_response_includes_repeated_failure_memory_fact() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let status = rules_status(&manifest);
        let mut entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::new("performance_fallback_selected"),
            GuardianDomain::Performance,
            TargetDescriptor::new(
                StabilizationSystem::Performance,
                TargetKind::PerformanceComposition,
                "family-f-fabric-core",
                OwnershipClass::CompositionManaged,
            ),
            GuardianMode::Managed,
            Some("intent"),
            "2026-06-15T12:00:00Z",
        );
        entry.occurrence_count = 3;

        let response =
            performance_rules_status_response_from_memory(status, std::iter::once(&entry));

        let fact = response
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "performance_repeated_failure_memory")
            .expect("repeated failure memory Guardian fact");
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("family-f-fabric-core")
        );
        assert!(
            fact.fields
                .iter()
                .any(|field| field.key == "occurrence_count" && field.value == "3")
        );
    }

    fn test_plan(
        composition_id: &str,
        tier: CompositionTier,
        mods: Vec<ManagedMod>,
        fallback_reason: &str,
        warnings: Vec<String>,
    ) -> CompositionPlan {
        CompositionPlan {
            composition_id: composition_id.to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier,
            mods,
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings,
            fallback_reason: fallback_reason.to_string(),
        }
    }

    fn test_mod(project_id: &str, name: &str) -> ManagedMod {
        ManagedMod {
            artifact_id: project_id.to_string(),
            project_id: project_id.to_string(),
            slug: project_id.to_string(),
            name: name.to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }
    }
}
