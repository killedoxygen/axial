use super::api::loader_reconstruction_plan;
use super::types::{LoaderError, LoaderInstallStrategy};
use sha2::{Digest as _, Sha256};
use std::io::{Cursor, Read};

const DECLARATION: &str = "cpw/mods/fml/relauncher/CoreFMLLibraries.class";
const DECLARATION_BYTES: usize = 1172;
const DECLARATION_SHA256: &str = "dbccc9ce173f5db2f24cbb566d39fd9b784be8710d6bc791087efe22bf967b4a";
const MAX_ARCHIVE_BYTES: usize = 272 << 20;
const MAX_DIRECTORY_BYTES: usize = 16 << 20;
const MAX_ENTRIES: usize = 32_768;

/// A recognized declaration, not installation or filesystem authority.
#[derive(Clone, Debug)]
pub struct Requirements {
    version_id: String,
}

#[derive(Debug)]
pub struct Requirement {
    file_name: &'static str,
    path: &'static str,
    sha1: &'static str,
    size: u64,
    provider_url: &'static str,
}

const LIBRARIES: [Requirement; 4] = [
    Requirement {
        file_name: "argo-2.25.jar",
        path: "fml/bb672829fde76cb163004752b86b0484bd0a7f4b-argo-2.25.jar",
        sha1: "bb672829fde76cb163004752b86b0484bd0a7f4b",
        size: 123_642,
        provider_url: "https://maven.minecraftforge.net/net/sourceforge/argo/argo/2.25/argo-2.25.jar",
    },
    Requirement {
        file_name: "guava-12.0.1.jar",
        path: "fml/b8e78b9af7bf45900e14c6f958486b6ca682195f-guava-12.0.1.jar",
        sha1: "b8e78b9af7bf45900e14c6f958486b6ca682195f",
        size: 1_795_932,
        provider_url: "https://maven.minecraftforge.net/com/google/guava/guava/12.0.1/guava-12.0.1.jar",
    },
    Requirement {
        file_name: "asm-all-4.0.jar",
        path: "fml/98308890597acb64047f7e896638e0d98753ae82-asm-all-4.0.jar",
        sha1: "98308890597acb64047f7e896638e0d98753ae82",
        size: 212_767,
        provider_url: "https://netbeans.osuosl.org/binaries/98308890597ACB64047F7E896638E0D98753AE82-asm-all-4.0.jar",
    },
    Requirement {
        file_name: "bcprov-jdk15on-147.jar",
        path: "fml/b6f5d9926b0afbde9f4dbe3db88c5247be7794bb-bcprov-jdk15on-147.jar",
        sha1: "b6f5d9926b0afbde9f4dbe3db88c5247be7794bb",
        size: 1_997_327,
        provider_url: "https://maven.minecraftforge.net/org/bouncycastle/bcprov-jdk15on/1.47/bcprov-jdk15on-1.47.jar",
    },
];

impl Requirements {
    pub(crate) fn version_id(&self) -> &str {
        &self.version_id
    }

    pub fn entries(&self) -> &[Requirement] {
        &LIBRARIES
    }
}

impl Requirement {
    pub fn file_name(&self) -> &str {
        self.file_name
    }

    /// The immutable source path relative to the managed Libraries component.
    pub fn path(&self) -> &str {
        self.path
    }

    pub fn sha1(&self) -> &str {
        self.sha1
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn provider_url(&self) -> &str {
        self.provider_url
    }
}

/// Inspect already authenticated archive/client bytes. Callers retain the
/// original client and library guards; recognition grants no file authority.
pub fn recognize(version_id: &str, bytes: &[u8]) -> Result<Option<Requirements>, LoaderError> {
    if !applies_to(version_id) {
        return Ok(None);
    }
    let entries = bounded_directory_entries(bytes)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| invalid())?;
    if archive.len() != entries {
        return Err(invalid());
    }
    let mut found = false;
    for index in 0..entries {
        let mut entry = archive.by_index(index).map_err(|_| invalid())?;
        if entry.name() != DECLARATION {
            continue;
        }
        if found || entry.size() != DECLARATION_BYTES as u64 || !entry.is_file() {
            return Err(invalid());
        }
        let mut declaration = Vec::with_capacity(DECLARATION_BYTES);
        entry
            .by_ref()
            .take((DECLARATION_BYTES + 1) as u64)
            .read_to_end(&mut declaration)
            .map_err(|_| invalid())?;
        if declaration.len() != DECLARATION_BYTES
            || format!("{:x}", Sha256::digest(&declaration)) != DECLARATION_SHA256
        {
            return Err(invalid());
        }
        found = true;
    }
    Ok(found.then(|| Requirements {
        version_id: version_id.to_owned(),
    }))
}

pub fn applies_to(version_id: &str) -> bool {
    loader_reconstruction_plan(version_id)
        .is_ok_and(|plan| plan.record.strategy == LoaderInstallStrategy::ForgeEarliestLegacy)
}

