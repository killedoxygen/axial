use crate::install::{
    PlannedFile, managed_entry_variant_paths, managed_variant_paths, validate_install_plan,
};
use crate::{
    CanonicalId, ContentDependency, ContentError, ContentManifest, ContentResult, DependencyKind,
    ManifestEntry,
};
use axial_minecraft::download::{ExpectedTransferDigests, TransferContract};
use axial_minecraft::managed_path::{
    ManagedContentMutationPlan, ManagedContentObservedState, ManagedContentPathMutation,
    ManagedContentPathObservation, ManagedContentPathResult, ManagedContentPayloadId,
    ManagedContentPayloadPlan, ManagedContentPlanningBinding, ManagedContentPlanningSession,
    ManagedContentTransactionSession,
};
use axial_minecraft::portable_path::{PortablePathKey, PortableRelativePath};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use url::Url;

const MAX_TRANSACTION_PATHS: usize = 512;

/// Exact managed entries whose bytes were observed to match their manifest
/// ownership proof. Resolver decisions must use this set, never ambient paths.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveManagedContent {
    entries: HashMap<CanonicalId, ManifestEntry>,
}

/// Manifest decoded only from the bytes held by a Core planning capability.
/// Its private field prevents callers from substituting ambient manifest data.
pub struct ObservedContentManifest {
    manifest: ContentManifest,
    snapshot: Option<Box<[u8]>>,
    binding: ManagedContentPlanningBinding,
}

impl ObservedContentManifest {
    pub fn manifest(&self) -> &ContentManifest {
        &self.manifest
    }
}

impl LiveManagedContent {
    pub fn contains(&self, entry: &ManifestEntry) -> bool {
        self.entries.get(entry.canonical_id()) == Some(entry)
    }

    /// Build explicit liveness from entries authenticated by an adapter-owned
    /// source. This constructor is path-free; capability execution should use
    /// `derive_live_managed_content` instead.
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a ManifestEntry>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| (entry.canonical_id().clone(), entry.clone()))
                .collect(),
        }
    }
}

pub struct ManagedContentPayloadSource {
    id: ManagedContentPayloadId,
    url: Url,
}

impl ManagedContentPayloadSource {
    pub fn id(&self) -> &ManagedContentPayloadId {
        &self.id
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn into_parts(self) -> (ManagedContentPayloadId, Url) {
        (self.id, self.url)
    }
}

pub struct ManagedContentExecutionPlan {
    mutation: ManagedContentMutationPlan,
    sources: Vec<ManagedContentPayloadSource>,
    affected_entries: usize,
}

/// Move-only Content projection created from the complete planning snapshot
/// before Core narrows it to effect paths.
#[must_use = "managed content projections retain the operation's exact effects"]
pub struct ManagedContentOperationProjection {
    effects: Vec<ManagedContentPathMutation>,
    payloads: Vec<ManagedContentPayloadPlan>,
    sources: Vec<ManagedContentPayloadSource>,
    manifest_body: Vec<u8>,
    observed_manifest: Option<Box<[u8]>>,
    binding: ManagedContentPlanningBinding,
    affected_entries: usize,
}

impl ManagedContentOperationProjection {
    pub fn effect_paths(&self) -> Vec<PortableRelativePath> {
        self.effects
            .iter()
            .map(|effect| effect.path().clone())
            .collect()
    }

