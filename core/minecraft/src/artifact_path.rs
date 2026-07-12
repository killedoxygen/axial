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
}
