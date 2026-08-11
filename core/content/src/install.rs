use crate::error::{ContentError, ContentResult};
use crate::limits::{
    MAX_CONTENT_ARTIFACT_BYTES, MAX_CONTENT_GRAPH_BYTES, MAX_DEPENDENCIES_PER_NODE,
    MAX_RESOLUTION_EDGES, MAX_RESOLUTION_NODES,
};
use crate::manifest::{ManifestEntry, entry_path_matches};
use crate::model::{
    CanonicalId, ContentDependency, ContentKind, FileRef, ManagedContentFileName, ProviderId,
};
use crate::transaction::contained_path;
use axial_minecraft::portable_path::{PortablePathKey, PortableRelativePath};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use url::Url;

/// A single resolved file the pipeline should download and record. Callers build
/// these from a resolution plan (selected content plus its dependencies).
#[derive(Clone)]
pub struct PlannedFile {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub kind: ContentKind,
    file: PlannedArtifact,
    pub dependencies: Vec<ContentDependency>,
    pub title: Option<String>,
}

#[derive(Clone)]
struct PlannedArtifact {
    filename: ManagedContentFileName,
    download_url: Url,
    sha1: Option<String>,
    sha512: String,
    size: u64,
}

impl PlannedFile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        kind: ContentKind,
        file: FileRef,
        dependencies: Vec<ContentDependency>,
        title: Option<String>,
    ) -> ContentResult<Self> {
        Ok(Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            file: PlannedArtifact::admit(kind, &file)?,
            dependencies,
            title,
        })
    }

    pub(crate) fn filename(&self) -> &ManagedContentFileName {
        &self.file.filename
    }

    pub(crate) fn download_url(&self) -> &Url {
        &self.file.download_url
    }

    pub(crate) fn sha1(&self) -> Option<&str> {
        self.file.sha1.as_deref()
    }

    pub(crate) fn sha512(&self) -> &str {
        &self.file.sha512
    }

    pub(crate) fn size(&self) -> u64 {
        self.file.size
    }
}

impl PlannedArtifact {
    fn admit(kind: ContentKind, file: &FileRef) -> ContentResult<Self> {
        let (size, download_url) = validate_planned_artifact(kind, file)?;
        Ok(Self {
            filename: ManagedContentFileName::new_exact(&file.filename).map_err(|_| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned an invalid content filename".to_string(),
                )
            })?,
            download_url,
            sha1: file.sha1.clone(),
            sha512: file.sha512.clone().ok_or_else(|| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned content without an exact SHA-512 digest".to_string(),
                )
            })?,
            size,
        })
    }
}

/// Revalidate a resolved plan immediately before any staging or filesystem
/// mutation. Resolution is the primary admission boundary, while this guard
/// prevents another in-process caller from constructing a weaker plan.
pub(crate) fn validate_install_plan(files: &[PlannedFile]) -> ContentResult<()> {
    if files.len() > MAX_RESOLUTION_NODES {
        return Err(ContentError::ProviderMetadataInvalid(
            "the content plan exceeds its item bound".to_string(),
        ));
    }
    let mut edge_count = 0_usize;
    let mut total_bytes = 0_u64;
    for planned in files {
        if planned.dependencies.len() > MAX_DEPENDENCIES_PER_NODE {
            return Err(ContentError::ProviderMetadataInvalid(
                "the content plan exceeds its per-item dependency bound".to_string(),
            ));
        }
        edge_count = edge_count
            .checked_add(planned.dependencies.len())
            .filter(|count| *count <= MAX_RESOLUTION_EDGES)
            .ok_or_else(|| {
                ContentError::ProviderMetadataInvalid(
                    "the content plan exceeds its dependency bound".to_string(),
                )
            })?;
        let artifact_bytes = planned.file.size;
        total_bytes = total_bytes
            .checked_add(artifact_bytes)
            .filter(|bytes| *bytes <= MAX_CONTENT_GRAPH_BYTES)
            .ok_or_else(|| {
                ContentError::ProviderMetadataInvalid(
                    "the content plan exceeds its aggregate download bound".to_string(),
                )
            })?;
    }
    Ok(())
}