    pub fn seal(
        self,
        session: &ManagedContentTransactionSession,
    ) -> ContentResult<ManagedContentExecutionPlan> {
        seal_projection(session, self)
    }
}

impl ManagedContentExecutionPlan {
    pub fn into_parts(
        self,
    ) -> (
        ManagedContentMutationPlan,
        Vec<ManagedContentPayloadSource>,
        usize,
    ) {
        (self.mutation, self.sources, self.affected_entries)
    }
}

/// Decode the manifest snapshot held by the capability planning session.
pub fn decode_observed_content_manifest(
    session: &ManagedContentPlanningSession,
) -> ContentResult<ObservedContentManifest> {
    Ok(ObservedContentManifest {
        manifest: ContentManifest::decode_managed(session.manifest_bytes())?,
        snapshot: session.manifest_bytes().map(Into::into),
        binding: session.planning_binding(),
    })
}

/// Observe both enabled and disabled variants for every file-owning manifest
/// entry. These are the only facts from which resolver liveness may be built.
pub fn managed_content_liveness_paths(
    manifest: &ObservedContentManifest,
) -> ContentResult<Vec<PortableRelativePath>> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for entry in manifest.manifest().entries() {
        for path in managed_entry_variant_paths(entry)? {
            if seen.insert(path.key()) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

/// Filter candidate observations against facts already held by the planning
/// capability. Callers can pass the result directly to `observe_more` when it
/// is non-empty.
pub fn missing_managed_content_observations(
    session: &ManagedContentPlanningSession,
    candidates: Vec<PortableRelativePath>,
) -> ContentResult<Vec<PortableRelativePath>> {
    let observations = observation_index(&session.observations())?;
    let mut missing = Vec::new();
    let mut seen = observations.keys().cloned().collect::<HashSet<_>>();
    for path in candidates {
        if seen.insert(path.key()) {
            missing.push(path);
        }
    }
    Ok(missing)
}

pub fn derive_live_managed_content(
    manifest: &ObservedContentManifest,
    session: &ManagedContentPlanningSession,
) -> ContentResult<LiveManagedContent> {
    require_planning_binding(session, &manifest.binding)?;
    derive_liveness(manifest.manifest(), &session.observations())
}

/// Paths needed to install the resolved batch, in addition to the resolver's
/// liveness observations. Both variants are guarded because a manifest entry
/// exclusively owns both names.
pub fn managed_install_observation_paths(
    manifest: &ObservedContentManifest,
    files: &[PlannedFile],
) -> ContentResult<Vec<PortableRelativePath>> {
    validate_install_plan(files)?;
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for planned in files {
        if let Some(existing) = manifest.manifest().find(&planned.canonical_id) {
            append_unique_paths(
                &mut paths,
                &mut seen,
                managed_entry_variant_paths(existing)?,
            );
        }
        append_unique_paths(
            &mut paths,
            &mut seen,
            managed_variant_paths(planned.kind, planned.filename())?,
        );
    }
    Ok(paths)
}

/// Paths required to remove the selected entries and to prove that every
/// metadata candidate dependent is either live (and blocks removal) or stale.
pub fn managed_uninstall_observation_paths(
    manifest: &ObservedContentManifest,
    canonical_ids: &[CanonicalId],
) -> ContentResult<Vec<PortableRelativePath>> {
    let scope = managed_uninstall_scope(manifest.manifest(), canonical_ids);

    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for entry in scope.selected.into_iter().chain(scope.dependents) {
        append_unique_paths(
            &mut paths,
            &mut seen,
            managed_entry_variant_paths(entry)?,
        );
    }
    Ok(paths)
}


pub fn plan_managed_content_install(
    session: &ManagedContentPlanningSession,
    observed_manifest: ObservedContentManifest,
    files: &[PlannedFile],
) -> ContentResult<ManagedContentOperationProjection> {
    validate_install_plan(files)?;
    if files.is_empty() {
        return Err(ContentError::Invalid("the content install plan is empty".to_string()));
    }
    require_manifest_snapshot(session.manifest_bytes(), observed_manifest.snapshot.as_deref())?;
    require_planning_binding(session, &observed_manifest.binding)?;
    let observations = session.observations();
    let index = observation_index(&observations)?;
    let projection = project_install(observed_manifest.manifest, files, &index)?;
    build_projection(
        observed_manifest.snapshot,
        observed_manifest.binding,
        observations,
        projection,
    )
}

pub fn plan_managed_content_uninstall(
    session: &ManagedContentPlanningSession,
    observed_manifest: ObservedContentManifest,
    canonical_ids: &[CanonicalId],
) -> ContentResult<Option<ManagedContentOperationProjection>> {
    require_manifest_snapshot(session.manifest_bytes(), observed_manifest.snapshot.as_deref())?;
    require_planning_binding(session, &observed_manifest.binding)?;
    let observations = session.observations();
    let index = observation_index(&observations)?;
    let Some(projection) = project_uninstall(
        observed_manifest.manifest,
        canonical_ids,
        &index,
    )?
    else {
        return Ok(None);
    };
    build_projection(
        observed_manifest.snapshot,
        observed_manifest.binding,
        observations,
        projection,
    )
    .map(Some)
}

struct ProjectedMutation {
    results: HashMap<PortablePathKey, ManagedContentPathResult>,
    payloads: Vec<(ManagedContentPayloadId, TransferContract, Url)>,
    manifest: ContentManifest,
    affected_entries: usize,
}

fn project_install(
    mut manifest: ContentManifest,
    files: &[PlannedFile],
    observations: &HashMap<PortablePathKey, ManagedContentPathObservation>,
) -> ContentResult<ProjectedMutation> {
    let selected_ids = files
        .iter()
        .map(|planned| planned.canonical_id.clone())
        .collect::<HashSet<_>>();
    if selected_ids.len() != files.len() {
        return Err(provider_error(
            "the content plan contains the same project more than once",
        ));
    }

    let mut owners = HashMap::<PortablePathKey, &ManifestEntry>::new();
    for entry in manifest.entries() {
        for path in managed_entry_variant_paths(entry)? {
            if owners.insert(path.key(), entry).is_some() {
                return Err(ContentError::Invalid(
                    "the managed manifest has conflicting path ownership".to_string(),
                ));
            }
        }
    }

    let mut variants_by_id = HashMap::new();
    let mut future_owner = HashMap::<PortablePathKey, &PlannedFile>::new();
    for planned in files {
        let variants = managed_variant_paths(planned.kind, planned.filename())?;
        if variants.len() != 2 {
            return Err(provider_error("content is not installable as one managed file"));
        }
        for path in &variants {
            if future_owner.insert(path.key(), planned).is_some() {
                return Err(provider_error(
                    "multiple content projects resolve to the same destination",
                ));
            }
        }
        variants_by_id.insert(planned.canonical_id.clone(), variants);
    }

    let mut results = HashMap::new();
    let mut entries = Vec::with_capacity(files.len());
    let mut payloads = Vec::with_capacity(files.len());

    for (index, planned) in files.iter().enumerate() {
        let variants = variants_by_id
            .get(&planned.canonical_id)
            .expect("validated planned variants");
        let existing = manifest.find(&planned.canonical_id);
        let enabled = match existing {
            Some(entry) => observed_enabled_state(entry, observations)?.unwrap_or(entry.enabled()),
            None => true,
        };
        let target = &variants[usize::from(!enabled)];

        for path in variants {
            let observation = require_observation(observations, path)?;
            if let Some(owner) = owners.get(&path.key()) {
                let same_project = owner.canonical_id() == &planned.canonical_id;
                let owner_moves_away = selected_ids.contains(owner.canonical_id())
                    && variants_by_id
                        .get(owner.canonical_id())
                        .is_some_and(|future| !future.iter().any(|item| item.key() == path.key()));
                if !same_project && !owner_moves_away {
                    return Err(ContentError::Invalid(
                        "a content destination is owned by another project".to_string(),
                    ));
                }
                require_owner_or_absent(observation.state(), owner)?;
            } else if matches!(observation.state(), ManagedContentObservedState::Exact { .. }) {
                return Err(ContentError::Invalid(
                    "a content destination is occupied by unmanaged bytes".to_string(),
                ));
            }
        }

        if let Some(previous) = existing {
            for old_path in managed_entry_variant_paths(previous)? {
                let observation = require_observation(observations, &old_path)?;
                require_owner_or_absent(observation.state(), previous)?;
                if observed_matches_entry(observation.state(), previous)
                    && !future_owner.contains_key(&old_path.key())
                {
                    results.insert(old_path.key(), ManagedContentPathResult::Absent);
                }
            }
        }

        let payload_id = ManagedContentPayloadId::new(&format!("content-{index}"))
            .map_err(core_plan_error)?;
        results.insert(
            target.key(),
            ManagedContentPathResult::Download(payload_id.clone()),
        );
        for alternate in variants.iter().filter(|path| *path != target) {
            let observation = require_observation(observations, alternate)?;
            if matches!(observation.state(), ManagedContentObservedState::Exact { .. })
            {
                results.insert(alternate.key(), ManagedContentPathResult::Absent);
            }
        }

        let digests = ExpectedTransferDigests::from_hex(planned.sha1(), Some(planned.sha512()))
            .map_err(|_| provider_error("the provider returned an invalid content digest"))?;
        let size = NonZeroU64::new(planned.size())
            .ok_or_else(|| provider_error("the provider returned an invalid content size"))?;
        let contract = TransferContract::authenticated_exact(size, digests)
            .map_err(|_| provider_error("the provider returned an invalid content digest"))?;
        payloads.push((payload_id, contract, planned.download_url().clone()));

        let mut entry = ManifestEntry::managed_file(
            planned.canonical_id.clone(),
            planned.provider,
            planned.project_id.clone(),
            planned.version_id.clone(),
            planned.kind,
            planned.filename().clone(),
            Some(planned.sha512().to_string()),
            Some(planned.size()),
            planned.dependencies.clone(),
            planned.title.clone(),
        )?;
        entry.set_enabled(enabled);
        manifest.validate_provider_entry(&entry)?;
        entries.push(entry);
    }

    manifest.try_upsert_batch(entries).map_err(|_| {
        provider_error("content metadata conflicts in the managed manifest")
    })?;
    Ok(ProjectedMutation {
        results,
        payloads,
        manifest,
        affected_entries: files.len(),
    })
}

fn project_uninstall(
    mut manifest: ContentManifest,
    canonical_ids: &[CanonicalId],
    observations: &HashMap<PortablePathKey, ManagedContentPathObservation>,
) -> ContentResult<Option<ProjectedMutation>> {
    let scope = managed_uninstall_scope(&manifest, canonical_ids);
    let entries = scope.selected.iter().map(|entry| (*entry).clone()).collect::<Vec<_>>();
    if entries.is_empty() {
        return Ok(None);
    }
    for candidate in scope.dependents {
        if entry_is_live(candidate, observations)? {
            return Err(ContentError::Invalid(
                "content is required by another installed item".to_string(),
            ));
        }
    }

    let mut results = HashMap::new();
    for entry in &entries {
        for path in managed_entry_variant_paths(entry)? {
            let observation = require_observation(observations, &path)?;
            require_owner_or_absent(observation.state(), entry)?;
            if observed_matches_entry(observation.state(), entry) {
                results.insert(path.key(), ManagedContentPathResult::Absent);
            }
        }
        manifest.remove(entry.canonical_id());
    }
    Ok(Some(ProjectedMutation {
        results,
        payloads: Vec::new(),
        manifest,
        affected_entries: entries.len(),
    }))
}

struct ManagedUninstallScope<'a> {
    selected: Vec<&'a ManifestEntry>,
    dependents: Vec<&'a ManifestEntry>,
}

struct SelectedDependencyIndex {
    project_ids: HashSet<String>,
    version_ids: HashSet<String>,
}

fn managed_uninstall_scope<'a>(
    manifest: &'a ContentManifest,
    canonical_ids: &[CanonicalId],
) -> ManagedUninstallScope<'a> {
    let requested = canonical_ids.iter().collect::<HashSet<_>>();
    let selected = manifest
        .entries()
        .iter()
        .filter(|entry| requested.contains(entry.canonical_id()))
        .collect::<Vec<_>>();
    let index = SelectedDependencyIndex {
        project_ids: selected
            .iter()
            .map(|entry| entry.project_id().to_string())
            .collect(),
        version_ids: selected
            .iter()
            .map(|entry| entry.version_id().to_string())
            .collect(),
    };
    let dependents = manifest
        .entries()
        .iter()
        .filter(|entry| !requested.contains(entry.canonical_id()))
        .filter(|entry| {
            entry
                .dependencies()
                .iter()
                .any(|dependency| index.matches(dependency))
        })
        .collect();
    ManagedUninstallScope {
        selected,
        dependents,
    }
}

