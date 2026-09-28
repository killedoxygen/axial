//! Shared public boundary. Feature payloads remain owned by their feature modules.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use ts_rs::TS;

/// Public failures contain sanitized user-facing copy, never provider diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ErrorResponse {
    pub error: String,
}

impl ErrorResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
        }
    }
}

/// Separate identity prevents development builds from admitting the legacy profile.
pub const DEVELOPMENT_APPLICATION_ID: &str = "com.mateoltd.axial.rewrite";

/// Stable public correlation identity. It does not grant operation authority.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, TS)]
#[ts(type = "string")]
pub struct OperationId(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Invalid operation identifier")]
pub struct InvalidOperationId;

impl OperationId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(value: &str) -> Result<Self, InvalidOperationId> {
        let parsed = uuid::Uuid::parse_str(value).map_err(|_| InvalidOperationId)?;
        let canonical = parsed.to_string();
        if canonical != value {
            return Err(InvalidOperationId);
        }
        Ok(Self(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for OperationId {
    type Err = InvalidOperationId;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}
impl TryFrom<String> for OperationId {
    type Error = InvalidOperationId;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}
impl From<OperationId> for String {
    fn from(value: OperationId) -> Self {
        value.0
    }
}

impl Serialize for OperationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_identity_round_trips_and_rejects_noncanonical_input() {
        let id = OperationId::new();
        assert_eq!(OperationId::parse(id.as_str()).unwrap(), id);
        assert_eq!(
            serde_json::from_str::<OperationId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
        assert!(OperationId::parse("../../another-instance").is_err());
        assert!(OperationId::parse(&id.as_str().replace('-', "")).is_err());
    }

    #[derive(Serialize, TS)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum WireProbe {
        Waiting {
            #[serde(skip_serializing_if = "Option::is_none")]
            #[ts(optional)]
            detail: Option<String>,
        },
        Done {
            detail: Option<String>,
        },
    }

    #[test]
    fn wire_generator_distinguishes_omission_null_and_tagged_states() {
        let missing = serde_json::to_value(WireProbe::Waiting { detail: None }).unwrap();
        let null = serde_json::to_value(WireProbe::Done { detail: None }).unwrap();
        assert_eq!(missing, serde_json::json!({"kind":"waiting"}));
        assert_eq!(null, serde_json::json!({"kind":"done","detail":null}));
        let declaration = WireProbe::decl(&ts_rs::Config::default());
        assert!(declaration.contains("detail?: string"), "{declaration}");
        assert!(
            declaration.contains("detail: string | null"),
            "{declaration}"
        );
        assert!(declaration.contains("\"waiting\"") && declaration.contains("\"done\""));
    }
}
