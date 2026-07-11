use super::{
    DiagnosisId, FactReliability, GuardianConfidence, GuardianDomain, GuardianFact, GuardianFactId,
    GuardianMode, GuardianSeverity,
    launch_decision::is_guardian_launch_crash_class,
    launch_recovery::{
        GuardianLaunchRecoveryCurrentIntent, launch_recovery_intent_fingerprint_matches,
    },
};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    sanitize_target_id,
};
use crate::state::failure_memory::{
    DEFAULT_FAILURE_MEMORY_LIMIT, FailureMemoryActionOutcome, FailureMemoryStoreError,
    GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
};
use axial_launcher::LaunchFailureClass;
use chrono::{DateTime, Duration, FixedOffset, SecondsFormat, Utc};

const RECENT_FAILURE_WINDOW_HOURS: i64 = 24;
pub(crate) const RECENT_STARTUP_FAILURE_FACT_ID: GuardianFactId =
    GuardianFactId::RecentStartupFailure;
pub(crate) const RECENT_REPAIR_FAILED_FACT_ID: GuardianFactId = GuardianFactId::RecentRepairFailed;
pub(crate) const REPAIR_SUPPRESSED_UNTIL_FACT_ID: GuardianFactId =
    GuardianFactId::RepairSuppressedUntil;

#[derive(Clone, Copy, Debug)]
pub struct GuardianLaunchFailureMemoryIntakeRequest<'a> {
    pub entries: &'a [GuardianFailureMemoryEntry],
    pub instance_id: &'a str,
    pub mode: GuardianMode,
    pub current_at: &'a str,
    pub current_intent: GuardianLaunchRecoveryCurrentIntent<'a>,
    pub current_memory_mb: i32,
    pub suggested_memory_mb: Option<i32>,
}

pub fn launch_failure_memory_guardian_facts(
    request: GuardianLaunchFailureMemoryIntakeRequest<'_>,
) -> Vec<GuardianFact> {
    if request.entries.len() > DEFAULT_FAILURE_MEMORY_LIMIT {
        return Vec::new();
    }
    let Some(context) = IntakeContext::new(&request) else {
        return Vec::new();
    };

    let mut facts = Vec::with_capacity(3);
    if let Some(entry) = newest_entry(&request, &context, accepted_startup_failure_entry) {
        facts.push(recent_startup_failure_fact(&request, &context, entry));
    }
    if let Some(entry) = newest_entry(&request, &context, accepted_failed_repair_entry) {
        facts.push(recent_repair_failed_fact(&context, entry));
    }
    if let Some(entry) = newest_entry(&request, &context, active_repair_suppression_entry) {
        facts.push(repair_suppressed_until_fact(&context, entry));
    }
    facts
}

struct IntakeContext {
    current_at: DateTime<Utc>,
    instance_id: String,
}

impl IntakeContext {
    fn new(request: &GuardianLaunchFailureMemoryIntakeRequest<'_>) -> Option<Self> {
        let current_at = DateTime::parse_from_rfc3339(request.current_at).ok()?;
        if current_at.offset().local_minus_utc() != 0 {
            return None;
        }
        let current_at = current_at.with_timezone(&Utc);
        let raw_instance_id = request.instance_id.trim();
        let instance_id = sanitize_target_id(raw_instance_id, "instance");
        if raw_instance_id.is_empty() || instance_id != raw_instance_id {
            return None;
        }
        Some(Self {
            current_at,
            instance_id,
        })
    }
}

fn newest_entry<'a>(
    request: &GuardianLaunchFailureMemoryIntakeRequest<'a>,
    context: &IntakeContext,
    accepted: fn(
        &GuardianFailureMemoryEntry,
        &GuardianLaunchFailureMemoryIntakeRequest<'_>,
        &IntakeContext,
    ) -> bool,
) -> Option<&'a GuardianFailureMemoryEntry> {
    request
        .entries
        .iter()
        .filter(|entry| current_instance_entry(entry, request, context))
        .filter(|entry| accepted(entry, request, context))
        .filter_map(|entry| {
            let last_observed_at = parsed_timestamp(&entry.last_observed_at)?;
            recent_at(last_observed_at, context.current_at).then_some((
                last_observed_at,
                entry.key.as_str(),
                entry,
            ))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)))
        .map(|(_, _, entry)| entry)
}

