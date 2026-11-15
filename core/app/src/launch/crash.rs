//! Bounded neutral crash evidence retained from the proven launcher leaf parser.
//! Artifact bytes must be admitted by the instance resource owner; this module never reads paths.

use serde::{Deserialize, Deserializer, Serialize, de};
use ts_rs::TS;

const MAX_SUSPECTED_MODS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CrashArtifactKind {
    MinecraftCrashReport,
    JvmFatalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CrashFailurePhase {
    Startup,
    Initialization,
    Loading,
    Runtime,
    Shutdown,
    Native,
}

macro_rules! evidence_value {
    ($name:ident, $validator:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
        #[serde(transparent)]
        #[ts(type = "string")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            fn checked(value: &str) -> Option<Self> {
                $validator(value).then(|| Self(value.to_string()))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::checked(&value)
                    .ok_or_else(|| de::Error::custom(concat!("invalid ", stringify!($name))))
            }
        }
    };
}

evidence_value!(CrashModName, is_safe_mod_name);
evidence_value!(CrashModVersion, is_safe_mod_version);
evidence_value!(CrashExceptionClass, is_throwable_class);
evidence_value!(CrashNativeModule, is_safe_native_module);
evidence_value!(CrashNativeSymbol, is_safe_native_identifier);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CrashNativeFrameKind {
    Native,
    Vm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CrashProblematicFrame {
    pub kind: CrashNativeFrameKind,
    pub module: CrashNativeModule,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub symbol: Option<CrashNativeSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CrashModEvidence {
    pub name: CrashModName,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub version: Option<CrashModVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CrashEvidence {
    pub source: CrashArtifactKind,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub failure_phase: Option<CrashFailurePhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub exception_class: Option<CrashExceptionClass>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(optional = nullable)]
    pub suspected_mods: Vec<CrashModEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub problematic_frame: Option<CrashProblematicFrame>,
    pub names_out_of_memory: bool,
}

impl<'de> Deserialize<'de> for CrashEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            source: CrashArtifactKind,
            truncated: bool,
            failure_phase: Option<CrashFailurePhase>,
            exception_class: Option<CrashExceptionClass>,
            #[serde(default)]
            suspected_mods: Vec<CrashModEvidence>,
            problematic_frame: Option<CrashProblematicFrame>,
            names_out_of_memory: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.suspected_mods.len() > MAX_SUSPECTED_MODS {
            return Err(de::Error::custom("too many suspected mods"));
        }
        if wire
            .suspected_mods
            .iter()
            .enumerate()
            .any(|(index, candidate)| wire.suspected_mods[..index].contains(candidate))
        {
            return Err(de::Error::custom("duplicate suspected mod"));
        }
        let source_is_coherent = match wire.source {
            CrashArtifactKind::MinecraftCrashReport => {
                wire.problematic_frame.is_none()
                    && wire.failure_phase != Some(CrashFailurePhase::Native)
            }
            CrashArtifactKind::JvmFatalError => {
                wire.exception_class.is_none()
                    && wire.suspected_mods.is_empty()
                    && matches!(wire.failure_phase, None | Some(CrashFailurePhase::Native))
            }
        };
        if !source_is_coherent {
            return Err(de::Error::custom("incoherent crash evidence source"));
        }
        if wire.failure_phase.is_none()
            && wire.exception_class.is_none()
            && wire.suspected_mods.is_empty()
            && wire.problematic_frame.is_none()
            && !wire.names_out_of_memory
        {
            return Err(de::Error::custom("empty crash evidence"));
        }
        Ok(Self {
            source: wire.source,
            truncated: wire.truncated,
            failure_phase: wire.failure_phase,
            exception_class: wire.exception_class,
            suspected_mods: wire.suspected_mods,
            problematic_frame: wire.problematic_frame,
            names_out_of_memory: wire.names_out_of_memory,
        })
    }
}

