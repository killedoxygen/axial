use crate::healing::HealingEvent;
use crate::types::{LaunchFailure, LaunchState, SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSession {
    pub id: SessionId,
    pub state: LaunchState,
    pub healing: Vec<HealingEvent>,
    pub failure: Option<LaunchFailure>,
}
