use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureFlagDef {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub stage: FlagStage,
    pub dev_only: bool,
    pub default_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagStage {
    Experimental,
    Beta,
}

pub const FEATURE_FLAGS: &[FeatureFlagDef] = &[FeatureFlagDef {
    key: "dev.state-inspector",
    title: "State inspector",
    description: "Show the live state inspector tab in the Dev Lab.",
    stage: FlagStage::Experimental,
    dev_only: true,
    default_enabled: false,
}];

pub fn find_flag(key: &str) -> Option<&'static FeatureFlagDef> {
    FEATURE_FLAGS.iter().find(|flag| flag.key == key)
}
