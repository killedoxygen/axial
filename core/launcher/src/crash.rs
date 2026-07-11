use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_CRASH_ARTIFACT_BYTES: usize = 512 * 1024;
pub const MAX_CRASH_EVIDENCE_LINES: usize = 4_096;
pub const MAX_CRASH_EVIDENCE_LINE_BYTES: usize = 4_096;
pub const MAX_CRASH_EVIDENCE_MODS: usize = 8;
pub const MAX_CRASH_EVIDENCE_TOKEN_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashFailurePhase {
    Startup,
    Initialization,
    Loading,
    Runtime,
    Shutdown,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CrashEvidenceToken(String);

impl CrashEvidenceToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn sanitized(value: &str, max_chars: usize) -> Option<Self> {
        let value = value.trim();
        if value.is_empty()
            || value.contains(['/', '\\'])
            || value.chars().any(|character| {
                character.is_control()
                    || (!character.is_ascii_alphanumeric()
                        && !matches!(
                            character,
                            ' ' | '.'
                                | '_'
                                | '-'
                                | '+'
                                | ':'
                                | '#'
                                | '@'
                                | '('
                                | ')'
                                | '['
                                | ']'
                                | '$'
                        ))
            })
        {
            return None;
        }

        let mut sanitized = String::with_capacity(value.len().min(max_chars));
        let mut previous_space = false;
        for character in value.chars().take(max_chars) {
            if character == ' ' {
                if previous_space {
                    continue;
                }
                previous_space = true;
            } else {
                previous_space = false;
            }
            sanitized.push(character);
        }
        let sanitized = sanitized.trim();
        (!sanitized.is_empty()).then(|| Self(sanitized.to_string()))
    }
}