pub(crate) fn is_out_of_memory_failure_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    let marker = "java.lang.outofmemoryerror";
    let throwable = lower
        .strip_prefix(marker)
        .is_some_and(has_throwable_boundary)
        || lower
            .strip_prefix("caused by: ")
            .and_then(|value| value.strip_prefix(marker))
            .is_some_and(has_throwable_boundary)
        || lower
            .strip_prefix("exception in thread ")
            .is_some_and(|value| {
                value.find(marker).is_some_and(|index| {
                    index > 0 && has_throwable_boundary(&value[index + marker.len()..])
                })
            });
    throwable
        || lower == "gc overhead limit exceeded"
        || lower == "# there is insufficient memory for the java runtime environment to continue."
        || lower
            .strip_prefix("# native memory allocation (")
            .and_then(|detail| detail.split_once(") failed to "))
            .is_some_and(|(_, failure)| {
                failure.starts_with("allocate ") || failure.starts_with("map ")
            })
        || lower
            .strip_prefix("# out of memory error (")
            .is_some_and(|detail| detail.ends_with(')'))
}

fn has_throwable_boundary(remainder: &str) -> bool {
    remainder
        .chars()
        .next()
        .is_none_or(|character| character == ':' || character.is_ascii_whitespace())
}

fn is_java_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || matches!(character, '_' | '$'))
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$'))
}

fn is_throwable_class(value: &str) -> bool {
    value.len() <= 128
        && value.split('.').count() >= 2
        && value.split('.').all(is_java_identifier)
        && value.rsplit('.').next().is_some_and(|name| {
            name.ends_with("Error") || name.ends_with("Exception") || name.ends_with("Throwable")
        })
}

fn is_safe_mod_name(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 96
        && value == value.trim()
        && !value.contains("  ")
        && !value.contains(['/', '\\', '@', '=', '[', ']'])
        && !lower.contains("bearer")
        && !lower.contains("token")
        && !lower.contains("username")
        && !lower.ends_with(".jar")
        && !lower.ends_with(".dll")
        && !lower.contains(".so")
        && !lower.ends_with(".dylib")
        && !looks_like_sensitive_public_value(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    ' ' | '.' | '_' | '-' | '+' | ':' | '#' | '(' | ')'
                )
        })
}

fn is_safe_mod_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value == value.trim()
        && !value.starts_with("-D")
        && !value.contains("..")
        && !looks_like_sensitive_public_value(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '+')
        })
}

fn is_safe_native_identifier(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 96
        && !lower.contains("token")
        && !lower.contains("bearer")
        && !looks_like_sensitive_public_value(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '$' | ':')
        })
}

fn looks_like_sensitive_public_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
        "account_id",
        "account-id",
        "username",
        "xuid",
        "bearer",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || looks_like_jwt(value)
        || (value.len() >= 48
            && !value.contains(' ')
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            }))
}

fn looks_like_jwt(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && value.len() >= 12
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
        })
}

fn is_safe_native_module(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    is_safe_native_identifier(value)
        && !lower.ends_with(".dll")
        && !lower.contains(".so")
        && !lower.ends_with(".dylib")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_memory_lines_require_a_throwable_boundary() {
        for line in [
            "java.lang.OutOfMemoryError: Java heap space",
            "Caused by: java.lang.OutOfMemoryError",
            "Exception in thread main java.lang.OutOfMemoryError: Metaspace",
        ] {
            assert!(is_out_of_memory_failure_line(line), "{line}");
        }
        for line in [
            "java.lang.OutOfMemoryErrorHelper: decoy",
            "Caused by: java.lang.OutOfMemoryErrorGuide: decoy",
            "Exception in thread main java.lang.OutOfMemoryErrorHelper: decoy",
            "Comment: Out of Memory Error is the title of this guide",
        ] {
            assert!(!is_out_of_memory_failure_line(line), "{line}");
        }
    }

    #[test]
    fn public_json_rejects_incoherent_sources() {
        for incoherent in [
            r#"{"source":"minecraft_crash_report","truncated":false,"failure_phase":"native","exception_class":null,"suspected_mods":[],"problematic_frame":{"kind":"native","module":"nvoglv64","symbol":null},"names_out_of_memory":false}"#,
            r#"{"source":"jvm_fatal_error","truncated":false,"failure_phase":null,"exception_class":null,"suspected_mods":[{"name":"Example Mod"}],"problematic_frame":null,"names_out_of_memory":false}"#,
        ] {
            assert!(serde_json::from_str::<CrashEvidence>(incoherent).is_err());
        }
    }
}