pub(crate) fn validate_planned_artifact(
    kind: ContentKind,
    file: &FileRef,
) -> ContentResult<(u64, Url)> {
    if kind == ContentKind::Modpack {
        return Err(ContentError::ProviderMetadataInvalid(
            "a modpack is not installable as a single content artifact".to_string(),
        ));
    }
    let filename = ManagedContentFileName::new_exact(&file.filename).map_err(|_| {
        ContentError::ProviderMetadataInvalid(
            "the provider returned an invalid content filename".to_string(),
        )
    })?;
    if kind == ContentKind::Mod && !filename.key().as_str().ends_with(".jar") {
        return Err(ContentError::ProviderMetadataInvalid(
            "the provider returned an invalid content filename".to_string(),
        ));
    }
    let url = Url::parse(&file.url).map_err(|_| {
        ContentError::ProviderMetadataInvalid(
            "the provider returned an invalid content download URL".to_string(),
        )
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ContentError::ProviderMetadataInvalid(
            "content downloads require an HTTPS provider URL".to_string(),
        ));
    }
    if !file.sha512.as_deref().is_some_and(valid_sha512) {
        return Err(ContentError::ProviderMetadataInvalid(
            "the provider returned content without an exact SHA-512 digest".to_string(),
        ));
    }
    let size = file.size.filter(|size| *size > 0).ok_or_else(|| {
        ContentError::ProviderMetadataInvalid(
            "the provider returned content without a positive size".to_string(),
        )
    })?;
    if size > MAX_CONTENT_ARTIFACT_BYTES {
        return Err(ContentError::ProviderMetadataInvalid(
            "the provider returned an oversized content artifact".to_string(),
        ));
    }
    Ok((size, url))
}

fn valid_sha512(value: &str) -> bool {
    value.len() == 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRemoval {
    relative: PortableRelativePath,
    owner: ManifestEntry,
    present: bool,
}

/// Prevalidated portable identities protected from stale ownership cleanup.
/// Construction is linear once, while every stale variant lookup is O(1).
#[derive(Debug, Clone, Default)]
pub struct ProtectedManagedPaths {
    keys: HashSet<PortablePathKey>,
}

impl ProtectedManagedPaths {
    pub fn new(relative_paths: &[String]) -> ContentResult<Self> {
        let mut keys = HashSet::with_capacity(relative_paths.len());
        for relative in relative_paths {
            let relative = PortableRelativePath::new_exact(relative).map_err(|_| {
                ContentError::Invalid("protected content path is invalid".to_string())
            })?;
            keys.insert(relative.key());
        }
        Ok(Self { keys })
    }

    fn contains(&self, relative: &PortableRelativePath) -> bool {
        self.keys.contains(&relative.key())
    }
}

impl ManagedRemoval {
    pub fn relative_path(&self) -> &str {
        self.relative.as_str()
    }
}

/// Return every unprotected managed variant with its observed presence. A live
/// path whose bytes no longer match provenance is user-owned and aborts the
/// whole cleanup. Absent variants remain guarded through commit so a late path
/// cannot appear beside the removed ownership record.
pub fn verified_removable_variants(
    game_dir: &Path,
    entry: &ManifestEntry,
    protected_paths: &ProtectedManagedPaths,
) -> ContentResult<Vec<ManagedRemoval>> {
    let mut removable = Vec::new();
    for relative in managed_entry_variant_paths(entry)? {
        if protected_paths.contains(&relative) {
            continue;
        }
        let path = contained_path(game_dir, relative.as_str())?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                removable.push(ManagedRemoval {
                    relative,
                    owner: entry.clone(),
                    present: false,
                });
            }
            Err(error) => return Err(ContentError::Io(error)),
            Ok(metadata) if !metadata.is_file() => {
                return Err(ContentError::Invalid(
                    "a managed content path is no longer a regular file".to_string(),
                ));
            }
            Ok(_) if entry_path_matches(&path, entry) => {
                removable.push(ManagedRemoval {
                    relative,
                    owner: entry.clone(),
                    present: true,
                });
            }
            Ok(_) => {
                return Err(ContentError::Invalid(
                    "a managed content file changed outside the launcher".to_string(),
                ));
            }
        }
    }
    Ok(removable)
}

pub(crate) fn managed_variant_paths(
    kind: ContentKind,
    filename: &ManagedContentFileName,
) -> ContentResult<Vec<PortableRelativePath>> {
    let Some(kind_dir) = kind.install_subdir() else {
        return Ok(Vec::new());
    };
    let disabled = filename.disabled();
    [filename.as_str(), disabled.as_str()]
        .into_iter()
        .map(|filename| {
            PortableRelativePath::new_exact(&format!("{kind_dir}/{filename}")).map_err(|_| {
                ContentError::ProviderMetadataInvalid(
                    "the provider returned an invalid content destination".to_string(),
                )
            })
        })
        .collect()
}

