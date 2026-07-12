use super::GuardianActionKind;
use crate::state::contracts::OperationPhase;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianUserOutcome {
    pub decision: GuardianActionKind,
    pub phase: OperationPhase,
    pub summary: String,
    pub details: Vec<String>,
    pub guidance: Vec<String>,
}