impl<'de> Deserialize<'de> for CrashEvidenceToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::sanitized(&value, MAX_CRASH_EVIDENCE_TOKEN_CHARS)
            .ok_or_else(|| de::Error::custom("invalid crash evidence token"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashModEvidence {
    pub name: CrashEvidenceToken,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<CrashEvidenceToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrashEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_phase: Option<CrashFailurePhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<CrashEvidenceToken>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspected_mods: Vec<CrashModEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problematic_frame: Option<CrashEvidenceToken>,
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
            failure_phase: Option<CrashFailurePhase>,
            exception_class: Option<CrashEvidenceToken>,
            #[serde(default)]
            suspected_mods: Vec<CrashModEvidence>,
            problematic_frame: Option<CrashEvidenceToken>,
            names_out_of_memory: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.suspected_mods.len() > MAX_CRASH_EVIDENCE_MODS {
            return Err(de::Error::custom("too many suspected mods"));
        }
        if wire
            .exception_class
            .as_ref()
            .is_some_and(|value| !is_java_class(value.as_str()))
        {
            return Err(de::Error::custom("invalid exception class"));
        }
        if wire
            .problematic_frame
            .as_ref()
            .is_some_and(|value| !is_problematic_frame_token(value.as_str()))
        {
            return Err(de::Error::custom("invalid problematic frame"));
        }
        Ok(Self {
            failure_phase: wire.failure_phase,
            exception_class: wire.exception_class,
            suspected_mods: wire.suspected_mods,
            problematic_frame: wire.problematic_frame,
            names_out_of_memory: wire.names_out_of_memory,
        })
    }
}

#[derive(Debug)]
struct PendingMod {
    id: Option<String>,
    name: CrashEvidenceToken,
    version: Option<CrashEvidenceToken>,
}

#[derive(Default)]
struct CrashEvidenceBuilder {
    failure_phase: Option<CrashFailurePhase>,
    exception_class: Option<CrashEvidenceToken>,
    suspected_mods: Vec<PendingMod>,
    problematic_frame: Option<CrashEvidenceToken>,
    names_out_of_memory: bool,
    expect_problematic_frame: bool,
}

impl CrashEvidenceBuilder {
    fn finish(self) -> Option<CrashEvidence> {
        let evidence = CrashEvidence {
            failure_phase: self.failure_phase,
            exception_class: self.exception_class,
            suspected_mods: self
                .suspected_mods
                .into_iter()
                .map(|entry| CrashModEvidence {
                    name: entry.name,
                    version: entry.version,
                })
                .collect(),
            problematic_frame: self.problematic_frame,
            names_out_of_memory: self.names_out_of_memory,
        };
        (evidence.failure_phase.is_some()
            || evidence.exception_class.is_some()
            || !evidence.suspected_mods.is_empty()
            || evidence.problematic_frame.is_some()
            || evidence.names_out_of_memory)
            .then_some(evidence)
    }

    fn inspect_line(&mut self, raw_line: &[u8]) {
        let line = String::from_utf8_lossy(raw_line);
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        self.names_out_of_memory |= line.contains("java.lang.OutOfMemoryError")
            || line.contains("GC overhead limit exceeded")
            || line.contains("Native memory allocation")
            || line.contains("Out of Memory Error");

        if self.expect_problematic_frame {
            self.expect_problematic_frame = false;
            if let Some(frame) = parse_problematic_frame(line) {
                self.problematic_frame = Some(frame);
                self.failure_phase.get_or_insert(CrashFailurePhase::Native);
            }
        }
        if line.eq_ignore_ascii_case("# Problematic frame:") {
            self.expect_problematic_frame = true;
            return;
        }

        if self.failure_phase.is_none() {
            self.failure_phase = parse_failure_phase(line);
        }
        if self.exception_class.is_none() {
            self.exception_class = parse_exception_class(line);
        }
        if let Some(value) = line.strip_prefix("Suspected Mods:") {
            self.add_suspected_mod_list(value);
        } else if let Some(value) = line.strip_prefix("Suspected Mod:") {
            self.add_suspected_mod(value);
        } else if line.starts_with("-- MOD ") && line.ends_with(" --") {
            let id = line
                .trim_start_matches("-- MOD ")
                .trim_end_matches(" --")
                .trim();
            self.add_mod(id, id, None);
        } else if line.contains('|') {
            self.enrich_forge_mod_list(line);
        } else if let Some(value) = line.strip_prefix("Mod Version:") {
            self.enrich_last_version(value);
        }
    }

    fn add_suspected_mod_list(&mut self, value: &str) {
        if value.trim().eq_ignore_ascii_case("none") {
            return;
        }
        for candidate in value.split(',') {
            self.add_suspected_mod(candidate);
            if self.suspected_mods.len() == MAX_CRASH_EVIDENCE_MODS {
                break;
            }
        }
    }

    fn add_suspected_mod(&mut self, value: &str) {
        let value = value.trim();
        let (value, version) = value
            .rsplit_once(" version ")
            .map_or((value, None), |(name, version)| (name, Some(version)));
        let (name, id) = value
            .rsplit_once(" (")
            .and_then(|(name, id)| id.strip_suffix(')').map(|id| (name, id)))
            .map_or((value, value), |parts| parts);
        self.add_mod(id, name, version);
    }

    fn add_mod(&mut self, id: &str, name: &str, version: Option<&str>) {
        if self.suspected_mods.len() >= MAX_CRASH_EVIDENCE_MODS {
            return;
        }
        let Some(name) = CrashEvidenceToken::sanitized(name, 96) else {
            return;
        };
        let id = sanitized_mod_id(id);
        let version = version.and_then(|value| CrashEvidenceToken::sanitized(value, 64));
        if self
            .suspected_mods
            .iter()
            .any(|entry| entry.id == id && entry.name == name)
        {
            return;
        }
        self.suspected_mods.push(PendingMod { id, name, version });
    }

    fn enrich_forge_mod_list(&mut self, line: &str) {
        let columns = line.split('|').map(str::trim).collect::<Vec<_>>();
        if columns.len() < 5 {
            return;
        }
        let name = columns[1];
        let id = columns[2];
        let version = columns[3];
        let Some(entry) = self.suspected_mods.iter_mut().find(|entry| {
            entry.id.as_deref() == Some(id) || entry.name.as_str().eq_ignore_ascii_case(name)
        }) else {
            return;
        };
        if entry.version.is_none() {
            entry.version = CrashEvidenceToken::sanitized(version, 64);
        }
    }

    fn enrich_last_version(&mut self, value: &str) {
        let Some(entry) = self.suspected_mods.last_mut() else {
            return;
        };
        if entry.version.is_none() {
            entry.version = CrashEvidenceToken::sanitized(value, 64);
        }
    }
}

pub fn parse_crash_evidence(raw: &[u8]) -> Option<CrashEvidence> {
    let bounded = &raw[..raw.len().min(MAX_CRASH_ARTIFACT_BYTES)];
    let mut builder = CrashEvidenceBuilder::default();
    for line in bounded
        .split(|byte| *byte == b'\n')
        .take(MAX_CRASH_EVIDENCE_LINES)
    {
        builder.inspect_line(&line[..line.len().min(MAX_CRASH_EVIDENCE_LINE_BYTES)]);
    }
    builder.finish()
}

fn parse_failure_phase(line: &str) -> Option<CrashFailurePhase> {
    let description = line
        .strip_prefix("Description:")?
        .trim()
        .to_ascii_lowercase();
    if description.contains("initializ") {
        Some(CrashFailurePhase::Initialization)
    } else if description.contains("load") || description.contains("bootstrap") {
        Some(CrashFailurePhase::Loading)
    } else if description.contains("start") {
        Some(CrashFailurePhase::Startup)
    } else if description.contains("shut") || description.contains("stopp") {
        Some(CrashFailurePhase::Shutdown)
    } else if description.contains("tick")
        || description.contains("render")
        || description.contains("game")
    {
        Some(CrashFailurePhase::Runtime)
    } else {
        None
    }
}

fn parse_exception_class(line: &str) -> Option<CrashEvidenceToken> {
    let value = line
        .strip_prefix("Exception:")
        .or_else(|| line.strip_prefix("Caused by:"))
        .map(str::trim)
        .unwrap_or(line);
    let candidate = value.split([':', ' ']).next()?.trim();
    if !is_java_class(candidate) {
        return None;
    }
    CrashEvidenceToken::sanitized(candidate, 128)
}

fn parse_problematic_frame(line: &str) -> Option<CrashEvidenceToken> {
    let value = line.strip_prefix('#').unwrap_or(line).trim();
    let value = value
        .strip_prefix("C  ")
        .or_else(|| value.strip_prefix("C "))
        .or_else(|| value.strip_prefix("V  "))
        .or_else(|| value.strip_prefix("J  "))?
        .trim();
    let start = value.find('[')?;
    let end = value[start..].find(']')? + start;
    let frame = &value[start..=end];
    is_problematic_frame_token(frame)
        .then(|| CrashEvidenceToken::sanitized(frame, MAX_CRASH_EVIDENCE_TOKEN_CHARS))?
}

fn is_java_class(value: &str) -> bool {
    let mut segments = value.split('.');
    segments.clone().count() >= 2
        && segments.all(|segment| {
            !segment.is_empty()
                && segment.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '$')
                })
        })
}

