//! Redacted execution facts for concrete file effects.

use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{OperationId, TargetDescriptor};

pub(crate) fn file_fact(
    kind: ExecutionFactKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
) -> ExecutionFact {
    let target = safe_target_descriptor(target);
    ExecutionFact {
        operation_id,
        kind,
        target: Some(target.clone()),
        fields: vec![EvidenceField::new(
            "target",
            target.id.clone(),
            EvidenceSensitivity::Public,
        )],
    }
}

fn safe_target_descriptor(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(target.system, target.kind, &target.id, target.ownership)
}

#[cfg(test)]
mod tests {
    use super::file_fact;
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn file_facts_sanitize_unsafe_target_ids() {
        let target = TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Artifact,
            id: r"C:\Users\Alice\.minecraft\libraries\bad.jar token=secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        };

        let fact = file_fact(ExecutionFactKind::FileQuarantined, None, &target);
        let encoded = serde_json::to_string(&fact).expect("fact json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("target")
        );
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("token"));
        assert!(!lower.contains("secret"));
        assert!(!lower.contains("-xmx"));
    }
}
