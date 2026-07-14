use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_ARTIFACT_RELATIVE_PATH_BYTES: usize = 512;
pub(crate) const MAX_ARTIFACT_PATH_SEGMENT_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactRelativePathError {
    NonUtf8,
    Unsafe,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ArtifactRelativePath(String);

impl ArtifactRelativePath {
    pub(crate) fn new(value: &str) -> Result<Self, ArtifactRelativePathError> {
        canonical_relative_path(value).map(Self)
    }

    pub(crate) fn from_path(path: &Path) -> Result<Self, ArtifactRelativePathError> {
        let value = path.to_str().ok_or(ArtifactRelativePathError::NonUtf8)?;
        Self::new(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn join_under(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }

    pub(crate) fn portable_key(&self) -> String {
        self.0.chars().flat_map(char::to_lowercase).collect()
    }

    pub(crate) fn portable_persisted_key(&self) -> Result<String, ArtifactRelativePathError> {
        if !self.0.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
            return Err(ArtifactRelativePathError::Unsafe);
        }

        for segment in self.0.split('/') {
            if segment.bytes().any(|byte| b"<>:\"|?*".contains(&byte))
                || segment.ends_with('.')
                || segment.ends_with(' ')
                || windows_device_name(segment)
            {
                return Err(ArtifactRelativePathError::Unsafe);
            }
        }

        Ok(self.0.to_ascii_lowercase())
    }
}

pub(crate) fn validate_artifact_path_segment(value: &str) -> Result<(), ArtifactRelativePathError> {
    if unsafe_segment(value) {
        Err(ArtifactRelativePathError::Unsafe)
    } else {
        Ok(())
    }
}

fn canonical_relative_path(value: &str) -> Result<String, ArtifactRelativePathError> {
    if value.is_empty()
        || value.len() > MAX_ARTIFACT_RELATIVE_PATH_BYTES
        || value.starts_with('/')
        || value.starts_with('\\')
        || windows_prefixed(value)
        || value.chars().any(char::is_control)
    {
        return Err(ArtifactRelativePathError::Unsafe);
    }

    let mut segments = Vec::new();
    for segment in value.split(['/', '\\']) {
        if unsafe_segment(segment) {
            return Err(ArtifactRelativePathError::Unsafe);
        }
        segments.push(segment);
    }
    Ok(segments.join("/"))
}

fn unsafe_segment(segment: &str) -> bool {
    segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.len() > MAX_ARTIFACT_PATH_SEGMENT_BYTES
        || segment.contains(':')
        || segment.chars().any(char::is_control)
        || Path::new(segment)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
}

fn windows_prefixed(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || value.starts_with("//")
        || value.starts_with("\\\\")
        || value.starts_with("\\?\\")
        || value.starts_with("\\.\\")
}

fn windows_device_name(segment: &str) -> bool {
    let basename = segment.split('.').next().unwrap_or(segment);
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$"]
        .iter()
        .any(|device| basename.eq_ignore_ascii_case(device))
    {
        return true;
    }

    let bytes = basename.as_bytes();
    bytes.len() == 4
        && matches!(bytes[3], b'1'..=b'9')
        && (bytes[..3].eq_ignore_ascii_case(b"COM") || bytes[..3].eq_ignore_ascii_case(b"LPT"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_portable_separators() {
        let path = ArtifactRelativePath::new(r"org\example/lib.jar").expect("safe path");

        assert_eq!(path.as_str(), "org/example/lib.jar");
    }

    #[test]
    fn rejects_escaping_and_windows_prefixed_paths() {
        for value in ["../lib.jar", "/lib.jar", r"C:\lib.jar", r"\\server\lib.jar"] {
            assert_eq!(
                ArtifactRelativePath::new(value),
                Err(ArtifactRelativePathError::Unsafe)
            );
        }
    }

    #[test]
    fn validates_single_bounded_portable_segments() {
        assert_eq!(validate_artifact_path_segment("1.21.6"), Ok(()));
        for value in ["", ".", "..", "../escape", "/absolute", "C:drive"] {
            assert_eq!(
                validate_artifact_path_segment(value),
                Err(ArtifactRelativePathError::Unsafe)
            );
        }
        assert_eq!(
            validate_artifact_path_segment(&"a".repeat(MAX_ARTIFACT_PATH_SEGMENT_BYTES + 1)),
            Err(ArtifactRelativePathError::Unsafe)
        );
    }

    #[test]
    fn admits_portable_persisted_maven_and_asset_paths() {
        for value in [
            "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar",
            "objects/af/af35b0d348e1b3a99a36da3a955c90b7a98f03d8",
        ] {
            let path = ArtifactRelativePath::new(value).expect("canonical artifact path");
            assert_eq!(
                path.portable_persisted_key(),
                Ok(value.to_ascii_lowercase())
            );
        }
    }

    #[test]
    fn portable_persisted_keys_identify_ascii_case_aliases() {
        let first = ArtifactRelativePath::new("Org/Example/Library.jar").unwrap();
        let second = ArtifactRelativePath::new("org/example/library.JAR").unwrap();

        assert_eq!(
            first.portable_persisted_key(),
            second.portable_persisted_key()
        );
    }

    #[test]
    fn portable_persisted_paths_reject_windows_forbidden_characters() {
        for forbidden in ['<', '>', ':', '"', '|', '?', '*'] {
            let path = ArtifactRelativePath(format!("org/example/lib{forbidden}.jar"));
            assert_eq!(
                path.portable_persisted_key(),
                Err(ArtifactRelativePathError::Unsafe),
                "accepted forbidden character {forbidden:?}"
            );
        }
    }

    #[test]
    fn portable_persisted_paths_reject_trailing_dots_and_spaces() {
        for value in [
            "org/example./library.jar",
            "org/example /library.jar",
            "org/example/library.jar.",
            "org/example/library.jar ",
        ] {
            let path = ArtifactRelativePath::new(value).expect("general canonical path");
            assert_eq!(
                path.portable_persisted_key(),
                Err(ArtifactRelativePathError::Unsafe),
                "accepted trailing dot or space in {value:?}"
            );
        }
    }

    #[test]
    fn portable_persisted_paths_reject_dos_devices_with_extensions() {
        for value in [
            "CON",
            "prn.txt",
            "Aux/library.jar",
            "nul.json",
            "CLOCK$.jar",
            "com1",
            "COM9.zip",
            "lpt1",
            "LpT9.tar.gz",
        ] {
            let path = ArtifactRelativePath::new(value).expect("general canonical path");
            assert_eq!(
                path.portable_persisted_key(),
                Err(ArtifactRelativePathError::Unsafe),
                "accepted DOS device {value:?}"
            );
        }

        for value in ["com0", "com10.jar", "lpt0", "lpt10.jar", "clock"] {
            let path = ArtifactRelativePath::new(value).expect("general canonical path");
            assert_eq!(
                path.portable_persisted_key(),
                Ok(value.to_ascii_lowercase()),
                "rejected non-device {value:?}"
            );
        }
    }

    #[test]
    fn portable_persisted_paths_reject_non_ascii() {
        let path = ArtifactRelativePath::new("org/example/caf\u{e9}.jar")
            .expect("general canonical path remains Unicode-capable");

        assert_eq!(
            path.portable_persisted_key(),
            Err(ArtifactRelativePathError::Unsafe)
        );

        let delete_control = ArtifactRelativePath("org/example/lib\u{7f}.jar".to_string());
        assert_eq!(
            delete_control.portable_persisted_key(),
            Err(ArtifactRelativePathError::Unsafe)
        );
    }
}