pub fn scratch_bytes(archive_size: u64) -> Result<u64, LoaderError> {
    if archive_size > MAX_ARCHIVE_BYTES as u64 {
        return Err(invalid());
    }
    Ok(archive_size * 2 + (64 << 20))
}

fn bounded_directory_entries(bytes: &[u8]) -> Result<usize, LoaderError> {
    if bytes.len() < 22 || bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(invalid());
    }
    let end = (bytes.len().saturating_sub(65_557)..=bytes.len() - 22)
        .rev()
        .find(|&offset| {
            bytes[offset..offset + 4] == *b"PK\x05\x06"
                && offset
                    + 22
                    + u16::from_le_bytes([bytes[offset + 20], bytes[offset + 21]]) as usize
                    == bytes.len()
        })
        .ok_or_else(invalid)?;
    let record = &bytes[end..end + 22];
    let entries = u16::from_le_bytes([record[10], record[11]]) as usize;
    let directory_bytes = u32::from_le_bytes(record[12..16].try_into().unwrap()) as usize;
    let directory_offset = u32::from_le_bytes(record[16..20].try_into().unwrap()) as usize;
    if record[4..8] != [0; 4]
        || record[8..10] != record[10..12]
        || entries > MAX_ENTRIES
        || directory_bytes > MAX_DIRECTORY_BYTES
        || directory_offset.checked_add(directory_bytes) != Some(end)
        || end
            .checked_sub(20)
            .is_some_and(|offset| &bytes[offset..offset + 4] == b"PK\x06\x07")
    {
        return Err(invalid());
    }
    let mut at = directory_offset;
    for _ in 0..entries {
        let header = bytes.get(at..at + 46).ok_or_else(invalid)?;
        if &header[..4] != b"PK\x01\x02" {
            return Err(invalid());
        }
        at += 46
            + u16::from_le_bytes([header[28], header[29]]) as usize
            + u16::from_le_bytes([header[30], header[31]]) as usize
            + u16::from_le_bytes([header[32], header[33]]) as usize;
        if at > end {
            return Err(invalid());
        }
    }
    if at != end {
        return Err(invalid());
    }
    Ok(entries)
}

fn invalid() -> LoaderError {
    LoaderError::InvalidProfile("unsupported or invalid game library declaration".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loaders::types::LoaderComponentId;
    use base64::Engine as _;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn jar(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn recognition_requires_the_exact_declared_recipe_and_forge_identity() {
        let id =
            super::super::installed_version_id_for(LoaderComponentId::Forge, "1.4.7", "6.6.2.534")
                .unwrap();
        let class = base64::engine::general_purpose::STANDARD
            .decode(include_str!("../../tests/fixtures/fml-libraries.base64").trim())
            .unwrap();
        let requirements = recognize(&id, &jar(DECLARATION, &class)).unwrap().unwrap();
        assert_eq!(requirements.version_id(), id);
        assert_eq!(requirements.entries().len(), 4);
        assert_eq!(
            requirements.entries()[2].sha1(),
            "98308890597acb64047f7e896638e0d98753ae82"
        );
        assert_eq!(
            requirements.entries()[3].file_name(),
            "bcprov-jdk15on-147.jar"
        );
        assert!(recognize("1.4.7", b"not a ZIP").unwrap().is_none());
        let fabric =
            super::super::installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.19.5")
                .unwrap();
        assert!(recognize(&fabric, b"not a ZIP").unwrap().is_none());
        let modern =
            super::super::installed_version_id_for(LoaderComponentId::Forge, "1.20.1", "47.4.10")
                .unwrap();
        assert!(!applies_to(&modern));
        assert!(recognize(&modern, b"not a ZIP").unwrap().is_none());
        assert!(
            recognize(&id, &jar("old/Forge.class", b"older Forge"))
                .unwrap()
                .is_none()
        );
        let mut changed = class.clone();
        changed[0] ^= 1;
        assert!(recognize(&id, &jar(DECLARATION, &changed)).is_err());
        assert!(recognize(&id, &jar(DECLARATION, &class[..class.len() - 1])).is_err());
        assert!(recognize(&id, b"not a ZIP").is_err());
        assert_eq!(scratch_bytes(0).unwrap(), 64 << 20);
        assert_eq!(scratch_bytes(224 << 20).unwrap(), 512 << 20);
        assert!(scratch_bytes(MAX_ARCHIVE_BYTES as u64).is_ok());
        assert!(scratch_bytes(MAX_ARCHIVE_BYTES as u64 + 1).is_err());
        let mut excessive = jar(DECLARATION, &class);
        let end = excessive.len() - 22;
        excessive[end + 8..end + 12].fill(255);
        assert!(recognize(&id, &excessive).is_err());
    }
}
