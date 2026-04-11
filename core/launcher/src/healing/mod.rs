use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealingEventKind {
    RuntimeBypassed,
    PresetDowngraded,
    StartupStalled,
    FallbackApplied,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealingEvent {
    pub kind: HealingEventKind,
    #[serde(default)]
    pub detail: Option<String>,
}
