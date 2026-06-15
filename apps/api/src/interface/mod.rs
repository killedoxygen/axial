//! Interface system boundary.
//!
//! Interface adapters own HTTP, desktop bridge, and frontend presentation
//! contracts that expose backend-authored Application state.

use crate::application::{ApplicationCommand, ApplicationOutcome, ApplicationViewModel};
use crate::observability::OperationEvent;
use crate::state::contracts::OperationId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub adapter: InterfaceAdapter,
    pub command: ApplicationCommand,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AdapterResponse {
    pub adapter: InterfaceAdapter,
    pub outcome: ApplicationOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewModelEnvelope {
    pub adapter: InterfaceAdapter,
    pub view_model: ApplicationViewModel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InterfaceAdapter {
    Api,
    Desktop,
    Frontend,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StreamEventEnvelope {
    pub adapter: InterfaceAdapter,
    pub stream: StreamContract,
    pub operation_id: Option<OperationId>,
    pub event: OperationEvent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StreamContract {
    OperationEvents,
    LaunchSessionEvents,
    ConfigChanges,
}
