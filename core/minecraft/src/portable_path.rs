use std::path::{Component, Path, PathBuf};

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

pub const MAX_PORTABLE_FILE_NAME_BYTES: usize = 255;
pub const MAX_PORTABLE_FILE_NAME_UTF16_UNITS: usize = 255;
pub const MAX_PORTABLE_RELATIVE_PATH_BYTES: usize = 512;
pub const MAX_PORTABLE_RELATIVE_PATH_UTF16_UNITS: usize = 512;

const DISABLED_SUFFIX: &str = ".disabled";
const MANAGED_CONTENT_EXACT_NAMES: [&str; 2] = ["axial.content.json", ".axial-publication"];
const MANAGED_CONTENT_PREFIXES: [&str; 3] = [
    ".axial-content-",
    ".axial-pack-",
    ".axial-replacement-",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortablePathError {
    NonUtf8,
    Unsafe,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortableFileName(String);

impl PortableFileName {
    pub fn new(value: &str) -> Result<Self, PortablePathError> {
        let spelling = nfc(value);
        validate_file_name(&spelling)?;
        Ok(Self(spelling))
    }

    pub fn new_exact(value: &str) -> Result<Self, PortablePathError> {
        let name = Self::new(value)?;
        if name.as_str() != value {
            return Err(PortablePathError::Unsafe);
        }
        Ok(name)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn key(&self) -> PortablePathKey {
        PortablePathKey::from_normalized(&self.0)
    }

    pub fn with_suffix(&self, suffix: &str) -> Result<Self, PortablePathError> {
        Self::new(&format!("{}{suffix}", self.0))
    }
}

impl std::fmt::Display for PortableFileName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortableRelativePath(String);

impl PortableRelativePath {
    pub fn new(value: &str) -> Result<Self, PortablePathError> {
        if value.is_empty() || value.contains('\\') {
            return Err(PortablePathError::Unsafe);
        }

        let spelling = value
            .split('/')
            .map(PortableFileName::new)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|name| name.0)
            .collect::<Vec<_>>()
            .join("/");
        validate_relative_path_bound(&spelling)?;
        Ok(Self(spelling))
    }

    pub fn new_exact(value: &str) -> Result<Self, PortablePathError> {
        let path = Self::new(value)?;
        if path.as_str() != value {
            return Err(PortablePathError::Unsafe);
        }
        Ok(path)
    }

    pub fn from_path(path: &Path) -> Result<Self, PortablePathError> {
        let mut names = Vec::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                return Err(PortablePathError::Unsafe);
            };
            let name = name.to_str().ok_or(PortablePathError::NonUtf8)?;
            names.push(PortableFileName::new(name)?);
        }
        if names.is_empty() {
            return Err(PortablePathError::Unsafe);
        }
        let spelling = names
            .into_iter()
            .map(|name| name.0)
            .collect::<Vec<_>>()
            .join("/");
        validate_relative_path_bound(&spelling)?;
        Ok(Self(spelling))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn join_under(&self, root: &Path) -> PathBuf {
        self.0.split('/').fold(root.to_path_buf(), |path, name| path.join(name))
    }

    pub fn key(&self) -> PortablePathKey {
        PortablePathKey::from_normalized(&self.0)
    }

    pub fn file_name(&self) -> PortableFileName {
        PortableFileName(
            self.0
                .rsplit('/')
                .next()
                .expect("portable relative paths have at least one leaf")
                .to_string(),
        )
    }
}

impl std::fmt::Display for PortableRelativePath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortablePathKey(String);

impl PortablePathKey {
    fn from_normalized(value: &str) -> Self {
        let folded = value.case_fold().collect::<String>();
        Self(folded.as_str().nfc().collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PortablePathKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub fn managed_content_name_is_reserved(name: &PortableFileName) -> bool {
    let key = managed_content_name_key(name);
    let base = key.as_str();
    MANAGED_CONTENT_EXACT_NAMES.contains(&base)
        || MANAGED_CONTENT_PREFIXES
            .iter()
            .any(|prefix| base.starts_with(prefix))
}

pub fn managed_content_name_key(name: &PortableFileName) -> PortablePathKey {
    let key = name.key();
    let mut base = key.as_str();
    while let Some(enabled) = base.strip_suffix(DISABLED_SUFFIX) {
        base = enabled;
    }
    PortablePathKey(base.to_string())
}

fn nfc(value: &str) -> String {
    value.nfc().collect()
}

fn validate_file_name(value: &str) -> Result<(), PortablePathError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > MAX_PORTABLE_FILE_NAME_BYTES
        || value.encode_utf16().count() > MAX_PORTABLE_FILE_NAME_UTF16_UNITS
        || value.bytes().any(|byte| b"<>:\"/\\|?*".contains(&byte))
        || value.chars().any(char::is_control)
        || value.ends_with(['.', ' '])
        || windows_device_name(value)
    {
        return Err(PortablePathError::Unsafe);
    }
    Ok(())
}

fn validate_relative_path_bound(value: &str) -> Result<(), PortablePathError> {
    if value.len() > MAX_PORTABLE_RELATIVE_PATH_BYTES
        || value.encode_utf16().count() > MAX_PORTABLE_RELATIVE_PATH_UTF16_UNITS
    {
        return Err(PortablePathError::Unsafe);
    }
    Ok(())
}

fn windows_device_name(value: &str) -> bool {
    let basename = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches(['.', ' ']);
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$", "CONIN$", "CONOUT$"]
        .iter()
        .any(|device| basename.eq_ignore_ascii_case(device))
    {
        return true;
    }

    let Some((prefix, digit)) = split_last_char(basename) else {
        return false;
    };
    matches!(digit, '0'..='9' | '\u{00b9}' | '\u{00b2}' | '\u{00b3}')
        && (prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
}

fn split_last_char(value: &str) -> Option<(&str, char)> {
    let (index, last) = value.char_indices().next_back()?;
    Some((&value[..index], last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_spelling_and_builds_full_case_folded_keys() {
        let composed = PortableRelativePath::new("Caf\u{e9}/Stra\u{df}e.jar").unwrap();
        let decomposed = PortableRelativePath::new("Cafe\u{301}/STRASSE.JAR").unwrap();

        assert_eq!(decomposed.as_str(), "Caf\u{e9}/STRASSE.JAR");
        assert_eq!(composed.key(), decomposed.key());
        assert_eq!(
            PortableRelativePath::new_exact("Cafe\u{301}/STRASSE.JAR"),
            Err(PortablePathError::Unsafe)
        );
    }

    #[test]
    fn rejects_wire_aliases_for_relative_paths() {
        for value in [
            "../lib.jar",
            "./lib.jar",
            "/lib.jar",
            "org//lib.jar",
            r"org\lib.jar",
            r"C:\lib.jar",
            r"\\server\lib.jar",
        ] {
            assert_eq!(PortableRelativePath::new(value), Err(PortablePathError::Unsafe));
        }
    }

    #[test]
    fn enforces_utf8_and_utf16_bounds_after_normalization() {
        assert!(PortableFileName::new(&"a".repeat(255)).is_ok());
        assert_eq!(
            PortableFileName::new(&"a".repeat(256)),
            Err(PortablePathError::Unsafe)
        );
        assert_eq!(
            PortableFileName::new(&"\u{1f600}".repeat(128)),
            Err(PortablePathError::Unsafe)
        );
        assert_eq!(
            PortableRelativePath::new(&format!("{}/{}", "a".repeat(255), "b".repeat(256))),
            Err(PortablePathError::Unsafe)
        );
        assert_eq!(
            PortableRelativePath::new(&format!(
                "{}/{}/c",
                "a".repeat(255),
                "b".repeat(255)
            )),
            Err(PortablePathError::Unsafe)
        );
    }

    #[test]
    fn admits_leading_spaces_unicode_and_non_reserved_internal_names() {
        for value in [" leading.jar", "caf\u{e9}.jar", ".axial-user-file"] {
            assert!(PortableFileName::new(value).is_ok(), "rejected {value:?}");
        }
    }

    #[test]
    fn rejects_forbidden_characters_trailing_aliases_and_devices() {
        for value in [
            "", ".", "..", "bad<name", "bad>name", "bad:name", "bad\"name", "bad|name",
            "bad?name", "bad*name", "bad/name", r"bad\name", "bad\nname", "name.", "name ",
            "CON", "prn.txt", "AUX", "nul.json", "CLOCK$.jar", "CONIN$", "conout$.log",
            "COM0", "com9.zip", "LPT0", "lpt9.tar", "COM\u{00b9}.jar", "lpt\u{00b2}",
            "LPT\u{00b3}.txt", "CON .txt", "COM1 .jar", "NUL...txt",
        ] {
            assert_eq!(PortableFileName::new(value), Err(PortablePathError::Unsafe), "accepted {value:?}");
        }
        for value in ["com10.jar", "lpt10.jar", "clock", "computer.jar"] {
            assert!(PortableFileName::new(value).is_ok(), "rejected {value:?}");
        }
    }

    #[test]
    fn reserves_only_managed_content_names_and_their_disabled_forms() {
        for value in [
            "axial.content.json",
            "AXIAL.CONTENT.JSON.disabled",
            ".axial-publication",
            ".axial-publication.DISABLED",
            ".axial-content-stage",
            ".axial-pack-123",
            ".axial-replacement-file.disabled",
            ".axial-pack-file.DISABLED.disabled",
        ] {
            let name = PortableFileName::new(value).unwrap();
            assert!(managed_content_name_is_reserved(&name), "not reserved: {value}");
        }
        for value in [".axial-user-file", "config/axial.content.json", "my-axial-pack"] {
            if let Ok(name) = PortableFileName::new(value) {
                assert!(!managed_content_name_is_reserved(&name), "over-reserved: {value}");
            }
        }
    }

    #[test]
    fn every_disabled_alias_of_a_managed_name_has_one_identity() {
        let enabled = PortableFileName::new("Stra\u{df}e.jar").unwrap();
        let disabled = PortableFileName::new("STRASSE.JAR.disabled").unwrap();
        let repeatedly_disabled =
            PortableFileName::new("stra\u{df}e.jar.DISABLED.disabled").unwrap();

        assert_eq!(
            managed_content_name_key(&enabled),
            managed_content_name_key(&disabled)
        );
        assert_eq!(
            managed_content_name_key(&enabled),
            managed_content_name_key(&repeatedly_disabled)
        );
    }
}
