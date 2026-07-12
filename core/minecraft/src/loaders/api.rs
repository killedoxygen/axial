use super::MAX_VERSION_ID_BYTES;
use super::types::{
    LoaderBuildId, LoaderBuildRecord, LoaderComponentId, LoaderComponentRecord, LoaderError,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

const INSTALLED_VERSION_ID_PREFIX: &str = "loader-v2-";
const INSTALLED_VERSION_ID_DOMAIN: &[u8] = b"axial-installed-loader";

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InstalledLoaderIdentity {
    component_id: LoaderComponentId,
    minecraft_version: String,
    loader_version: String,
}

impl InstalledLoaderIdentity {
    pub(crate) fn component_id(&self) -> LoaderComponentId {
        self.component_id
    }

    pub(crate) fn minecraft_version(&self) -> &str {
        &self.minecraft_version
    }

    pub(crate) fn loader_version(&self) -> &str {
        &self.loader_version
    }
}

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

pub(crate) fn decode_installed_version_id(
    version_id: &str,
) -> Result<InstalledLoaderIdentity, LoaderError> {
    if version_id.len() > MAX_VERSION_ID_BYTES {
        return Err(invalid_identity("encoded loader version id is too long"));
    }
    let encoded = version_id
        .strip_prefix(INSTALLED_VERSION_ID_PREFIX)
        .ok_or_else(|| invalid_identity("installed loader version id has an invalid prefix"))?;
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_identity("installed loader version id payload is invalid"))?;
    let mut remaining = payload.as_slice();

    if take_payload_bytes(&mut remaining, INSTALLED_VERSION_ID_DOMAIN.len())
        != Some(INSTALLED_VERSION_ID_DOMAIN)
        || take_payload_bytes(&mut remaining, 1) != Some(&[0])
    {
        return Err(invalid_identity(
            "installed loader version id domain is invalid",
        ));
    }

    let component_id = match take_payload_bytes(&mut remaining, 1) {
        Some([1]) => LoaderComponentId::Fabric,
        Some([2]) => LoaderComponentId::Quilt,
        Some([3]) => LoaderComponentId::Forge,
        Some([4]) => LoaderComponentId::NeoForge,
        _ => {
            return Err(invalid_identity(
                "installed loader version id component is invalid",
            ));
        }
    };
    let minecraft_version = take_identity_coordinate(&mut remaining, "Minecraft version")?;
    let loader_version = take_identity_coordinate(&mut remaining, "loader version")?;
    if !remaining.is_empty() {
        return Err(invalid_identity(
            "installed loader version id has trailing data",
        ));
    }

    let identity = InstalledLoaderIdentity {
        component_id,
        minecraft_version,
        loader_version,
    };
    let canonical = installed_version_id_for(
        identity.component_id,
        &identity.minecraft_version,
        &identity.loader_version,
    )?;
    if canonical != version_id {
        return Err(invalid_identity(
            "installed loader version id is not canonical",
        ));
    }
    Ok(identity)
}

fn take_identity_coordinate(remaining: &mut &[u8], name: &str) -> Result<String, LoaderError> {
    let length = take_payload_bytes(remaining, size_of::<u16>())
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]) as usize)
        .ok_or_else(|| invalid_identity(&format!("{name} length is missing")))?;
    let bytes = take_payload_bytes(remaining, length)
        .ok_or_else(|| invalid_identity(&format!("{name} length is invalid")))?;
    let value = std::str::from_utf8(bytes)
        .map_err(|_| invalid_identity(&format!("{name} is not UTF-8")))?
        .to_string();
    validate_identity_coordinate(&value, name)?;
    Ok(value)
}