impl SelectedDependencyIndex {
    fn matches(&self, dependency: &ContentDependency) -> bool {
        if dependency.kind != DependencyKind::Required {
            return false;
        }
        match dependency.project_id.as_ref() {
            Some(project_id) => self.project_ids.contains(project_id),
            None => dependency
                .version_id
                .as_ref()
                .is_some_and(|version_id| self.version_ids.contains(version_id)),
        }
    }
}

fn build_projection(
    observed_manifest: Option<Box<[u8]>>,
    binding: ManagedContentPlanningBinding,
    observations: Vec<ManagedContentPathObservation>,
    projection: ProjectedMutation,
) -> ContentResult<ManagedContentOperationProjection> {
    if projection.results.len() > MAX_TRANSACTION_PATHS {
        return Err(ContentError::Invalid(
            "content operation has too many atomic filesystem effects".to_string(),
        ));
    }
    let mut effects = observations
        .iter()
        .filter_map(|observation| {
            projection.results.get(&observation.path().key()).map(|result| {
                ManagedContentPathMutation::new(
                    observation.path().clone(),
                    observation.state().clone(),
                    result.clone(),
                )
            })
        })
        .collect::<Vec<_>>();
    if effects.len() != projection.results.len() {
        return Err(ContentError::Invalid(
            "content projection contains an unobserved effect path".to_string(),
        ));
    }
    effects.sort_by(|left, right| left.path().as_str().cmp(right.path().as_str()));
    let sources = projection
        .payloads
        .iter()
        .map(|(id, _, url)| ManagedContentPayloadSource {
            id: id.clone(),
            url: url.clone(),
        })
        .collect();
    let payloads = projection
        .payloads
        .into_iter()
        .map(|(id, contract, _)| ManagedContentPayloadPlan::new(id, contract))
        .collect();
    Ok(ManagedContentOperationProjection {
        effects,
        payloads,
        sources,
        manifest_body: projection.manifest.encode_managed()?,
        observed_manifest,
        binding,
        affected_entries: projection.affected_entries,
    })
}