fn is_problematic_frame_token(value: &str) -> bool {
    value.starts_with('[')
        && value.ends_with(']')
        && value.contains('+')
        && value.len() <= MAX_CRASH_EVIDENCE_TOKEN_CHARS
}

fn sanitized_mod_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 96
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        }))
    .then(|| value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{
        CrashEvidence, CrashFailurePhase, MAX_CRASH_ARTIFACT_BYTES, MAX_CRASH_EVIDENCE_MODS,
        parse_crash_evidence,
    };

    const VANILLA: &[u8] = include_bytes!("../tests/fixtures/crash/vanilla.txt");
    const FORGE: &[u8] = include_bytes!("../tests/fixtures/crash/forge.txt");
    const FABRIC: &[u8] = include_bytes!("../tests/fixtures/crash/fabric.txt");
    const HS_ERR: &[u8] = include_bytes!("../tests/fixtures/crash/hs_err.log");
    const MALFORMED: &[u8] = include_bytes!("../tests/fixtures/crash/malformed.txt");

    #[test]
    fn parses_vanilla_exception_phase_and_oom_without_stack_text() {
        let evidence = parse_crash_evidence(VANILLA).expect("vanilla evidence");
        assert_eq!(evidence.failure_phase, Some(CrashFailurePhase::Runtime));
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("java.lang.OutOfMemoryError")
        );
        assert!(evidence.names_out_of_memory);
        assert!(evidence.suspected_mods.is_empty());
        assert!(
            !serde_json::to_string(&evidence)
                .expect("serialize")
                .contains("RenderSystem.java")
        );
    }

    #[test]
    fn parses_forge_suspected_mod_and_enriches_its_version() {
        let evidence = parse_crash_evidence(FORGE).expect("forge evidence");
        assert_eq!(evidence.failure_phase, Some(CrashFailurePhase::Loading));
        assert_eq!(evidence.suspected_mods.len(), 1);
        assert_eq!(evidence.suspected_mods[0].name.as_str(), "Example Machines");
        assert_eq!(
            evidence.suspected_mods[0]
                .version
                .as_ref()
                .map(|value| value.as_str()),
            Some("3.2.1")
        );
    }

    #[test]
    fn parses_fabric_suspected_mod_and_explicit_version() {
        let evidence = parse_crash_evidence(FABRIC).expect("fabric evidence");
        assert_eq!(
            evidence.failure_phase,
            Some(CrashFailurePhase::Initialization)
        );
        assert_eq!(evidence.suspected_mods.len(), 1);
        assert_eq!(evidence.suspected_mods[0].name.as_str(), "Canvas Renderer");
        assert_eq!(
            evidence.suspected_mods[0]
                .version
                .as_ref()
                .map(|value| value.as_str()),
            Some("1.4.2")
        );
    }

    #[test]
    fn parses_only_the_safe_hs_err_frame_token() {
        let evidence = parse_crash_evidence(HS_ERR).expect("hs_err evidence");
        assert_eq!(evidence.failure_phase, Some(CrashFailurePhase::Native));
        assert_eq!(
            evidence
                .problematic_frame
                .as_ref()
                .map(|value| value.as_str()),
            Some("[libGLX_nvidia.so.0+0x5a13f]")
        );
        let encoded = serde_json::to_string(&evidence).expect("serialize");
        for private in ["/home/alice", "access-token", "-Duser.home"] {
            assert!(!encoded.contains(private));
        }
    }

    #[test]
    fn malformed_and_every_truncation_are_bounded_and_panic_free() {
        let _ = parse_crash_evidence(MALFORMED);
        for fixture in [VANILLA, FORGE, FABRIC, HS_ERR, MALFORMED] {
            for length in 0..=fixture.len() {
                let _ = parse_crash_evidence(&fixture[..length]);
            }
        }
        let oversized = vec![b'x'; MAX_CRASH_ARTIFACT_BYTES + 32_768];
        assert!(parse_crash_evidence(&oversized).is_none());
    }

    #[test]
    fn public_json_round_trip_revalidates_every_field_and_caps_mods() {
        for fixture in [VANILLA, FORGE, FABRIC, HS_ERR] {
            let evidence = parse_crash_evidence(fixture).expect("fixture evidence");
            let encoded = serde_json::to_string(&evidence).expect("serialize");
            let decoded: CrashEvidence = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(decoded, evidence);
        }

        for invalid in [
            r#"{"failure_phase":null,"exception_class":"/home/alice/Secret","suspected_mods":[],"problematic_frame":null,"names_out_of_memory":false}"#,
            r#"{"failure_phase":null,"exception_class":"access-token","suspected_mods":[],"problematic_frame":null,"names_out_of_memory":false}"#,
            r#"{"failure_phase":null,"exception_class":null,"suspected_mods":[],"problematic_frame":"access-token","names_out_of_memory":false}"#,
        ] {
            assert!(serde_json::from_str::<CrashEvidence>(invalid).is_err());
        }

        let mods = (0..=MAX_CRASH_EVIDENCE_MODS)
            .map(|index| format!(r#"{{"name":"mod{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let oversized = format!(
            r#"{{"failure_phase":null,"exception_class":null,"suspected_mods":[{mods}],"problematic_frame":null,"names_out_of_memory":false}}"#
        );
        assert!(serde_json::from_str::<CrashEvidence>(&oversized).is_err());
    }

    #[test]
    fn unrelated_private_decoys_do_not_become_evidence() {
        let raw = br#"
Username: alice
JVM Flags: -Duser.home=/home/alice -Dtoken=access-token
Stacktrace: at private.mod.MemoryHelper.run(/home/alice/Secret.java:42)
Mod name: memory_optimizer
"#;
        assert!(parse_crash_evidence(raw).is_none());
    }
}