fn current_instance_entry(
    entry: &GuardianFailureMemoryEntry,
    request: &GuardianLaunchFailureMemoryIntakeRequest<'_>,
    context: &IntakeContext,
) -> bool {
    entry.validate().is_ok()
        && entry.mode == request.mode
        && entry.target.system == StabilizationSystem::Guardian
        && entry.target.kind == TargetKind::Instance
        && entry.target.id == context.instance_id
}

fn recent_at(observed_at: DateTime<FixedOffset>, current_at: DateTime<Utc>) -> bool {
    let observed_at = observed_at.with_timezone(&Utc);
    observed_at <= current_at
        && current_at - observed_at <= Duration::hours(RECENT_FAILURE_WINDOW_HOURS)
}

fn accepted_startup_failure_entry(
    entry: &GuardianFailureMemoryEntry,
    _request: &GuardianLaunchFailureMemoryIntakeRequest<'_>,
    _context: &IntakeContext,
) -> bool {
    entry.domain == GuardianDomain::Startup
        && entry.ownership == OwnershipClass::UserOwned
        && entry.last_action_kind.is_none()
        && entry.last_action_outcome.is_none()
        && LaunchFailureClass::from_name(entry.diagnosis_id.as_str())
            .is_some_and(is_guardian_launch_crash_class)
}

fn accepted_failed_repair_entry(
    entry: &GuardianFailureMemoryEntry,
    request: &GuardianLaunchFailureMemoryIntakeRequest<'_>,
    context: &IntakeContext,
) -> bool {
    accepted_repair_entry(entry, request, context)
        && entry.last_action_outcome == Some(FailureMemoryActionOutcome::Failed)
}

fn active_repair_suppression_entry(
    entry: &GuardianFailureMemoryEntry,
    request: &GuardianLaunchFailureMemoryIntakeRequest<'_>,
    context: &IntakeContext,
) -> bool {
    accepted_repair_entry(entry, request, context)
        && matches!(
            entry.last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed | FailureMemoryActionOutcome::Suppressed)
        )
        && entry
            .suppression_until
            .as_deref()
            .and_then(parsed_timestamp)
            .is_some_and(|until| until.with_timezone(&Utc) > context.current_at)
}

fn accepted_repair_entry(
    entry: &GuardianFailureMemoryEntry,
    request: &GuardianLaunchFailureMemoryIntakeRequest<'_>,
    _context: &IntakeContext,
) -> bool {
    entry.domain == GuardianDomain::Launch
        && entry.ownership == OwnershipClass::LauncherManaged
        && entry.repair_attempt_count > 0
        && launch_recovery_intent_fingerprint_matches(
            &entry.diagnosis_id,
            entry.last_action_kind,
            entry.user_intent_hash.as_deref(),
            request.current_intent,
        )
}

fn recent_startup_failure_fact(
    request: &GuardianLaunchFailureMemoryIntakeRequest<'_>,
    context: &IntakeContext,
    entry: &GuardianFailureMemoryEntry,
) -> GuardianFact {
    let failure_class = LaunchFailureClass::from_name(entry.diagnosis_id.as_str())
        .expect("accepted launch crash class must remain parseable");
    let mut fields = vec![
        public_field("failure_class", failure_class.as_str()),
        public_field("occurrences", entry.occurrence_count.to_string()),
    ];
    let latest_today = parsed_timestamp(&entry.last_observed_at).is_some_and(|value| {
        value.with_timezone(&Utc).date_naive() == context.current_at.date_naive()
    });
    fields.push(public_field(
        "latest_observed_today",
        latest_today.to_string(),
    ));
    let all_observations_today = parsed_timestamp(&entry.first_observed_at).is_some_and(|value| {
        value.with_timezone(&Utc).date_naive() == context.current_at.date_naive()
    }) && latest_today;
    if all_observations_today {
        fields.push(public_field(
            "occurrences_today",
            entry.occurrence_count.to_string(),
        ));
    }
    if failure_class == LaunchFailureClass::OutOfMemory && request.current_memory_mb > 0 {
        fields.push(public_field(
            "current_memory_mb",
            request.current_memory_mb.to_string(),
        ));
        if let Some(suggested_memory_mb) = request
            .suggested_memory_mb
            .filter(|suggested| *suggested > request.current_memory_mb)
        {
            fields.push(public_field(
                "suggested_memory_mb",
                suggested_memory_mb.to_string(),
            ));
        }
    }
    memory_fact(
        RECENT_STARTUP_FAILURE_FACT_ID,
        GuardianDomain::Startup,
        OwnershipClass::UserOwned,
        &context.instance_id,
        fields,
    )
}