fn seal_projection(
    session: &ManagedContentTransactionSession,
    projection: ManagedContentOperationProjection,
) -> ContentResult<ManagedContentExecutionPlan> {
    require_manifest_snapshot(
        session.manifest_bytes(),
        projection.observed_manifest.as_deref(),
    )?;
    if !session.matches_planning_binding(&projection.binding) {
        return Err(ContentError::Invalid(
            "the managed content projection belongs to a different planning session".to_string(),
        ));
    }
    let observations = session.observations();
    if observations.len() != projection.effects.len() {
        return Err(ContentError::Invalid(
            "Core selected a different managed content effect set".to_string(),
        ));
    }
    let observed = observation_index(&observations)?;
    for effect in &projection.effects {
        let current = require_observation(&observed, effect.path())?;
        if current.state() != effect.observed() {
            return Err(ContentError::Invalid(
                "a managed content effect changed before sealing".to_string(),
            ));
        }
    }
    let manifest = session
        .bind_encoded_manifest(projection.manifest_body)
        .map_err(core_plan_error)?;
    let mutation = ManagedContentMutationPlan::new(
        &observations,
        projection.effects,
        projection.payloads,
        manifest,
    )
        .map_err(core_plan_error)?;
    Ok(ManagedContentExecutionPlan {
        mutation,
        sources: projection.sources,
        affected_entries: projection.affected_entries,
    })
}

