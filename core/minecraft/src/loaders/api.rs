use super::types::{LoaderBuildId, LoaderComponentId, LoaderComponentRecord};

pub fn loader_components() -> Vec<LoaderComponentRecord> {
    [
        LoaderComponentId::Fabric,
        LoaderComponentId::Quilt,
        LoaderComponentId::Forge,
        LoaderComponentId::NeoForge,
    ]
    .into_iter()
    .map(|id| LoaderComponentRecord {
        id,
        name: id.display_name().to_string(),
    })
    .collect()
}

pub fn build_id_for(
    component_id: LoaderComponentId,
    minecraft_version: &str,
    loader_version: &str,
) -> LoaderBuildId {
    format!(
        "{}:{}:{}",
        component_id.short_key(),
        minecraft_version.trim(),
        loader_version.trim()
    )
}

pub fn parse_build_id(build_id: &str) -> Option<(LoaderComponentId, String, String)> {
    let mut parts = build_id.splitn(3, ':');
    let component_id = LoaderComponentId::parse(parts.next()?)?;
    let minecraft_version = parts.next()?.trim();
    let loader_version = parts.next()?.trim();
    if minecraft_version.is_empty() || loader_version.is_empty() {
        return None;
    }
    Some((
        component_id,
        minecraft_version.to_string(),
        loader_version.to_string(),
    ))
}

pub fn installed_version_id_for(
    component_id: LoaderComponentId,
    minecraft_version: &str,
    loader_version: &str,
) -> String {
    match component_id {
        LoaderComponentId::Fabric => {
            format!(
                "fabric-loader-{}-{}",
                loader_version.trim(),
                minecraft_version.trim()
            )
        }
        LoaderComponentId::Quilt => {
            format!(
                "quilt-loader-{}-{}",
                loader_version.trim(),
                minecraft_version.trim()
            )
        }
        LoaderComponentId::Forge => {
            format!(
                "{}-forge-{}",
                minecraft_version.trim(),
                loader_version.trim()
            )
        }
        LoaderComponentId::NeoForge => format!("neoforge-{}", loader_version.trim()),
    }
}

pub fn infer_build_from_version_id(
    version_id: &str,
) -> Option<(LoaderComponentId, LoaderBuildId, String, String)> {
    let lower = version_id.to_ascii_lowercase();

    if let Some(rest) = version_id.strip_prefix("fabric-loader-") {
        let (loader_version, minecraft_version) = rest.rsplit_once('-')?;
        let component_id = LoaderComponentId::Fabric;
        return Some((
            component_id,
            build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version.to_string(),
            loader_version.to_string(),
        ));
    }

    if let Some(rest) = version_id.strip_prefix("quilt-loader-") {
        let (loader_version, minecraft_version) = rest.rsplit_once('-')?;
        let component_id = LoaderComponentId::Quilt;
        return Some((
            component_id,
            build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version.to_string(),
            loader_version.to_string(),
        ));
    }

    if let Some((minecraft_version, loader_version)) = version_id.split_once("-forge-") {
        let component_id = LoaderComponentId::Forge;
        return Some((
            component_id,
            build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version.to_string(),
            loader_version.to_string(),
        ));
    }

    if lower.starts_with("neoforge-") {
        let loader_version = version_id.strip_prefix("neoforge-")?;
        let minecraft_version = infer_neoforge_minecraft_version(loader_version)?;
        let component_id = LoaderComponentId::NeoForge;
        return Some((
            component_id,
            build_id_for(component_id, &minecraft_version, loader_version),
            minecraft_version,
            loader_version.to_string(),
        ));
    }

    None
}

pub fn infer_neoforge_minecraft_version(loader_version: &str) -> Option<String> {
    let numeric_parts = loader_version
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
