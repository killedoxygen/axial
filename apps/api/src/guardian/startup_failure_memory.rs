use super::{DiagnosisId, GuardianDomain, GuardianMode};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::failure_memory::{
    FailureMemoryStoreError, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
};

pub fn record_out_of_memory_observation(
    failure_memory: &GuardianFailureMemoryStore,
    instance_id: &str,
    mode: GuardianMode,
    observed_at: &str,
) -> Result<(), FailureMemoryStoreError> {
    failure_memory.record(GuardianFailureMemoryEntry::observed(
        DiagnosisId::new("out_of_memory"),
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
    fn repeated_out_of_memory_observations_merge_by_instance() {
        let store = GuardianFailureMemoryStore::new();
        record_out_of_memory_observation(
            &store,
            "instance-a",
            GuardianMode::Managed,
            "2026-01-01T00:00:00Z",
        )
        .expect("record first OOM");
        record_out_of_memory_observation(
            &store,
            "instance-a",
            GuardianMode::Managed,
            "2026-01-01T00:05:00Z",
        )
        .expect("record repeated OOM");

        let entries = store.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target.kind, TargetKind::Instance);
        assert_eq!(entries[0].target.id, "instance-a");
        assert_eq!(entries[0].occurrence_count, 2);
        assert_eq!(entries[0].first_observed_at, "2026-01-01T00:00:00Z");
        assert_eq!(entries[0].last_observed_at, "2026-01-01T00:05:00Z");
        assert_eq!(entries[0].last_action_kind, None);
        assert_eq!(entries[0].last_action_outcome, None);
    }
}