fn derive_liveness(
    manifest: &ContentManifest,
    observations: &[ManagedContentPathObservation],
) -> ContentResult<LiveManagedContent> {
    let observations = observation_index(observations)?;
    let mut entries = HashMap::new();
    for entry in manifest.entries() {
        if entry_is_live(entry, &observations)? {
            entries.insert(entry.canonical_id().clone(), entry.clone());
        }
    }
    Ok(LiveManagedContent { entries })
}

fn entry_is_live(
    entry: &ManifestEntry,
    observations: &HashMap<PortablePathKey, ManagedContentPathObservation>,
) -> ContentResult<bool> {
    let paths = managed_entry_variant_paths(entry)?;
    if paths.is_empty() {
        return Ok(true);
    }
    let mut matching_variants = 0_usize;
    for path in paths {
        let observation = require_observation(observations, &path)?;
        if observed_matches_entry(observation.state(), entry) {
            matching_variants += 1;
        }
    }
    if matching_variants > 1 {
        return Err(ContentError::Invalid(
            "both managed content variants contain owned bytes".to_string(),
        ));
    }
    Ok(matching_variants == 1)
}

fn observed_enabled_state(
    entry: &ManifestEntry,
    observations: &HashMap<PortablePathKey, ManagedContentPathObservation>,
) -> ContentResult<Option<bool>> {
    let variants = managed_entry_variant_paths(entry)?;
    let [enabled, disabled] = variants.as_slice() else {
        return Ok(None);
    };
    let enabled_live = observed_matches_entry(
        require_observation(observations, enabled)?.state(),
        entry,
    );
    let disabled_live = observed_matches_entry(
        require_observation(observations, disabled)?.state(),
        entry,
    );
    if enabled_live && disabled_live {
        return Err(ContentError::Invalid(
            "both managed content variants contain owned bytes".to_string(),
        ));
    }
    Ok(enabled_live.then_some(true).or(disabled_live.then_some(false)))
}

