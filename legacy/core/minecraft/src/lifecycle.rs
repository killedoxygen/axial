use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleChannel {
    Stable,
    Preview,
    Experimental,
    Legacy,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleLabel {
    Release,
    Recommended,
    Latest,
    Snapshot,
    PreRelease,
    ReleaseCandidate,
    Beta,
    Alpha,
    OldBeta,
    OldAlpha,
    Nightly,
    Dev,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LifecycleMeta {
    #[serde(default)]
    pub channel: LifecycleChannel,
    #[serde(default)]
    pub labels: Vec<LifecycleLabel>,
    #[serde(default)]
    pub default_rank: i32,
    #[serde(default)]
    pub badge_text: String,
    #[serde(default)]
    pub provider_terms: Vec<String>,
}

impl LifecycleMeta {
    pub fn new(
        channel: LifecycleChannel,
        labels: Vec<LifecycleLabel>,
        default_rank: i32,
        badge_text: impl Into<String>,
        provider_terms: Vec<String>,
    ) -> Self {
        Self {
            channel,
            labels,
            default_rank,
            badge_text: badge_text.into(),
            provider_terms,
        }
    }

    pub fn has_label(&self, label: LifecycleLabel) -> bool {
        self.labels.contains(&label)
    }
}