fn recent_repair_failed_fact(
    context: &IntakeContext,
    entry: &GuardianFailureMemoryEntry,
) -> GuardianFact {
    memory_fact(
        RECENT_REPAIR_FAILED_FACT_ID,
        GuardianDomain::Launch,
        OwnershipClass::LauncherManaged,
        &context.instance_id,
        vec![public_field("diagnosis", entry.diagnosis_id.as_str())],
    )
}

fn repair_suppressed_until_fact(
    context: &IntakeContext,
    entry: &GuardianFailureMemoryEntry,
) -> GuardianFact {
    let suppression_until = entry
        .suppression_until
        .as_deref()
        .and_then(parsed_timestamp)
        .expect("accepted suppression must have a valid timestamp")
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    memory_fact(
        REPAIR_SUPPRESSED_UNTIL_FACT_ID,
        GuardianDomain::Launch,
        OwnershipClass::LauncherManaged,
        &context.instance_id,
        vec![
            public_field("diagnosis", entry.diagnosis_id.as_str()),
            public_field("suppression_until", suppression_until),
        ],
    )
}

fn memory_fact(
    id: GuardianFactId,
    domain: GuardianDomain,
    ownership: OwnershipClass,
    instance_id: &str,
    fields: Vec<EvidenceField>,
) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase: OperationPhase::Validating,
        reliability: FactReliability::DirectStructured,
        severity: Some(GuardianSeverity::Warning),
        confidence: Some(GuardianConfidence::Confirmed),
        ownership,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Instance,
            instance_id,
            ownership,
        )),
        fields,
    }
}

fn public_field(key: impl Into<String>, value: impl Into<String>) -> EvidenceField {
    EvidenceField::new(key, value, EvidenceSensitivity::Public)
}

fn parsed_timestamp(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value.trim()).ok()
}

