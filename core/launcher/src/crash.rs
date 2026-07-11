use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_CRASH_ARTIFACT_BYTES: usize = 512 * 1024;
pub const CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS: u64 = 15_000;
const MAX_LINES: usize = 4_096;
const MAX_LINE_BYTES: usize = 4_096;
const MAX_SUSPECTED_MODS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashArtifactKind {
    MinecraftCrashReport,
    JvmFatalError,
}

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

macro_rules! evidence_value {
    ($name:ident, $validator:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashNativeFrameKind {
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashProblematicFrame {
    pub kind: CrashNativeFrameKind,
    pub module: CrashNativeModule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<CrashNativeSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashModEvidence {
    pub name: CrashModName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<CrashModVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrashEvidence {
    pub source: CrashArtifactKind,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_phase: Option<CrashFailurePhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<CrashExceptionClass>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspected_mods: Vec<CrashModEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[derive(Debug)]
struct PendingMod {
    id: Option<String>,
    name: CrashModName,
    version: Option<CrashModVersion>,
}

struct CrashEvidenceBuilder {
    source: CrashArtifactKind,
    truncated: bool,
    failure_phase: Option<CrashFailurePhase>,
    exception_class: Option<CrashExceptionClass>,
    suspected_mods: Vec<PendingMod>,
    problematic_frame: Option<CrashProblematicFrame>,
    names_out_of_memory: bool,
    expect_problematic_frame: bool,
    expect_root_throwable: bool,
    current_failed_mod_id: Option<String>,
}

impl CrashEvidenceBuilder {
    fn new(source: CrashArtifactKind, truncated: bool) -> Self {
        Self {
            source,
            truncated,
            failure_phase: None,
            exception_class: None,
            suspected_mods: Vec::new(),
            problematic_frame: None,
            names_out_of_memory: false,
            expect_problematic_frame: false,
            expect_root_throwable: false,
            current_failed_mod_id: None,
        }
    }

    fn finish(self) -> Option<CrashEvidence> {
        let evidence = CrashEvidence {
            source: self.source,
            truncated: self.truncated,
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

        self.names_out_of_memory |= is_out_of_memory_failure_line(line);
        if self.source == CrashArtifactKind::JvmFatalError {
            if self.expect_problematic_frame {
                self.expect_problematic_frame = false;
                if let Some(frame) = parse_problematic_frame(line) {
                    self.problematic_frame = Some(frame);
                    self.failure_phase.get_or_insert(CrashFailurePhase::Native);
                }
            }
            if line.eq_ignore_ascii_case("# Problematic frame:") {
                self.expect_problematic_frame = true;
            }
            return;
        }

        if let Some(section) = line
            .strip_prefix("-- ")
            .and_then(|value| value.strip_suffix(" --"))
        {
            self.current_failed_mod_id = section.strip_prefix("MOD ").and_then(sanitized_mod_id);
            return;
        }

        if let Some(phase) = parse_failure_phase(line) {
            self.failure_phase.get_or_insert(phase);
            self.expect_root_throwable = true;
            return;
        }
        if self.exception_class.is_none() {
            self.exception_class = parse_exception_class(line, self.expect_root_throwable);
        }
        self.expect_root_throwable = false;
        if let Some(value) = line.strip_prefix("Suspected Mods:") {
            self.add_suspected_mod_list(value);
        } else if let Some(value) = line.strip_prefix("Suspected Mod:") {
            self.add_suspected_mod(value);
        } else if let Some(value) = line.strip_prefix("Failure message:") {
            self.add_failed_section_mod(value);
        } else if let Some(value) = line.strip_prefix("Mod Version:") {
            self.enrich_current_version(value);
        } else if line.contains('|') {
            self.enrich_forge_mod_list(line);
        }
    }

    fn add_suspected_mod_list(&mut self, value: &str) {
        if value.trim().eq_ignore_ascii_case("none") {
            return;
        }
        for candidate in value.split(',') {
            self.add_suspected_mod(candidate);
            if self.suspected_mods.len() == MAX_SUSPECTED_MODS {
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

    fn add_failed_section_mod(&mut self, value: &str) {
        let Some(id) = self.current_failed_mod_id.clone() else {
            return;
        };
        let (name, reported_id) = value
            .split_once(" (")
            .and_then(|(name, remainder)| {
                remainder
                    .split_once(')')
                    .map(|(reported_id, _)| (name.trim(), reported_id))
            })
            .filter(|(_, reported_id)| reported_id.eq_ignore_ascii_case(&id))
            .unwrap_or((&id, &id));
        self.add_mod(reported_id, name, None);
    }

    fn add_mod(&mut self, id: &str, name: &str, version: Option<&str>) {
        if self.suspected_mods.len() >= MAX_SUSPECTED_MODS {
            return;
        }
        let Some(name) = normalized_mod_name(name) else {
            return;
        };
        let id = sanitized_mod_id(id);
        let version = version.and_then(|value| CrashModVersion::checked(value.trim()));
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
        let (name, id, version) = (columns[1], columns[2], columns[3]);
        let Some(entry) = self.suspected_mods.iter_mut().find(|entry| {
            entry.id.as_deref() == Some(id) || entry.name.as_str().eq_ignore_ascii_case(name)
        }) else {
            return;
        };
        if entry.version.is_none() {
            entry.version = CrashModVersion::checked(version);
        }
    }

    fn enrich_current_version(&mut self, value: &str) {
        let Some(id) = self.current_failed_mod_id.as_deref() else {
            return;
        };
        let Some(entry) = self
            .suspected_mods
            .iter_mut()
            .find(|entry| entry.id.as_deref() == Some(id))
        else {
            return;
        };
        if entry.version.is_none() {
            entry.version = CrashModVersion::checked(value.trim());
        }
    }
}

pub fn parse_crash_evidence(source: CrashArtifactKind, raw: &[u8]) -> Option<CrashEvidence> {
    let bounded = &raw[..raw.len().min(MAX_CRASH_ARTIFACT_BYTES)];
    let mut lines = bounded.split(|byte| *byte == b'\n');
    let mut builder = CrashEvidenceBuilder::new(source, raw.len() > MAX_CRASH_ARTIFACT_BYTES);
    for _ in 0..MAX_LINES {
        let Some(line) = lines.next() else {
            return builder.finish();
        };
        if line.len() > MAX_LINE_BYTES {
            builder.truncated = true;
        }
        builder.inspect_line(&line[..line.len().min(MAX_LINE_BYTES)]);
    }
    builder.truncated |= lines.next().is_some();
    builder.finish()
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

fn parse_exception_class(line: &str, allow_bare: bool) -> Option<CrashExceptionClass> {
    let explicit = line
        .strip_prefix("Exception:")
        .or_else(|| line.strip_prefix("Caused by:"))
        .or_else(|| line.strip_prefix("Exception message:"));
    if explicit.is_none() && !allow_bare {
        return None;
    }
    let value = explicit.map(str::trim).unwrap_or(line);
    let (candidate, remainder) = value
        .split_once(':')
        .map_or((value, ""), |(candidate, remainder)| (candidate, remainder));
    if explicit.is_none() && remainder.is_empty() {
        return None;
    }
    CrashExceptionClass::checked(candidate.trim())
}

fn parse_problematic_frame(line: &str) -> Option<CrashProblematicFrame> {
    let value = line.strip_prefix('#').unwrap_or(line).trim();
    let value = value
        .strip_prefix("C  ")
        .or_else(|| value.strip_prefix("C "))?
        .trim();
    let start = value.find('[')?;
    let end = value[start..].find(']')? + start;
    let raw_frame = &value[start + 1..end];
    let (raw_module, raw_offset) = raw_frame.rsplit_once('+')?;
    if !is_native_offset(raw_offset) || raw_module.contains(['/', '\\']) {
        return None;
    }
    let module = CrashNativeModule::checked(strip_native_extension(raw_module))?;
    let symbol = value[end + 1..]
        .trim()
        .split('+')
        .next()
        .filter(|value| !value.is_empty())
        .and_then(CrashNativeSymbol::checked);
    Some(CrashProblematicFrame {
        kind: CrashNativeFrameKind::Native,
        module,
        symbol,
    })
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

fn normalized_mod_name(value: &str) -> Option<CrashModName> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    CrashModName::checked(&normalized)
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

fn is_native_offset(value: &str) -> bool {
    value.strip_prefix("0x").is_some_and(|digits| {
        !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_hexdigit())
    })
}

fn strip_native_extension(value: &str) -> &str {
    if let Some((base, _)) = value.split_once(".so") {
        base
    } else if let Some(base) = value.strip_suffix(".dll") {
        base
    } else if let Some(base) = value.strip_suffix(".dylib") {
        base
    } else {
        value
    }
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
    use super::*;

    const VANILLA: &[u8] = include_bytes!("../tests/fixtures/crash/vanilla.txt");
    const FORGE: &[u8] = include_bytes!("../tests/fixtures/crash/forge.txt");
    const FABRIC: &[u8] = include_bytes!("../tests/fixtures/crash/fabric.txt");
    const HS_ERR: &[u8] = include_bytes!("../tests/fixtures/crash/hs_err.log");
    const MALFORMED: &[u8] = include_bytes!("../tests/fixtures/crash/malformed.txt");

    fn parse_report(raw: &[u8]) -> Option<CrashEvidence> {
        parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, raw)
    }

    #[test]
    fn parses_vanilla_exception_phase_and_exact_oom() {
        let evidence = parse_report(VANILLA).expect("vanilla evidence");
        assert_eq!(evidence.source, CrashArtifactKind::MinecraftCrashReport);
        assert_eq!(evidence.failure_phase, Some(CrashFailurePhase::Runtime));
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("java.lang.OutOfMemoryError")
        );
        assert!(evidence.names_out_of_memory);
        assert!(!evidence.truncated);
        assert!(evidence.suspected_mods.is_empty());
    }

    #[test]
    fn parses_only_failed_forge_mod_section_and_version() {
        let evidence = parse_report(FORGE).expect("forge evidence");
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
    fn fabric_inventory_is_not_treated_as_attribution() {
        let evidence = parse_report(FABRIC).expect("fabric evidence");
        assert_eq!(
            evidence.failure_phase,
            Some(CrashFailurePhase::Initialization)
        );
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("net.fabricmc.loader.api.EntrypointException")
        );
        assert!(evidence.suspected_mods.is_empty());
    }

    #[test]
    fn hs_err_exports_structured_module_and_symbol_without_extension_or_offset() {
        let evidence = parse_crash_evidence(CrashArtifactKind::JvmFatalError, HS_ERR)
            .expect("hs_err evidence");
        let frame = evidence.problematic_frame.expect("problematic frame");
        assert_eq!(frame.module.as_str(), "libGLX_nvidia");
        assert_eq!(
            frame.symbol.as_ref().map(|value| value.as_str()),
            Some("glXSwapBuffers")
        );
        let encoded = serde_json::to_string(&frame).unwrap();
        assert!(!encoded.contains(".so"));
        assert!(!encoded.contains("0x"));
    }

    #[test]
    fn dotted_preamble_stack_and_oom_prose_do_not_become_evidence() {
        for raw in [
            "1.20.1 details\nexample.com support\nmod.example loaded",
            "at private.mod.MemoryException.run(MemoryException.java:42)",
            "Comment: Out of Memory Error is the title of this guide",
            "JVM Flags: -Dnote=java.lang.OutOfMemoryError -Duser.home=/home/alice",
        ] {
            assert!(parse_report(raw.as_bytes()).is_none(), "accepted {raw}");
        }
        let evidence =
            parse_report(b"Suspected Mods: Native memory allocation helper (memoryhelper)")
                .expect("safe suspected mod");
        assert!(!evidence.names_out_of_memory);

        let evidence = parse_report(
            b"com.attacker.FakeException: decoy\nDescription: Rendering game\njava.lang.IllegalStateException: real",
        )
        .expect("root throwable");
        assert_eq!(
            evidence
                .exception_class
                .as_ref()
                .map(|value| value.as_str()),
            Some("java.lang.IllegalStateException")
        );

        for helper in [
            "java.lang.OutOfMemoryErrorHelper: decoy",
            "Caused by: java.lang.OutOfMemoryErrorGuide: decoy",
            "Exception in thread main java.lang.OutOfMemoryErrorHelper: decoy",
        ] {
            assert!(!is_out_of_memory_failure_line(helper));
        }
    }

    #[test]
    fn artifact_kind_gates_extractors_and_wire_contract() {
        let spoofed_report = b"# Problematic frame:\n# C  [private.dll+0x12] secret+0x1";
        assert!(parse_report(spoofed_report).is_none());

        let spoofed_hs_err = b"Suspected Mods: Secret Mod (secretmod)\nDescription: Loading game";
        assert!(parse_crash_evidence(CrashArtifactKind::JvmFatalError, spoofed_hs_err).is_none());

        for incoherent in [
            r#"{"source":"minecraft_crash_report","truncated":false,"failure_phase":"native","exception_class":null,"suspected_mods":[],"problematic_frame":{"kind":"native","module":"nvoglv64","symbol":null},"names_out_of_memory":false}"#,
            r#"{"source":"jvm_fatal_error","truncated":false,"failure_phase":null,"exception_class":null,"suspected_mods":[{"name":"Example Mod"}],"problematic_frame":null,"names_out_of_memory":false}"#,
        ] {
            assert!(serde_json::from_str::<CrashEvidence>(incoherent).is_err());
        }
    }

    #[test]
    fn malformed_invalid_utf8_and_every_truncation_are_panic_free() {
        let mut malformed = MALFORMED.to_vec();
        malformed.extend_from_slice(&[0, 0xff, 0xfe]);
        let _ = parse_report(&malformed);
        for (kind, fixture) in [
            (CrashArtifactKind::MinecraftCrashReport, VANILLA),
            (CrashArtifactKind::MinecraftCrashReport, FORGE),
            (CrashArtifactKind::MinecraftCrashReport, FABRIC),
            (CrashArtifactKind::JvmFatalError, HS_ERR),
        ] {
            for length in 0..=fixture.len() {
                let _ = parse_crash_evidence(kind, &fixture[..length]);
            }
        }
    }

    #[test]
    fn truncation_is_explicit_and_work_is_bounded() {
        let mut before_cap =
            b"Description: Rendering game\njava.lang.IllegalStateException: first\n".to_vec();
        before_cap.resize(MAX_CRASH_ARTIFACT_BYTES + 32, b'x');
        let evidence = parse_report(&before_cap).expect("prefix evidence");
        assert!(evidence.truncated);

        let mut after_cap = vec![b'x'; MAX_CRASH_ARTIFACT_BYTES];
        after_cap.extend_from_slice(b"\njava.lang.IllegalStateException: hidden");
        assert!(parse_report(&after_cap).is_none());

        let huge_line = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(parse_report(&huge_line).is_none());
    }

    #[test]
    fn public_json_round_trip_revalidates_every_field() {
        for (kind, fixture) in [
            (CrashArtifactKind::MinecraftCrashReport, VANILLA),
            (CrashArtifactKind::MinecraftCrashReport, FORGE),
            (CrashArtifactKind::MinecraftCrashReport, FABRIC),
            (CrashArtifactKind::JvmFatalError, HS_ERR),
        ] {
            let evidence = parse_crash_evidence(kind, fixture).expect("fixture evidence");
            let encoded = serde_json::to_string(&evidence).expect("serialize");
            assert_eq!(
                serde_json::from_str::<CrashEvidence>(&encoded).unwrap(),
                evidence
            );
        }
    }

    #[test]
    fn public_json_rejects_empty_oversized_duplicate_and_sensitive_fields() {
        let base = |exception: &str, mods: &str, frame: &str| {
            format!(
                r#"{{"source":"minecraft_crash_report","truncated":false,"failure_phase":null,"exception_class":{exception},"suspected_mods":{mods},"problematic_frame":{frame},"names_out_of_memory":false}}"#
            )
        };
        for invalid in [
            base("null", "[]", "null"),
            base(r#""access-token""#, "[]", "null"),
            base("null", r#"[{"name":"Bearer raw-secret-token"}]"#, "null"),
            base("null", r#"[{"name":"alice@example.com"}]"#, "null"),
            base("null", r#"[{"name":"mod","version":"-Dtoken"}]"#, "null"),
            base("null", r#"[{"name":"SecretPlayer"}]"#, "null"),
            base("null", r#"[{"name":"account_id abc"}]"#, "null"),
            base("null", r#"[{"name":"Password credential"}]"#, "null"),
            base(
                "null",
                r#"[{"name":"mod","version":"access-token"}]"#,
                "null",
            ),
            base(
                "null",
                r#"[{"name":"mod","version":"abc.def.ghi123"}]"#,
                "null",
            ),
            base("null", r#"[{"name":"mod"},{"name":"mod"}]"#, "null"),
            base(
                "null",
                "[]",
                r#"{"kind":"native","module":"access-token","symbol":"secret"}"#,
            ),
            base(
                "null",
                "[]",
                r#"{"kind":"native","module":"raw_secret_value","symbol":null}"#,
            ),
        ] {
            assert!(serde_json::from_str::<CrashEvidence>(&invalid).is_err());
        }

        let long_name = "x".repeat(97);
        let invalid = base("null", &format!(r#"[{{"name":"{long_name}"}}]"#), "null");
        assert!(serde_json::from_str::<CrashEvidence>(&invalid).is_err());

        let long_version = "1".repeat(65);
        let invalid = base(
            "null",
            &format!(r#"[{{"name":"mod","version":"{long_version}"}}]"#),
            "null",
        );
        assert!(serde_json::from_str::<CrashEvidence>(&invalid).is_err());

        let long_exception = format!("example.{}Exception", "X".repeat(120));
        assert!(
            serde_json::from_str::<CrashEvidence>(&base(
                &format!(r#""{long_exception}""#),
                "[]",
                "null"
            ))
            .is_err()
        );

        let extension_frame = base(
            "null",
            "[]",
            r#"{"kind":"native","module":"private.dll","symbol":null}"#,
        );
        assert!(serde_json::from_str::<CrashEvidence>(&extension_frame).is_err());

        let mods = (0..=MAX_SUSPECTED_MODS)
            .map(|index| format!(r#"{{"name":"mod{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            serde_json::from_str::<CrashEvidence>(&base("null", &format!("[{mods}]"), "null"))
                .is_err()
        );
    }
}