fn take_payload_bytes<'a>(remaining: &mut &'a [u8], length: usize) -> Option<&'a [u8]> {
    if length > remaining.len() {
        return None;
    }
    let (taken, rest) = remaining.split_at(length);
    *remaining = rest;
    Some(taken)
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
    let installed_identity = decode_installed_version_id(&record.version_id)?;
    if installed_identity.component_id() != record.component_id
        || installed_identity.minecraft_version() != record.minecraft_version
        || installed_identity.loader_version() != record.loader_version
    {
        return Err(invalid_identity(
            "installed loader version id does not match the build identity",
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

    #[test]
    fn installed_version_id_decoder_round_trips_every_component_and_coordinate_shape() {
        let coordinates = [
            ("1.21.5", "0.16.14"),
            ("c", "a-b"),
            ("b-c", "a"),
            ("1.21.1", "21.1.200"),
            ("1.21.2", "21.1.200"),
            ("version:with:colons", "release+build.7"),
            ("1.20.1-pre1", "loader_\u{03b2}"),
        ];

        for component_id in [
            LoaderComponentId::Fabric,
            LoaderComponentId::Quilt,
            LoaderComponentId::Forge,
            LoaderComponentId::NeoForge,
        ] {
            for (minecraft_version, loader_version) in coordinates {
                let version_id =
                    installed_version_id_for(component_id, minecraft_version, loader_version)
                        .expect("canonical installed version id");
                let decoded = decode_installed_version_id(&version_id).expect("decoded identity");

                assert_eq!(decoded.component_id(), component_id);
                assert_eq!(decoded.minecraft_version(), minecraft_version);
                assert_eq!(decoded.loader_version(), loader_version);
                assert_eq!(
                    installed_version_id_for(
                        decoded.component_id(),
                        decoded.minecraft_version(),
                        decoded.loader_version(),
                    )
                    .expect("re-encoded identity"),
                    version_id
                );
            }
        }
    }

    #[test]
    fn installed_version_id_decoder_rejects_invalid_envelope_and_encoding() {
        let canonical = installed_version_id_for(LoaderComponentId::Fabric, "1.21.5", "0.16.14")
            .expect("canonical id");
        let encoded = canonical
            .strip_prefix(INSTALLED_VERSION_ID_PREFIX)
            .expect("canonical prefix");

        for hostile in [
            "",
            "loader-v1-Zm9v",
            "Loader-v2-Zm9v",
            "loader-v2-",
            "loader-v2-+w",
            "loader-v2-_w==",
        ] {
            assert!(
                decode_installed_version_id(hostile).is_err(),
                "accepted hostile id {hostile:?}"
            );
        }

        let mut ambiguous = encoded.as_bytes().to_vec();
        let final_index = base64_url_value(*ambiguous.last().expect("encoded payload"));
        assert_eq!(
            final_index & 0b11,
            0,
            "fixture must have unused base64 bits"
        );
        *ambiguous.last_mut().expect("encoded payload") =
            BASE64_URL_ALPHABET[(final_index | 1) as usize];
        let ambiguous = format!(
            "{INSTALLED_VERSION_ID_PREFIX}{}",
            std::str::from_utf8(&ambiguous).expect("base64 text")
        );
        assert!(decode_installed_version_id(&ambiguous).is_err());

        let overlong = format!(
            "{INSTALLED_VERSION_ID_PREFIX}{}",
            "A".repeat(MAX_VERSION_ID_BYTES)
        );
        assert!(decode_installed_version_id(&overlong).is_err());
    }

    #[test]
    fn installed_version_id_decoder_rejects_domain_and_component_mutations() {
        let payload = canonical_payload();

        let mut wrong_domain = payload.clone();
        wrong_domain[0] ^= 1;
        assert_payload_rejected(wrong_domain);

        let mut missing_domain_separator = payload.clone();
        missing_domain_separator[INSTALLED_VERSION_ID_DOMAIN.len()] = 1;
        assert_payload_rejected(missing_domain_separator);

        for component_tag in [0, 5, u8::MAX] {
            let mut unknown_component = payload.clone();
            unknown_component[INSTALLED_VERSION_ID_DOMAIN.len() + 1] = component_tag;
            assert_payload_rejected(unknown_component);
        }
    }

    #[test]
    fn installed_version_id_decoder_rejects_invalid_lengths_and_trailing_data() {
        let payload = canonical_payload();
        let minecraft_length_offset = INSTALLED_VERSION_ID_DOMAIN.len() + 2;
        let minecraft_length = u16::from_be_bytes([
            payload[minecraft_length_offset],
            payload[minecraft_length_offset + 1],
        ]) as usize;
        let loader_length_offset = minecraft_length_offset + 2 + minecraft_length;

        for truncated_length in [
            payload[..minecraft_length_offset].to_vec(),
            payload[..loader_length_offset + 1].to_vec(),
        ] {
            assert_payload_rejected(truncated_length);
        }

        let mut excessive_minecraft_length = payload.clone();
        excessive_minecraft_length[minecraft_length_offset..minecraft_length_offset + 2]
            .copy_from_slice(&u16::MAX.to_be_bytes());
        assert_payload_rejected(excessive_minecraft_length);

        let mut excessive_loader_length = payload.clone();
        excessive_loader_length[loader_length_offset..loader_length_offset + 2]
            .copy_from_slice(&u16::MAX.to_be_bytes());
        assert_payload_rejected(excessive_loader_length);

        let mut trailing = payload;
        trailing.push(0);
        assert_payload_rejected(trailing);
    }

    #[test]
    fn installed_version_id_decoder_rejects_invalid_coordinate_bytes_and_values() {
        for (minecraft_version, loader_version) in [
            (b"".as_slice(), b"0.16.14".as_slice()),
            (b"1.21.5".as_slice(), b"".as_slice()),
            (b" 1.21.5".as_slice(), b"0.16.14".as_slice()),
            (b"1.21.5".as_slice(), b"0.16.14\n".as_slice()),
            (&[0xff], b"0.16.14".as_slice()),
            (b"1.21.5".as_slice(), &[0xc3, 0x28]),
        ] {
            assert_payload_rejected(identity_payload(1, minecraft_version, loader_version));
        }
    }

    const BASE64_URL_ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    fn canonical_payload() -> Vec<u8> {
        identity_payload(1, b"1.21.5", b"0.16.14")
    }

    fn identity_payload(
        component_tag: u8,
        minecraft_version: &[u8],
        loader_version: &[u8],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(INSTALLED_VERSION_ID_DOMAIN);
        payload.push(0);
        payload.push(component_tag);
        payload.extend_from_slice(
            &u16::try_from(minecraft_version.len())
                .expect("Minecraft coordinate length")
                .to_be_bytes(),
        );
        payload.extend_from_slice(minecraft_version);
        payload.extend_from_slice(
            &u16::try_from(loader_version.len())
                .expect("loader coordinate length")
                .to_be_bytes(),
        );
        payload.extend_from_slice(loader_version);
        payload
    }

    fn assert_payload_rejected(payload: Vec<u8>) {
        let version_id = format!(
            "{INSTALLED_VERSION_ID_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(payload)
        );
        assert!(
            decode_installed_version_id(&version_id).is_err(),
            "accepted hostile payload as {version_id}"
        );
    }

    fn base64_url_value(byte: u8) -> u8 {
        u8::try_from(
            BASE64_URL_ALPHABET
                .iter()
                .position(|candidate| *candidate == byte)
                .expect("base64url byte"),
        )
        .expect("base64url index")
    }
}