fn require_owner_or_absent(
    state: &ManagedContentObservedState,
    owner: &ManifestEntry,
) -> ContentResult<()> {
    if matches!(state, ManagedContentObservedState::Absent) || observed_matches_entry(state, owner) {
        Ok(())
    } else {
        Err(ContentError::Invalid(
            "a managed content path no longer matches its ownership proof".to_string(),
        ))
    }
}

fn observed_matches_entry(state: &ManagedContentObservedState, entry: &ManifestEntry) -> bool {
    match (state, entry.size(), entry.sha512()) {
        (ManagedContentObservedState::Exact { size, sha512 }, Some(expected_size), Some(expected)) => {
            *size == expected_size && sha512.as_ref() == expected
        }
        _ => false,
    }
}

fn require_observation<'a>(
    observations: &'a HashMap<PortablePathKey, ManagedContentPathObservation>,
    path: &PortableRelativePath,
) -> ContentResult<&'a ManagedContentPathObservation> {
    let observation = observations.get(&path.key()).ok_or_else(|| {
        ContentError::Invalid("content planning is missing a required path observation".to_string())
    })?;
    if observation.path() != path {
        return Err(ContentError::Invalid(
            "content planning path identity is ambiguous".to_string(),
        ));
    }
    Ok(observation)
}

fn observation_index(
    observations: &[ManagedContentPathObservation],
) -> ContentResult<HashMap<PortablePathKey, ManagedContentPathObservation>> {
    let mut index = HashMap::with_capacity(observations.len());
    for observation in observations {
        if index
            .insert(observation.path().key(), observation.clone())
            .is_some()
        {
            return Err(ContentError::Invalid(
                "content planning contains a duplicate path observation".to_string(),
            ));
        }
    }
    Ok(index)
}

fn append_unique_paths(
    paths: &mut Vec<PortableRelativePath>,
    seen: &mut HashSet<PortablePathKey>,
    additions: Vec<PortableRelativePath>,
) {
    for path in additions {
        if seen.insert(path.key()) {
            paths.push(path);
        }
    }
}

fn require_manifest_snapshot(
    current: Option<&[u8]>,
    expected: Option<&[u8]>,
) -> ContentResult<()> {
    if current != expected {
        return Err(ContentError::Invalid(
            "the managed content projection belongs to a different manifest snapshot".to_string(),
        ));
    }
    Ok(())
}