pub fn record_launch_failure_observation(
    failure_memory: &GuardianFailureMemoryStore,
    instance_id: &str,
    mode: GuardianMode,
    failure_class: LaunchFailureClass,
    observed_at: &str,
) -> Result<(), FailureMemoryStoreError> {
    if !is_guardian_launch_crash_class(failure_class) {
        return Ok(());
    }
    failure_memory.record(GuardianFailureMemoryEntry::observed(
        DiagnosisId::new(failure_class.as_str()),
        GuardianDomain::Startup,
        TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Instance,
            instance_id,
            OwnershipClass::UserOwned,
        ),
        mode,
        None,
        observed_at,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{
        GuardianActionKind, GuardianLaunchRecoveryKind, launch_recovery_user_intent_fingerprint,
    };
    use crate::observability::RedactionAudience;

    #[test]
    fn repeated_launch_failure_observations_merge_by_class_and_instance() {
        let store = GuardianFailureMemoryStore::new();
        record_launch_failure_observation(
            &store,
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::ModAttributedCrash,
            "2026-01-01T00:00:00Z",
        )
        .expect("record first mod-attributed crash");
        record_launch_failure_observation(
            &store,
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::ModAttributedCrash,
            "2026-01-01T00:05:00Z",
        )
        .expect("record repeated mod-attributed crash");

        let entries = store.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].diagnosis_id.as_str(), "mod_attributed_crash");
        assert_eq!(entries[0].target.kind, TargetKind::Instance);
        assert_eq!(entries[0].target.id, "instance-a");
        assert_eq!(entries[0].occurrence_count, 2);
        assert_eq!(entries[0].first_observed_at, "2026-01-01T00:00:00Z");
        assert_eq!(entries[0].last_observed_at, "2026-01-01T00:05:00Z");
        assert_eq!(entries[0].last_action_kind, None);
        assert_eq!(entries[0].last_action_outcome, None);
    }

    #[test]
    fn records_each_accepted_class_and_ignores_generic_failures() {
        let store = GuardianFailureMemoryStore::new();
        let accepted = [
            LaunchFailureClass::OutOfMemory,
            LaunchFailureClass::GraphicsDriverCrash,
            LaunchFailureClass::MissingDependency,
            LaunchFailureClass::ModTransformationFailure,
            LaunchFailureClass::ModAttributedCrash,
        ];
        for failure_class in accepted {
            record_launch_failure_observation(
                &store,
                "instance-a",
                GuardianMode::Managed,
                failure_class,
                "2026-01-01T00:00:00Z",
            )
            .expect("record accepted launch failure");
        }
        record_launch_failure_observation(
            &store,
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::Unknown,
            "2026-01-01T00:00:00Z",
        )
        .expect("ignore generic launch failure");

        let entries = store.list();
        assert_eq!(entries.len(), accepted.len());
        for failure_class in accepted {
            assert!(entries.iter().any(|entry| {
                entry.diagnosis_id.as_str() == failure_class.as_str()
                    && entry.target.id == "instance-a"
                    && entry.occurrence_count == 1
            }));
        }
    }

    #[test]
    fn intake_emits_at_most_three_closed_facts() {
        let mut startup = startup_entry(
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::OutOfMemory,
            "2026-07-11T09:00:00Z",
        );
        startup.occurrence_count = 2;
        let repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_preset_recovery",
            GuardianActionKind::Downgrade,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T10:00:00Z",
            Some("2026-07-11T11:00:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::DowngradePreset),
        );
        let entries = vec![startup, repair];

        let facts = launch_failure_memory_guardian_facts(intake_request(&entries));

        assert_eq!(facts.len(), 3);
        assert_eq!(facts[0].id, RECENT_STARTUP_FAILURE_FACT_ID);
        assert_eq!(facts[1].id, RECENT_REPAIR_FAILED_FACT_ID);
        assert_eq!(facts[2].id, REPAIR_SUPPRESSED_UNTIL_FACT_ID);
        assert_eq!(field(&facts[0], "occurrences_today"), Some("2"));
        assert_eq!(field(&facts[1], "diagnosis"), Some("jvm_preset_recovery"));
        assert_eq!(
            field(&facts[2], "suppression_until"),
            Some("2026-07-11T11:00:00Z")
        );
    }

    #[test]
    fn intake_filters_stale_malformed_unrelated_mode_instance_and_class_entries() {
        let mut malformed = startup_entry(
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::OutOfMemory,
            "2026-07-11T09:00:00Z",
        );
        malformed.last_observed_at = "not-a-time".to_string();
        let unknown = GuardianFailureMemoryEntry::observed(
            DiagnosisId::new("unknown_future_class"),
            GuardianDomain::Startup,
            instance_target("instance-a", OwnershipClass::UserOwned),
            GuardianMode::Managed,
            None,
            "2026-07-11T09:15:00Z",
        );
        let entries = vec![
            startup_entry(
                "instance-a",
                GuardianMode::Managed,
                LaunchFailureClass::OutOfMemory,
                "2026-07-09T09:00:00Z",
            ),
            malformed,
            unknown,
            startup_entry(
                "instance-b",
                GuardianMode::Managed,
                LaunchFailureClass::OutOfMemory,
                "2026-07-11T09:30:00Z",
            ),
            startup_entry(
                "instance-a",
                GuardianMode::Custom,
                LaunchFailureClass::OutOfMemory,
                "2026-07-11T09:45:00Z",
            ),
            startup_entry(
                "instance-a",
                GuardianMode::Managed,
                LaunchFailureClass::MissingDependency,
                "2026-07-11T10:05:00Z",
            ),
            startup_entry(
                "instance-a",
                GuardianMode::Managed,
                LaunchFailureClass::ModAttributedCrash,
                "2026-07-11T09:50:00Z",
            ),
        ];

        let facts = launch_failure_memory_guardian_facts(intake_request(&entries));

        assert_eq!(facts.len(), 1);
        assert_eq!(
            field(&facts[0], "failure_class"),
            Some("mod_attributed_crash")
        );
    }

    #[test]
    fn intake_requires_exact_repair_intent_and_active_suppression() {
        let active = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_arg_unsupported",
            GuardianActionKind::Strip,
            FailureMemoryActionOutcome::Suppressed,
            "2026-07-11T09:30:00Z",
            Some("2026-07-11T11:00:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::StripRawJvmArgs),
        );
        let expired = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_arg_unsupported",
            GuardianActionKind::Strip,
            FailureMemoryActionOutcome::Suppressed,
            "2026-07-11T09:40:00Z",
            Some("2026-07-11T09:59:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::DisableCustomGc),
        );

        let active_facts =
            launch_failure_memory_guardian_facts(intake_request(std::slice::from_ref(&active)));
        let wrong_intent_facts =
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                current_intent: GuardianLaunchRecoveryCurrentIntent {
                    explicit_jvm_args: &["-XX:+UseG1GC".to_string()],
                    ..current_intent()
                },
                ..intake_request(std::slice::from_ref(&active))
            });
        let expired_facts =
            launch_failure_memory_guardian_facts(intake_request(std::slice::from_ref(&expired)));

        assert_eq!(active_facts.len(), 1);
        assert_eq!(active_facts[0].id, REPAIR_SUPPRESSED_UNTIL_FACT_ID);
        assert!(wrong_intent_facts.is_empty());
        assert!(expired_facts.is_empty());
    }

    #[test]
    fn intake_resolves_ambiguous_strip_action_through_exact_kind_fingerprints() {
        let raw_args_repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_arg_unsupported",
            GuardianActionKind::Strip,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T09:00:00Z",
            None,
            &intent_fingerprint(GuardianLaunchRecoveryKind::StripRawJvmArgs),
        );
        let custom_gc_repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_arg_unsupported",
            GuardianActionKind::Strip,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T09:00:00Z",
            None,
            &intent_fingerprint(GuardianLaunchRecoveryKind::DisableCustomGc),
        );

        let raw_match = launch_failure_memory_guardian_facts(intake_request(std::slice::from_ref(
            &raw_args_repair,
        )));
        let gc_match = launch_failure_memory_guardian_facts(intake_request(std::slice::from_ref(
            &custom_gc_repair,
        )));
        let raw_mismatch =
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                current_intent: GuardianLaunchRecoveryCurrentIntent {
                    explicit_jvm_args: &["-XX:+UseG1GC".to_string()],
                    ..current_intent()
                },
                ..intake_request(std::slice::from_ref(&raw_args_repair))
            });
        let gc_mismatch =
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                current_intent: GuardianLaunchRecoveryCurrentIntent {
                    requested_preset: "performance",
                    ..current_intent()
                },
                ..intake_request(std::slice::from_ref(&custom_gc_repair))
            });

        assert_eq!(raw_match[0].id, RECENT_REPAIR_FAILED_FACT_ID);
        assert_eq!(gc_match[0].id, RECENT_REPAIR_FAILED_FACT_ID);
        assert!(raw_mismatch.is_empty());
        assert!(gc_mismatch.is_empty());
    }

    #[test]
    fn intake_rejects_changed_or_unfingerprintable_repair_intent() {
        let java_repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "java_runtime_recovery",
            GuardianActionKind::Fallback,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T09:00:00Z",
            Some("2026-07-11T11:00:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::SwitchManagedRuntime),
        );
        let preset_repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_preset_recovery",
            GuardianActionKind::Downgrade,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T09:00:00Z",
            Some("2026-07-11T11:00:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::DowngradePreset),
        );
        let args_repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_arg_unsupported",
            GuardianActionKind::Strip,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T09:00:00Z",
            Some("2026-07-11T11:00:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::StripRawJvmArgs),
        );
        let changed_args = vec!["-XX:+UseG1GC".to_string()];
        let changed_cases = [
            (
                &java_repair,
                GuardianLaunchRecoveryCurrentIntent {
                    requested_java: "/opt/other-java/bin/java",
                    ..current_intent()
                },
            ),
            (
                &args_repair,
                GuardianLaunchRecoveryCurrentIntent {
                    explicit_jvm_args: &changed_args,
                    ..current_intent()
                },
            ),
            (
                &preset_repair,
                GuardianLaunchRecoveryCurrentIntent {
                    requested_preset: "performance",
                    ..current_intent()
                },
            ),
            (
                &java_repair,
                GuardianLaunchRecoveryCurrentIntent {
                    target_version_id: "1.20.1",
                    ..current_intent()
                },
            ),
            (
                &preset_repair,
                GuardianLaunchRecoveryCurrentIntent {
                    target_version_id: "/invalid/version",
                    ..current_intent()
                },
            ),
        ];
        for (entry, current_intent) in changed_cases {
            let facts =
                launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                    current_intent,
                    ..intake_request(std::slice::from_ref(entry))
                });
            assert!(facts.is_empty());
        }
    }

    #[test]
    fn intake_selects_newest_deterministically_and_keeps_occurrence_copy_truthful() {
        let mut older = startup_entry(
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::OutOfMemory,
            "2026-07-10T12:00:00Z",
        );
        older.last_observed_at = "2026-07-11T09:00:00Z".to_string();
        older.occurrence_count = 4;
        let newer = startup_entry(
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::ModAttributedCrash,
            "2026-07-11T09:30:00Z",
        );
        let forward = vec![older.clone(), newer.clone()];
        let reverse = vec![newer, older];

        for entries in [&forward, &reverse] {
            let facts = launch_failure_memory_guardian_facts(intake_request(entries));
            assert_eq!(facts.len(), 1);
            assert_eq!(
                field(&facts[0], "failure_class"),
                Some("mod_attributed_crash")
            );
            assert_eq!(field(&facts[0], "latest_observed_today"), Some("true"));
            assert_eq!(field(&facts[0], "occurrences_today"), Some("1"));
        }

        let older_only = vec![forward[0].clone()];
        let facts = launch_failure_memory_guardian_facts(intake_request(&older_only));
        assert_eq!(field(&facts[0], "occurrences"), Some("4"));
        assert_eq!(field(&facts[0], "latest_observed_today"), Some("true"));
        assert_eq!(field(&facts[0], "occurrences_today"), None);
    }

    #[test]
    fn intake_carries_only_validated_low_and_normal_memory_suggestions() {
        let entry = startup_entry(
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::OutOfMemory,
            "2026-07-11T09:00:00Z",
        );
        let entries = vec![entry];
        for (current_memory_mb, suggested_memory_mb, current, suggested) in
            [(1024, 2048, "1024", "2048"), (4096, 6144, "4096", "6144")]
        {
            let facts =
                launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                    current_memory_mb,
                    suggested_memory_mb: Some(suggested_memory_mb),
                    ..intake_request(&entries)
                });
            assert_eq!(field(&facts[0], "current_memory_mb"), Some(current));
            assert_eq!(field(&facts[0], "suggested_memory_mb"), Some(suggested));
        }

        let facts =
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                current_memory_mb: 4096,
                suggested_memory_mb: Some(2048),
                ..intake_request(&entries)
            });
        assert_eq!(field(&facts[0], "suggested_memory_mb"), None);
    }

    #[test]
    fn intake_facts_round_trip_through_public_redaction_only() {
        let repair = repair_entry(
            "instance-a",
            GuardianMode::Managed,
            "jvm_preset_recovery",
            GuardianActionKind::Downgrade,
            FailureMemoryActionOutcome::Failed,
            "2026-07-11T09:00:00Z",
            Some("2026-07-11T11:00:00Z"),
            &intent_fingerprint(GuardianLaunchRecoveryKind::DowngradePreset),
        );
        let facts =
            launch_failure_memory_guardian_facts(intake_request(std::slice::from_ref(&repair)));

        assert_eq!(facts.len(), 2);
        for fact in &facts {
            for field in &fact.fields {
                assert_eq!(field.sensitivity, EvidenceSensitivity::Public);
                assert!(field.value_for(RedactionAudience::UserVisible).is_some());
                assert!(
                    field
                        .value_for(RedactionAudience::ExportableProof)
                        .is_some()
                );
            }
        }
        serde_json::to_string(&facts).expect("serialize memory intake facts");
    }

    #[test]
    fn intake_fails_closed_for_invalid_request_or_oversized_input() {
        let entry = startup_entry(
            "instance-a",
            GuardianMode::Managed,
            LaunchFailureClass::OutOfMemory,
            "2026-07-11T09:00:00Z",
        );
        let oversized = vec![entry.clone(); DEFAULT_FAILURE_MEMORY_LIMIT + 1];
        assert!(launch_failure_memory_guardian_facts(intake_request(&oversized)).is_empty());
        assert!(
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                current_at: "not-a-time",
                ..intake_request(std::slice::from_ref(&entry))
            })
            .is_empty()
        );
        assert!(
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                current_at: "2026-07-11T12:00:00+02:00",
                ..intake_request(std::slice::from_ref(&entry))
            })
            .is_empty()
        );
        assert!(
            launch_failure_memory_guardian_facts(GuardianLaunchFailureMemoryIntakeRequest {
                instance_id: "/home/alice/instance",
                ..intake_request(std::slice::from_ref(&entry))
            })
            .is_empty()
        );
    }

    fn startup_entry(
        instance_id: &str,
        mode: GuardianMode,
        failure_class: LaunchFailureClass,
        observed_at: &str,
    ) -> GuardianFailureMemoryEntry {
        GuardianFailureMemoryEntry::observed(
            DiagnosisId::new(failure_class.as_str()),
            GuardianDomain::Startup,
            instance_target(instance_id, OwnershipClass::UserOwned),
            mode,
            None,
            observed_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn repair_entry(
        instance_id: &str,
        mode: GuardianMode,
        diagnosis: &str,
        action: GuardianActionKind,
        outcome: FailureMemoryActionOutcome,
        observed_at: &str,
        suppression_until: Option<&str>,
        user_intent_hash: &str,
    ) -> GuardianFailureMemoryEntry {
        let mut entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::new(diagnosis),
            GuardianDomain::Launch,
            instance_target(instance_id, OwnershipClass::LauncherManaged),
            mode,
            Some(user_intent_hash),
            observed_at,
        )
        .with_action(action, outcome)
        .with_repair_attempt();
        if let Some(suppression_until) = suppression_until {
            entry = entry.with_suppression_until(suppression_until);
        }
        entry
    }

    fn instance_target(instance_id: &str, ownership: OwnershipClass) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Instance,
            instance_id,
            ownership,
        )
    }

    fn intake_request<'a>(
        entries: &'a [GuardianFailureMemoryEntry],
    ) -> GuardianLaunchFailureMemoryIntakeRequest<'a> {
        GuardianLaunchFailureMemoryIntakeRequest {
            entries,
            instance_id: "instance-a",
            mode: GuardianMode::Managed,
            current_at: "2026-07-11T10:00:00Z",
            current_intent: current_intent(),
            current_memory_mb: 1024,
            suggested_memory_mb: Some(2048),
        }
    }

    fn current_intent() -> GuardianLaunchRecoveryCurrentIntent<'static> {
        GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "1.21.1",
            requested_java: "/opt/java/bin/java",
            explicit_jvm_args: &[],
            requested_preset: "graalvm",
        }
    }

    fn intent_fingerprint(kind: GuardianLaunchRecoveryKind) -> String {
        launch_recovery_user_intent_fingerprint(current_intent(), kind)
            .expect("valid test recovery intent")
    }

    fn field<'a>(fact: &'a GuardianFact, key: &str) -> Option<&'a str> {
        fact.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.value.as_str())
    }
}
