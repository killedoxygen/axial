use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum FlagStage {
    Experimental,
    Beta,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum FlagSource {
    Default,
    Override,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct FlagViewModel {
    pub key: String,
    pub title: String,
    pub description: String,
    pub stage: FlagStage,
    pub dev_only: bool,
    pub default_enabled: bool,
    pub enabled: bool,
    pub source: FlagSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct FlagsResponse {
    /// Flags and ordinary preferences share one persisted revision.
    pub revision: u64,
    pub flags: Vec<FlagViewModel>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlagOverridePatch {
    pub expected_revision: u64,
    /// Explicit null removes an override; omission is rejected.
    #[serde(deserialize_with = "required_nullable")]
    pub enabled: Option<bool>,
}

fn required_nullable<'de, D: Deserializer<'de>>(de: D) -> Result<Option<bool>, D::Error> {
    Option::<bool>::deserialize(de)
}

pub const STATE_INSPECTOR_FLAG: &str = "dev.state-inspector";

pub(super) fn known_flag(key: &str) -> bool {
    key == STATE_INSPECTOR_FLAG
}

pub(super) fn flag_visible(key: &str, development_build: bool) -> bool {
    known_flag(key) && development_build
}

pub(super) fn project(
    revision: u64,
    overrides: &BTreeMap<String, bool>,
    development_build: bool,
) -> FlagsResponse {
    let flags = if development_build {
        let overridden = overrides.get(STATE_INSPECTOR_FLAG).copied();
        vec![FlagViewModel {
            key: STATE_INSPECTOR_FLAG.into(),
            title: "State inspector".into(),
            description: "Show the live state inspector tab in the Dev Lab.".into(),
            stage: FlagStage::Experimental,
            dev_only: true,
            default_enabled: false,
            enabled: overridden.unwrap_or(false),
            source: if overridden.is_some() {
                FlagSource::Override
            } else {
                FlagSource::Default
            },
        }]
    } else {
        Vec::new()
    };
    FlagsResponse { revision, flags }
}