fn require_planning_binding(
    session: &ManagedContentPlanningSession,
    binding: &ManagedContentPlanningBinding,
) -> ContentResult<()> {
    if !session.matches_planning_binding(binding) {
        return Err(ContentError::Invalid(
            "the managed content data belongs to a different planning session".to_string(),
        ));
    }
    Ok(())
}

fn provider_error(message: &str) -> ContentError {
    ContentError::ProviderMetadataInvalid(message.to_string())
}

fn core_plan_error(error: impl std::fmt::Debug) -> ContentError {
    ContentError::Invalid(format!("managed content plan was refused: {error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentKind, FileRef, ProviderId};

    fn managed_entry(project: &str) -> ManifestEntry {
        ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, project),
            ProviderId::Modrinth,
            project.to_string(),
            format!("{project}-v1"),
            ContentKind::Mod,
            &FileRef {
                url: format!("https://example.invalid/{project}.jar"),
                filename: format!("{project}.jar"),
                sha1: None,
                sha512: Some("a".repeat(128)),
                size: Some(1),
                primary: true,
            },
            Vec::new(),
            None,
        )
        .expect("managed entry")
    }

    #[test]
    fn explicit_liveness_requires_full_manifest_entry_equality() {
        let entry = managed_entry("project");
        let live = LiveManagedContent::from_entries(std::iter::once(&entry));
        let mut changed = entry.clone();
        changed.set_enabled(false);

        assert!(live.contains(&entry));
        assert!(!live.contains(&changed));
    }

    #[test]
    fn explicit_liveness_retains_provenance_entries() {
        let provenance = ManifestEntry::provenance(
            CanonicalId::for_project(ProviderId::Modrinth, "pack"),
            ProviderId::Modrinth,
            "pack".to_string(),
            "pack-v1".to_string(),
            Some("Pack".to_string()),
        )
        .expect("pack provenance");
        let live = LiveManagedContent::from_entries(std::iter::once(&provenance));

        assert!(live.contains(&provenance));
    }

    #[test]
    fn manifest_snapshot_comparison_rejects_mixed_planning_flows() {
        assert!(require_manifest_snapshot(None, None).is_ok());
        assert!(require_manifest_snapshot(Some(b"one"), Some(b"one")).is_ok());
        assert!(require_manifest_snapshot(Some(b"one"), Some(b"two")).is_err());
        assert!(require_manifest_snapshot(Some(b"one"), None).is_err());
    }

    #[test]
    fn unique_path_projection_retains_first_seen_order() {
        let first = PortableRelativePath::new_exact("mods/first.jar").expect("first path");
        let second = PortableRelativePath::new_exact("mods/second.jar").expect("second path");
        let mut paths = Vec::new();
        let mut seen = HashSet::new();

        append_unique_paths(
            &mut paths,
            &mut seen,
            vec![first.clone(), second.clone(), first],
        );

        assert_eq!(paths, vec![
            PortableRelativePath::new_exact("mods/first.jar").expect("first path"),
            second,
        ]);
    }

    #[test]
    fn selected_dependency_index_preserves_project_and_version_only_semantics() {
        let index = SelectedDependencyIndex {
            project_ids: HashSet::from(["selected".to_string()]),
            version_ids: HashSet::from(["selected-v1".to_string()]),
        };
        let dependency = |project_id: Option<&str>, version_id: Option<&str>, kind| {
            ContentDependency {
                project_id: project_id.map(str::to_string),
                version_id: version_id.map(str::to_string),
                kind,
            }
        };

        assert!(index.matches(&dependency(
            Some("selected"),
            Some("other-version"),
            DependencyKind::Required,
        )));
        assert!(index.matches(&dependency(
            None,
            Some("selected-v1"),
            DependencyKind::Required,
        )));
        assert!(!index.matches(&dependency(
            Some("other"),
            Some("selected-v1"),
            DependencyKind::Required,
        )));
        assert!(!index.matches(&dependency(
            Some("selected"),
            None,
            DependencyKind::Optional,
        )));
    }
}
