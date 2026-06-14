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
