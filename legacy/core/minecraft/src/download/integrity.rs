#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherManagedArtifactReadiness {
    Missing,
    MetadataInvalid,
    MetadataMissing,
    UnsupportedExisting,
    Verified,
    Corrupt,
}

pub(super) fn is_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
