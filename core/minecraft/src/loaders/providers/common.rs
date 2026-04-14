use crate::loaders::http::fetch_bytes;
use crate::loaders::types::LoaderError;
use regex::Regex;

pub const FABRIC_META_BASE: &str = "https://meta.fabricmc.net/v2/versions";
pub const QUILT_META_BASE: &str = "https://meta.quiltmc.org/v3/versions";
pub const FORGE_MAVEN_META: &str =
    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";
pub const FORGE_PROMOTIONS_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
pub const NEOFORGE_MAVEN_META: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";
pub const FORGE_MAVEN_BASE: &str = "https://maven.minecraftforge.net";
pub const NEOFORGE_MAVEN_BASE: &str = "https://maven.neoforged.net/releases";

pub async fn fetch_text(url: &str) -> Result<String, LoaderError> {
    let bytes = fetch_bytes(url, 2 << 20).await?;
    String::from_utf8(bytes)
        .map_err(|error| LoaderError::Other(format!("invalid text body for {url}: {error}")))
}

pub fn parse_maven_versions(xml: &str) -> Vec<String> {
    let pattern = Regex::new(r"<version>([^<]+)</version>").expect("valid regex");
    pattern
        .captures_iter(xml)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_string()))
        .collect()
}

pub fn extract_forge_minecraft_version(entry: &str) -> String {
    entry
        .split_once('-')
        .map(|(minecraft_version, _)| minecraft_version.to_string())
        .unwrap_or_default()
}

pub fn extract_forge_loader_version(entry: &str) -> String {
    entry
        .split_once('-')
        .map(|(_, loader_version)| loader_version.to_string())
        .unwrap_or_default()
}

pub fn parse_version_triplet(version: &str) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for part in version.split('.') {
        if part.is_empty() {
            return None;
        }
        let digits = part
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        values.push(digits.parse::<u32>().ok()?);
    }
    Some(values)
}

pub fn minecraft_version_at_least(version: &str, target: &[u32]) -> bool {
    let Some(parts) = parse_version_triplet(version) else {
        return false;
    };
    for index in 0..target.len().max(parts.len()) {
        let left = *parts.get(index).unwrap_or(&0);
        let right = *target.get(index).unwrap_or(&0);
        if left != right {
            return left > right;
        }
    }
    true
}

pub fn neoforge_to_minecraft_version(version: &str) -> Option<String> {
    let numeric_parts = version
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
        })
        .take_while(|part| !part.is_empty())
        .collect::<Vec<_>>();

    let major = numeric_parts.first()?;
    let minor = numeric_parts.get(1)?;

    if major.parse::<u32>().ok()? >= 25 {
        let mut parts = vec![major.clone(), minor.clone()];
        if let Some(patch) = numeric_parts.get(2)
            && patch != "0"
        {
            parts.push(patch.clone());
        }
        return Some(parts.join("."));
    }

    if minor == "0" {
        Some(format!("1.{major}"))
    } else {
        Some(format!("1.{major}.{minor}"))
    }
}

#[cfg(test)]
mod tests {
    use super::neoforge_to_minecraft_version;

    #[test]
    fn maps_legacy_neoforge_versions_to_one_prefixed_minecraft_versions() {
        assert_eq!(
            neoforge_to_minecraft_version("21.0.167"),
            Some("1.21".to_string())
        );
        assert_eq!(
            neoforge_to_minecraft_version("21.11.5-beta"),
            Some("1.21.11".to_string())
        );
        assert_eq!(
            neoforge_to_minecraft_version("20.4.239"),
            Some("1.20.4".to_string())
        );
    }

    #[test]
    fn maps_year_based_neoforge_versions_without_one_prefix() {
        assert_eq!(
            neoforge_to_minecraft_version("26.1.0.7-beta"),
            Some("26.1".to_string())
        );
        assert_eq!(
            neoforge_to_minecraft_version("26.1.2.7-beta"),
            Some("26.1.2".to_string())
        );
    }
}
