use super::MAX_VERSION_ID_BYTES;
use super::types::{
    LoaderBuildId, LoaderBuildRecord, LoaderComponentId, LoaderComponentRecord, LoaderError,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

const INSTALLED_VERSION_ID_PREFIX: &str = "loader-v2-";
const INSTALLED_VERSION_ID_DOMAIN: &[u8] = b"axial-installed-loader";

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
    let component_id = match parts.next()? {
        "fabric" => LoaderComponentId::Fabric,
        "quilt" => LoaderComponentId::Quilt,
        "forge" => LoaderComponentId::Forge,
        "neoforge" => LoaderComponentId::NeoForge,
        _ => return None,
    };
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
) -> Result<String, LoaderError> {
    validate_identity_coordinate(minecraft_version, "Minecraft version")?;
    validate_identity_coordinate(loader_version, "loader version")?;
    let minecraft_version = minecraft_version.as_bytes();
    let loader_version = loader_version.as_bytes();
    let minecraft_len = u16::try_from(minecraft_version.len())
        .map_err(|_| invalid_identity("Minecraft version is too long"))?;
    let loader_len = u16::try_from(loader_version.len())
        .map_err(|_| invalid_identity("loader version is too long"))?;

    let mut payload = Vec::with_capacity(
        INSTALLED_VERSION_ID_DOMAIN.len()
            + 1
            + 1
            + 2
            + minecraft_version.len()
            + 2
            + loader_version.len(),
    );
    payload.extend_from_slice(INSTALLED_VERSION_ID_DOMAIN);
    payload.push(0);
    payload.push(match component_id {
        LoaderComponentId::Fabric => 1,
        LoaderComponentId::Quilt => 2,
        LoaderComponentId::Forge => 3,
        LoaderComponentId::NeoForge => 4,
    });
    payload.extend_from_slice(&minecraft_len.to_be_bytes());
    payload.extend_from_slice(minecraft_version);
    payload.extend_from_slice(&loader_len.to_be_bytes());
    payload.extend_from_slice(loader_version);

    let version_id = format!(
        "{INSTALLED_VERSION_ID_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(payload)
    );
    if version_id.len() > MAX_VERSION_ID_BYTES {
        return Err(invalid_identity("encoded loader version id is too long"));
    }
    Ok(version_id)
}

pub(crate) fn validate_loader_build_record_identity(
    record: &LoaderBuildRecord,
) -> Result<(), LoaderError> {
    if record.build_id
        != build_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        )
    {
        return Err(invalid_identity("loader build id is not canonical"));
    }
    let expected_version_id = installed_version_id_for(
        record.component_id,
        &record.minecraft_version,
        &record.loader_version,
    )?;
    if record.version_id != expected_version_id {
        return Err(invalid_identity(
            "installed loader version id is not canonical",
        ));
    }
    Ok(())
}

fn validate_identity_coordinate(value: &str, name: &str) -> Result<(), LoaderError> {
    if value.is_empty() {
        return Err(invalid_identity(&format!("{name} is empty")));
    }
    if value != value.trim() {
        return Err(invalid_identity(&format!(
            "{name} contains surrounding whitespace"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid_identity(&format!(
            "{name} contains control characters"
        )));
    }
    Ok(())
}

fn invalid_identity(message: &str) -> LoaderError {
    LoaderError::InvalidProfile(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabric_and_quilt_ids_are_injective_across_delimiter_like_coordinates() {
        for component_id in [LoaderComponentId::Fabric, LoaderComponentId::Quilt] {
            let left = installed_version_id_for(component_id, "c", "a-b").expect("left id");
            let right = installed_version_id_for(component_id, "b-c", "a").expect("right id");

            assert_ne!(left, right);
        }
    }

    #[test]
    fn neoforge_id_is_bound_to_minecraft_target() {
        let left = installed_version_id_for(LoaderComponentId::NeoForge, "1.21.1", "21.1.200")
            .expect("left id");
        let right = installed_version_id_for(LoaderComponentId::NeoForge, "1.21.2", "21.1.200")
            .expect("right id");

        assert_ne!(left, right);
    }

    #[test]
    fn invalid_coordinates_do_not_produce_filesystem_ids() {
        assert!(installed_version_id_for(LoaderComponentId::Fabric, " 1.21.1", "0.16.10").is_err());
        assert!(
            installed_version_id_for(LoaderComponentId::Fabric, "1.21.1", "0.16.10\n").is_err()
        );
    }

    #[test]
    fn encoded_id_respects_known_good_json_filename_segment_limit() {
        let error = installed_version_id_for(
            LoaderComponentId::Fabric,
            "1.21.1",
            &"x".repeat(MAX_VERSION_ID_BYTES),
        )
        .expect_err("oversized encoded id");

        assert!(error.to_string().contains("too long"));
    }
}
