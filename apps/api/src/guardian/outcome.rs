use super::GuardianDecisionKind;
use crate::state::contracts::OperationPhase;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianUserOutcome {
    pub decision: GuardianDecisionKind,
    pub phase: OperationPhase,
    pub summary: String,
    pub details: Vec<String>,
    pub guidance: Vec<String>,
}
