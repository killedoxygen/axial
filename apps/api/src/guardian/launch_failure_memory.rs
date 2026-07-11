use super::{
    DiagnosisId, GuardianDomain, GuardianMode, launch_decision::is_guardian_launch_crash_class,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::failure_memory::{
    FailureMemoryStoreError, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
};
use axial_launcher::LaunchFailureClass;

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
}
