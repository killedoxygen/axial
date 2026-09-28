//! Shared profile composition. Authentication remains with the retained provider
//! installer; successfully parsing a fragment alone never authenticates its source.

use axial_minecraft::VersionJson;
use axial_minecraft::loaders::{LoaderError, LoaderProfileFragment, compose_loader_version};

pub const MAX_PROFILE_BYTES: usize = 16 << 20;

pub fn parse_profile_fragment(bytes: &[u8]) -> Result<LoaderProfileFragment, LoaderError> {
    if bytes.is_empty() || bytes.len() > MAX_PROFILE_BYTES {
        return Err(LoaderError::InvalidProfile("loader profile exceeds its bounds".into()));
    }
    serde_json::from_slice(bytes)
        .map_err(|_| LoaderError::InvalidProfile("loader profile is malformed".into()))
}

pub fn compose_profile(
    authenticated_base: &VersionJson,
    base_version_id: &str,
    installed_version_id: &str,
    fragment: &LoaderProfileFragment,
) -> Result<VersionJson, LoaderError> {
    compose_loader_version(authenticated_base, base_version_id, installed_version_id, fragment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_admission_rejects_oversized_or_malformed_metadata() {
        assert!(parse_profile_fragment(&vec![b' '; MAX_PROFILE_BYTES + 1]).is_err());
        assert!(parse_profile_fragment(b"{bad").is_err());
        assert!(parse_profile_fragment(b"").is_err());
    }

    #[test]
    fn fragment_can_omit_base_owned_assets() {
        let fragment = parse_profile_fragment(br#"{"id":"fabric-loader-test-1.21.6","inheritsFrom":"1.21.6","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient"}"#).unwrap();
        assert!(fragment.asset_index.is_none());
        assert_eq!(fragment.inherits_from, "1.21.6");
    }
}