pub(crate) fn managed_entry_variant_paths(
    entry: &ManifestEntry,
) -> ContentResult<Vec<PortableRelativePath>> {
    match entry.managed_filename() {
        Some(filename) => managed_variant_paths(entry.kind(), filename)
            .map_err(|_| ContentError::Invalid("managed content path is invalid".to_string())),
        None if entry.kind() == ContentKind::Modpack => Ok(Vec::new()),
        None => Err(ContentError::Invalid(
            "managed content path is invalid".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DependencyKind;

    fn planned(project: &str, filename: &str) -> PlannedFile {
        PlannedFile::new(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.to_string(),
            format!("{project}-version"),
            ContentKind::Mod,
            FileRef {
                url: format!("https://example.invalid/{filename}"),
                filename: filename.to_string(),
                sha1: None,
                sha512: Some("a".repeat(128)),
                size: Some(1),
                primary: true,
            },
            Vec::new(),
            Some(project.to_string()),
        )
        .expect("valid planned content")
    }

    fn dependency(index: usize) -> ContentDependency {
        ContentDependency {
            project_id: Some(format!("dependency-{index}")),
            version_id: None,
            kind: DependencyKind::Required,
        }
    }

    #[test]
    fn artifact_admission_is_exact_and_closed() {
        let candidate = FileRef {
            url: "https://example.invalid/project.jar".to_string(),
            filename: "project.jar".to_string(),
            sha1: None,
            sha512: Some("a".repeat(128)),
            size: Some(MAX_CONTENT_ARTIFACT_BYTES),
            primary: true,
        };
        assert_eq!(
            validate_planned_artifact(ContentKind::Mod, &candidate)
                .expect("exact artifact limit")
                .0,
            MAX_CONTENT_ARTIFACT_BYTES
        );

        let mut invalid = candidate.clone();
        invalid.filename = "project.zip".to_string();
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid = candidate.clone();
        invalid.url = "http://example.invalid/project.jar".to_string();
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid = candidate.clone();
        invalid.sha512 = Some("A".repeat(128));
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid = candidate.clone();
        invalid.size = Some(0);
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
        invalid.size = Some(MAX_CONTENT_ARTIFACT_BYTES + 1);
        assert!(validate_planned_artifact(ContentKind::Mod, &invalid).is_err());
    }

    #[test]
    fn install_plan_limits_admit_exact_and_reject_one_over() {
        let exact_nodes = (0..MAX_RESOLUTION_NODES)
            .map(|index| planned(&format!("project-{index}"), &format!("project-{index}.jar")))
            .collect::<Vec<_>>();
        validate_install_plan(&exact_nodes).expect("exact node limit");
        let mut too_many_nodes = exact_nodes;
        too_many_nodes.push(planned("overflow", "overflow.jar"));
        assert!(validate_install_plan(&too_many_nodes).is_err());

        let mut exact_edges = (0..(MAX_RESOLUTION_EDGES / MAX_DEPENDENCIES_PER_NODE))
            .map(|index| planned(&format!("root-{index}"), &format!("root-{index}.jar")))
            .collect::<Vec<_>>();
        for item in &mut exact_edges {
            item.dependencies = (0..MAX_DEPENDENCIES_PER_NODE).map(dependency).collect();
        }
        validate_install_plan(&exact_edges).expect("exact edge limit");
        let mut per_node_over = exact_edges.clone();
        per_node_over[0]
            .dependencies
            .push(dependency(MAX_DEPENDENCIES_PER_NODE));
        assert!(validate_install_plan(&per_node_over).is_err());
        let mut edge_over = exact_edges;
        let mut overflow_edge = planned("edge-over", "edge-over.jar");
        overflow_edge.dependencies.push(dependency(0));
        edge_over.push(overflow_edge);
        assert!(validate_install_plan(&edge_over).is_err());

        let mut exact_graph = planned("large", "large.jar");
        exact_graph.file.size = MAX_CONTENT_GRAPH_BYTES;
        validate_install_plan(std::slice::from_ref(&exact_graph)).expect("exact graph byte limit");
        let mut graph_over = exact_graph;
        graph_over.file.filename = ManagedContentFileName::new_exact("first.jar").unwrap();
        let second = planned("second", "second.jar");
        assert!(validate_install_plan(&[graph_over, second]).is_err());
    }
}
