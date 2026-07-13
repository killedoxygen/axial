use crate::artifact_path::ArtifactRelativePath;
use crate::download::{
    DownloadJob, ExactLibraryDownloadProof, LibraryArtifactPlan,
    MaterializedSelectedArtifactSource, library_artifact_plans_for,
};
use crate::launch::{Library, VersionJson, effective_java_version_for, library_merge_key};
use crate::loaders::providers::ProfileInstallProof;
use crate::loaders::{LoaderProfileFragment, types::LoaderComponentId};
use crate::rules::Environment;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedLibraryKind {
    Library,
    Native,
}

struct SealedExactLibraryDeclaration {
    path: ArtifactRelativePath,
    kind: SealedLibraryKind,
    sha1: [u8; 20],
    size: u64,
}

pub(crate) struct SealedExactLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    structure: LibraryStructure,
}

pub(crate) struct PendingExactLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    selected: BTreeMap<ArtifactRelativePath, LibraryArtifactPlan>,
    structure: LibraryStructure,
}

pub(crate) struct PendingStreamedLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    selected: BTreeMap<ArtifactRelativePath, LibraryArtifactPlan>,
    structure: LibraryStructure,
}

enum LibraryStructure {
    Vanilla(Box<VanillaLibraryStructure>),
    Profile(Box<ProfileLibraryStructure>),
}

struct VanillaLibraryStructure {
    version: VersionJson,
    environment: Environment,
}

struct ProfileLibraryStructure {
    fragment: LoaderProfileFragment,
    environment: Environment,
}

pub(crate) struct AuthenticatedVanillaLibraryDeclarationSource {
    declarations: PendingExactLibraryDeclarations,
    bytes: Arc<[u8]>,
    size: u64,
    sha1: [u8; 20],
}

impl AuthenticatedVanillaLibraryDeclarationSource {
    pub(crate) fn into_parts(self) -> (PendingExactLibraryDeclarations, Arc<[u8]>, u64, [u8; 20]) {
        (self.declarations, self.bytes, self.size, self.sha1)
    }
}

pub(crate) struct ClassifiedLibraryDownload {
    pub(crate) job: DownloadJob,
    pub(crate) acquisition: LibraryAcquisition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryAcquisition {
    ExactDeclaration,
    FreshStream,
}

struct StreamedExactLibraryProof {
    path: ArtifactRelativePath,
    kind: SealedLibraryKind,
    sha1: [u8; 20],
    size: u64,
    provider_url: String,
    expected: crate::download::ExpectedIntegrity,
}

impl StreamedExactLibraryProof {
    fn from_authenticated_stream(
        path: ArtifactRelativePath,
        kind: SealedLibraryKind,
        provider_url: String,
        expected: crate::download::ExpectedIntegrity,
        sha1: [u8; 20],
        size: u64,
    ) -> Result<Self, SealedLibraryDeclarationError> {
        if size == 0 {
            return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
        }
        Ok(Self {
            path,
            kind,
            sha1,
            size,
            provider_url,
            expected,
        })
    }
}

impl SealedExactLibraryDeclarations {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(
        &self,
        path: &ArtifactRelativePath,
    ) -> Option<(SealedLibraryKind, [u8; 20], u64)> {
        self.entries
            .get(path)
            .map(|entry| (entry.kind, entry.sha1, entry.size))
    }

    pub(crate) fn matches_version(&self, version: &VersionJson, environment: &Environment) -> bool {
        matches!(
            &self.structure,
            LibraryStructure::Vanilla(contract)
                if contract.version == *version && contract.environment == *environment
        )
    }

    pub(crate) fn profile_contract(&self) -> Option<(&LoaderProfileFragment, &Environment)> {
        match &self.structure {
            LibraryStructure::Profile(contract) => {
                Some((&contract.fragment, &contract.environment))
            }
            LibraryStructure::Vanilla(_) => None,
        }
    }
}

impl PendingExactLibraryDeclarations {
    pub(crate) fn profile_plan_inputs(&self) -> Option<(&[Library], &Environment)> {
        match &self.structure {
            LibraryStructure::Profile(contract) => {
                Some((&contract.fragment.libraries, &contract.environment))
            }
            LibraryStructure::Vanilla(_) => None,
        }
    }

    pub(crate) fn classify_jobs(
        self,
        libraries_root: &Path,
        jobs: Vec<DownloadJob>,
    ) -> Result<
        (
            PendingStreamedLibraryDeclarations,
            Vec<ClassifiedLibraryDownload>,
        ),
        SealedLibraryDeclarationError,
    > {
        let mut jobs = jobs.into_iter().try_fold(
            BTreeMap::new(),
            |mut jobs, job| -> Result<_, SealedLibraryDeclarationError> {
                if jobs.insert(job.relative_path.clone(), job).is_some() {
                    return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
                }
                Ok(jobs)
            },
        )?;
        if jobs.len() != self.selected.len() {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        let mut classified = Vec::with_capacity(self.selected.len());
        for (path, plan) in &self.selected {
            let job = jobs
                .remove(path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            if job.is_native != plan.is_native {
                return Err(SealedLibraryDeclarationError::KindDrift);
            }
            if plan.source_url.as_deref() != Some(job.url.as_str())
                || job.expected != plan.expected
                || job.path != path.join_under(libraries_root)
            {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            classified.push(ClassifiedLibraryDownload {
                acquisition: if self.entries.contains_key(path) {
                    LibraryAcquisition::ExactDeclaration
                } else {
                    LibraryAcquisition::FreshStream
                },
                job,
            });
        }
        if !jobs.is_empty() {
            return Err(SealedLibraryDeclarationError::ExtraDeclaration);
        }
        Ok((
            PendingStreamedLibraryDeclarations {
                entries: self.entries,
                selected: self.selected,
                structure: self.structure,
            },
            classified,
        ))
    }
}

impl PendingStreamedLibraryDeclarations {
    pub(crate) fn seal_streamed(
        self,
        streamed: Vec<ExactLibraryDownloadProof>,
    ) -> Result<SealedExactLibraryDeclarations, SealedLibraryDeclarationError> {
        let streamed = streamed
            .into_iter()
            .map(|proof| {
                let (path, is_native, provider_url, expected, size, sha1) = proof.into_parts();
                StreamedExactLibraryProof::from_authenticated_stream(
                    path,
                    if is_native {
                        SealedLibraryKind::Native
                    } else {
                        SealedLibraryKind::Library
                    },
                    provider_url,
                    expected,
                    sha1,
                    size,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        merge_selected_library_declarations(
            self.selected.into_values().collect(),
            AuthenticatedExactLibraryDeclarations {
                entries: self.entries,
            },
            streamed,
            self.structure,
        )
    }
}

pub(crate) fn seal_vanilla_exact_library_declarations(
    source: MaterializedSelectedArtifactSource,
    resolved: &VersionJson,
    environment: &Environment,
) -> Result<AuthenticatedVanillaLibraryDeclarationSource, SealedLibraryDeclarationError> {
    let mut authenticated = serde_json::from_slice::<VersionJson>(source.bytes())
        .map_err(|_| SealedLibraryDeclarationError::AncestorMismatch)?;
    if authenticated.asset_index.id.is_empty() && !authenticated.assets.is_empty() {
        authenticated
            .asset_index
            .id
            .clone_from(&authenticated.assets);
    }
    authenticated.java_version = effective_java_version_for(
        &authenticated.id,
        &authenticated.kind,
        &authenticated.java_version,
    );
    if authenticated != *resolved {
        return Err(SealedLibraryDeclarationError::AncestorMismatch);
    }
    let plans = library_artifact_plans_for(&resolved.libraries, environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    let declarations = seal_exact_plan_subset(
        plans,
        LibraryStructure::Vanilla(Box::new(VanillaLibraryStructure {
            version: authenticated,
            environment: environment.clone(),
        })),
    )?;
    let (bytes, size, sha1) = source.into_parts();
    Ok(AuthenticatedVanillaLibraryDeclarationSource {
        declarations,
        bytes,
        size,
        sha1,
    })
}

pub(crate) fn seal_profile_exact_library_declarations(
    mut fragment: LoaderProfileFragment,
    proof: ProfileInstallProof,
    component: LoaderComponentId,
    environment: &Environment,
) -> Result<PendingExactLibraryDeclarations, SealedLibraryDeclarationError> {
    if !matches!(
        component,
        LoaderComponentId::Fabric | LoaderComponentId::Quilt
    ) || proof.required_libraries().is_empty()
    {
        return Err(SealedLibraryDeclarationError::AncestorMismatch);
    }
    let (canonical_profile_id, inherits_from, client_main_class) = proof.identity();
    if canonical_profile_id != fragment.id
        || inherits_from != fragment.inherits_from
        || client_main_class != fragment.main_class
    {
        return Err(SealedLibraryDeclarationError::AncestorMismatch);
    }
    reject_profile_library_shadowing(&fragment.libraries)?;
    let mut required_coordinates = BTreeMap::new();
    let mut required_keys = BTreeMap::new();
    for required in proof.required_libraries() {
        if required_coordinates
            .insert(required.coordinate(), ())
            .is_some()
            || required_keys
                .insert(library_merge_key(required.coordinate()), ())
                .is_some()
        {
            return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
        }
    }
    strip_profile_integrity(&mut fragment.libraries);

    let mut exact = AuthenticatedExactLibraryDeclarations::empty();
    for required in proof.required_libraries() {
        if required.has_partial_integrity() {
            return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
        }
        let matching = fragment
            .libraries
            .iter()
            .enumerate()
            .filter(|(_, library)| library.name == required.coordinate())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(SealedLibraryDeclarationError::AncestorMismatch);
        }
        let index = matching[0];
        let primary = library_artifact_plans_for(
            std::slice::from_ref(&fragment.libraries[index]),
            environment,
        )
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?
        .into_iter()
        .filter(|plan| !plan.is_native)
        .collect::<Vec<_>>();
        if primary.len() != 1 {
            return Err(SealedLibraryDeclarationError::InvalidSelectedPlan);
        }
        let plan = &primary[0];
        let integrity = required
            .exact_integrity()
            .map(|(sha1, size)| {
                if size == 0 {
                    return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
                }
                Ok((decode_sha1(sha1)?, size))
            })
            .transpose()?;
        if component == LoaderComponentId::Fabric && integrity.is_some() {
            return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
        }
        let Some((sha1, size)) = integrity else {
            continue;
        };
        author_library_integrity(&mut fragment.libraries[index], plan, sha1, size)?;
        exact.insert(SealedExactLibraryDeclaration {
            path: plan.relative_path.clone(),
            kind: SealedLibraryKind::Library,
            sha1,
            size,
        })?;
    }

    let mut structural_paths = BTreeMap::new();
    for library in &fragment.libraries {
        for plan in library_artifact_plans_for(std::slice::from_ref(library), environment)
            .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?
        {
            validate_authoring_slot(library, &plan)?;
            if structural_paths
                .insert(plan.relative_path.clone(), ())
                .is_some()
            {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
        }
    }
    let plans = library_artifact_plans_for(&fragment.libraries, environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    let plan_count = plans.len();
    let selected = plans
        .into_iter()
        .map(|plan| (plan.relative_path.clone(), plan))
        .collect::<BTreeMap<_, _>>();
    if selected.len() != plan_count
        || selected.len() < exact.entries.len()
        || exact
            .entries
            .keys()
            .any(|path| !selected.contains_key(path))
    {
        return Err(SealedLibraryDeclarationError::ContractDrift);
    }
    let structure = LibraryStructure::Profile(Box::new(ProfileLibraryStructure {
        fragment,
        environment: environment.clone(),
    }));
    Ok(PendingExactLibraryDeclarations {
        entries: exact.entries,
        selected,
        structure,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedLibraryDeclarationError {
    AncestorMismatch,
    InvalidSelectedPlan,
    InvalidExactDeclaration,
    MissingDeclaration,
    ExtraDeclaration,
    DuplicateDeclaration,
    KindDrift,
    ContractDrift,
    MissingStreamSource,
}

struct AuthenticatedExactLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
}

impl AuthenticatedExactLibraryDeclarations {
    fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    fn insert(
        &mut self,
        entry: SealedExactLibraryDeclaration,
    ) -> Result<(), SealedLibraryDeclarationError> {
        if entry.size == 0 {
            return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
        }
        if self.entries.insert(entry.path.clone(), entry).is_some() {
            return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
        }
        Ok(())
    }
}

fn merge_selected_library_declarations(
    plans: Vec<LibraryArtifactPlan>,
    mut exact: AuthenticatedExactLibraryDeclarations,
    streamed: Vec<StreamedExactLibraryProof>,
    structure: LibraryStructure,
) -> Result<SealedExactLibraryDeclarations, SealedLibraryDeclarationError> {
    let mut selected = BTreeMap::new();
    for plan in plans {
        if selected.insert(plan.relative_path.clone(), plan).is_some() {
            return Err(SealedLibraryDeclarationError::InvalidSelectedPlan);
        }
    }
    let mut streamed = streamed
        .into_iter()
        .map(|proof| (proof.path.clone(), proof))
        .try_fold(BTreeMap::new(), |mut proofs, (path, proof)| {
            if proofs.insert(path, proof).is_some() {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
            Ok(proofs)
        })?;
    let mut complete = BTreeMap::new();
    for (path, plan) in selected {
        let selected_kind = kind_for_plan(&plan);
        let entry = match (exact.entries.remove(&path), streamed.remove(&path)) {
            (Some(_), Some(_)) => {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
            (Some(entry), None) => entry,
            (None, Some(proof)) => {
                if plan.source_url.as_ref() != Some(&proof.provider_url)
                    || plan.expected != proof.expected
                {
                    return Err(SealedLibraryDeclarationError::MissingStreamSource);
                }
                SealedExactLibraryDeclaration {
                    path: proof.path,
                    kind: proof.kind,
                    sha1: proof.sha1,
                    size: proof.size,
                }
            }
            (None, None) => return Err(SealedLibraryDeclarationError::MissingDeclaration),
        };
        if entry.path != path || entry.kind != selected_kind {
            return Err(SealedLibraryDeclarationError::KindDrift);
        }
        validate_plan_contract(&plan, entry.sha1, entry.size)?;
        complete.insert(path, entry);
    }
    if !exact.entries.is_empty() || !streamed.is_empty() {
        return Err(SealedLibraryDeclarationError::ExtraDeclaration);
    }
    let structure = match structure {
        LibraryStructure::Vanilla(contract) => LibraryStructure::Vanilla(contract),
        LibraryStructure::Profile(mut contract) => {
            author_profile_library_integrity(
                &mut contract.fragment.libraries,
                &complete,
                &contract.environment,
            )?;
            LibraryStructure::Profile(contract)
        }
    };
    Ok(SealedExactLibraryDeclarations {
        entries: complete,
        structure,
    })
}

fn seal_exact_plan_subset(
    plans: Vec<LibraryArtifactPlan>,
    structure: LibraryStructure,
) -> Result<PendingExactLibraryDeclarations, SealedLibraryDeclarationError> {
    let mut selected = BTreeMap::new();
    let mut exact = AuthenticatedExactLibraryDeclarations::empty();
    for plan in plans {
        if plan.expected.size.is_some_and(|size| size > 0)
            && plan.expected.sha1.as_deref().is_some()
        {
            let size = plan.expected.size.expect("checked exact size");
            let sha1 = decode_sha1(plan.expected.sha1.as_deref().expect("checked exact digest"))?;
            exact.insert(SealedExactLibraryDeclaration {
                path: plan.relative_path.clone(),
                kind: kind_for_plan(&plan),
                sha1,
                size,
            })?;
        }
        if selected.insert(plan.relative_path.clone(), plan).is_some() {
            return Err(SealedLibraryDeclarationError::InvalidSelectedPlan);
        }
    }
    Ok(PendingExactLibraryDeclarations {
        entries: exact.entries,
        selected,
        structure,
    })
}

fn reject_profile_library_shadowing(
    libraries: &[Library],
) -> Result<(), SealedLibraryDeclarationError> {
    let mut keys = BTreeMap::new();
    for library in libraries {
        let key = library_merge_key(&library.name);
        if key.is_empty() || keys.insert(key, ()).is_some() {
            return Err(SealedLibraryDeclarationError::InvalidSelectedPlan);
        }
    }
    Ok(())
}

fn strip_profile_integrity(libraries: &mut [Library]) {
    for library in libraries {
        library.sha1.clear();
        library.sha256.clear();
        library.checksums.clear();
        library.size = 0;
        if let Some(downloads) = library.downloads.as_mut() {
            if let Some(artifact) = downloads.artifact.as_mut() {
                artifact.sha1.clear();
                artifact.size = 0;
            }
            for artifact in downloads.classifiers.values_mut() {
                artifact.sha1.clear();
                artifact.size = 0;
            }
        }
    }
}

fn validate_authoring_slot(
    library: &Library,
    plan: &LibraryArtifactPlan,
) -> Result<(), SealedLibraryDeclarationError> {
    let Some(downloads) = library.downloads.as_ref() else {
        return if plan.is_native {
            Err(SealedLibraryDeclarationError::InvalidSelectedPlan)
        } else {
            Ok(())
        };
    };
    let mut matches = usize::from(downloads.artifact.as_ref().is_some_and(|artifact| {
        ArtifactRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
    }));
    matches += downloads
        .classifiers
        .values()
        .filter(|artifact| {
            ArtifactRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
        })
        .count();
    let top_level_only = !plan.is_native && downloads.artifact.is_none();
    if matches == usize::from(!top_level_only) {
        Ok(())
    } else {
        Err(SealedLibraryDeclarationError::InvalidSelectedPlan)
    }
}

fn author_profile_library_integrity(
    libraries: &mut [Library],
    entries: &BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    environment: &Environment,
) -> Result<(), SealedLibraryDeclarationError> {
    for library in libraries.iter_mut() {
        let plans = library_artifact_plans_for(std::slice::from_ref(library), environment)
            .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
        for plan in plans {
            let entry = entries
                .get(&plan.relative_path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            if entry.kind != kind_for_plan(&plan) {
                return Err(SealedLibraryDeclarationError::KindDrift);
            }
            author_library_integrity(library, &plan, entry.sha1, entry.size)?;
        }
    }
    let plans = library_artifact_plans_for(libraries, environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    if plans.len() != entries.len() {
        return Err(SealedLibraryDeclarationError::ContractDrift);
    }
    for plan in plans {
        let entry = entries
            .get(&plan.relative_path)
            .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
        if entry.kind != kind_for_plan(&plan)
            || plan.expected.size != Some(entry.size)
            || plan
                .expected
                .sha1
                .as_deref()
                .is_none_or(|sha1| !decode_sha1(sha1).is_ok_and(|digest| digest == entry.sha1))
        {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
    }
    Ok(())
}

fn author_library_integrity(
    library: &mut Library,
    plan: &LibraryArtifactPlan,
    sha1: [u8; 20],
    size: u64,
) -> Result<(), SealedLibraryDeclarationError> {
    let size =
        i64::try_from(size).map_err(|_| SealedLibraryDeclarationError::InvalidExactDeclaration)?;
    let digest = encode_sha1(sha1);
    if !plan.is_native {
        library.sha1.clone_from(&digest);
        library.size = size;
    }
    if let Some(downloads) = library.downloads.as_mut() {
        let mut matches = 0;
        if let Some(artifact) = downloads.artifact.as_mut()
            && ArtifactRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
        {
            artifact.sha1.clone_from(&digest);
            artifact.size = size;
            matches += 1;
        }
        for artifact in downloads.classifiers.values_mut() {
            if ArtifactRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path) {
                artifact.sha1.clone_from(&digest);
                artifact.size = size;
                matches += 1;
            }
        }
        let top_level_only = !plan.is_native && downloads.artifact.is_none();
        if matches != usize::from(!top_level_only) {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
    } else if plan.is_native {
        return Err(SealedLibraryDeclarationError::ContractDrift);
    }
    Ok(())
}

fn encode_sha1(sha1: [u8; 20]) -> String {
    let mut encoded = String::with_capacity(40);
    use std::fmt::Write as _;
    for byte in sha1 {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

fn decode_sha1(value: &str) -> Result<[u8; 20], SealedLibraryDeclarationError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
    }
    let mut digest = [0_u8; 20];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or(SealedLibraryDeclarationError::InvalidExactDeclaration)?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or(SealedLibraryDeclarationError::InvalidExactDeclaration)?;
        digest[index] = ((high << 4) | low) as u8;
    }
    Ok(digest)
}

#[cfg(test)]
pub(crate) fn seal_vanilla_library_declarations_for_test(
    version: &VersionJson,
    environment: &Environment,
    streamed: Vec<ExactLibraryDownloadProof>,
) -> Result<SealedExactLibraryDeclarations, SealedLibraryDeclarationError> {
    let plans = library_artifact_plans_for(&version.libraries, environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    let pending = seal_exact_plan_subset(
        plans,
        LibraryStructure::Vanilla(Box::new(VanillaLibraryStructure {
            version: version.clone(),
            environment: environment.clone(),
        })),
    )?;
    PendingStreamedLibraryDeclarations {
        entries: pending.entries,
        selected: pending.selected,
        structure: pending.structure,
    }
    .seal_streamed(streamed)
}

fn kind_for_plan(plan: &LibraryArtifactPlan) -> SealedLibraryKind {
    if plan.is_native {
        SealedLibraryKind::Native
    } else {
        SealedLibraryKind::Library
    }
}

fn validate_plan_contract(
    plan: &LibraryArtifactPlan,
    sha1: [u8; 20],
    size: u64,
) -> Result<(), SealedLibraryDeclarationError> {
    if size == 0 || plan.expected.size.is_some_and(|expected| expected != size) {
        return Err(SealedLibraryDeclarationError::ContractDrift);
    }
    if let Some(expected) = plan.expected.sha1.as_deref() {
        let mut observed = String::with_capacity(40);
        use std::fmt::Write as _;
        for byte in sha1 {
            let _ = write!(&mut observed, "{byte:02x}");
        }
        if !expected.eq_ignore_ascii_case(&observed) {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::ExpectedIntegrity;
    use crate::launch::{LibraryArtifact, LibraryDownload};
    use crate::loaders::providers::{ProfileInstallProof, ProfileLibraryProof};
    use std::collections::HashMap;
    use std::path::Path;

    fn plan(path: &str, exact: bool, source: bool, native: bool) -> LibraryArtifactPlan {
        LibraryArtifactPlan {
            relative_path: ArtifactRelativePath::new(path).unwrap(),
            source_url: source.then(|| "https://example.invalid/library.jar".to_string()),
            name: path.to_string(),
            expected: if exact {
                ExpectedIntegrity {
                    size: Some(7),
                    sha1: Some("0101010101010101010101010101010101010101".to_string()),
                }
            } else {
                ExpectedIntegrity::default()
            },
            is_native: native,
        }
    }

    fn exact(path: &str, native: bool) -> SealedExactLibraryDeclaration {
        SealedExactLibraryDeclaration {
            path: ArtifactRelativePath::new(path).unwrap(),
            kind: if native {
                SealedLibraryKind::Native
            } else {
                SealedLibraryKind::Library
            },
            sha1: [1; 20],
            size: 7,
        }
    }

    fn stream(path: &str, native: bool) -> StreamedExactLibraryProof {
        StreamedExactLibraryProof::from_authenticated_stream(
            ArtifactRelativePath::new(path).unwrap(),
            if native {
                SealedLibraryKind::Native
            } else {
                SealedLibraryKind::Library
            },
            "https://example.invalid/library.jar".to_string(),
            ExpectedIntegrity::default(),
            [1; 20],
            7,
        )
        .unwrap()
    }

    fn version_contract() -> LibraryStructure {
        LibraryStructure::Vanilla(Box::new(VanillaLibraryStructure {
            version: serde_json::from_str(r#"{"id":"fixture"}"#).expect("fixture version contract"),
            environment: crate::rules::default_environment(),
        }))
    }

    fn profile_library(coordinate: &str, path: &str, raw_sha1: &str, raw_size: i64) -> Library {
        Library {
            name: coordinate.to_string(),
            sha1: raw_sha1.to_string(),
            sha256: "untrusted-sha256".to_string(),
            checksums: vec![raw_sha1.to_string()],
            size: raw_size,
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: path.to_string(),
                    sha1: raw_sha1.to_string(),
                    size: raw_size,
                    url: "https://example.invalid/library.jar".to_string(),
                }),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        }
    }

    fn profile_proof(
        coordinate: &str,
        sha1: Option<&str>,
        size: Option<u64>,
    ) -> ProfileInstallProof {
        ProfileInstallProof::from_test(
            "profile-id".to_string(),
            "1.21.5".to_string(),
            "example.Main".to_string(),
            vec![ProfileLibraryProof::from_test(
                coordinate.to_string(),
                sha1.map(str::to_string),
                size,
            )],
        )
    }

    fn profile_fragment(libraries: Vec<Library>) -> LoaderProfileFragment {
        LoaderProfileFragment {
            id: "profile-id".to_string(),
            inherits_from: "1.21.5".to_string(),
            kind: "release".to_string(),
            main_class: "example.Main".to_string(),
            libraries,
            ..LoaderProfileFragment::default()
        }
    }

    fn jobs_for(libraries: &[Library], environment: &Environment) -> Vec<DownloadJob> {
        let root = Path::new("/managed/libraries");
        library_artifact_plans_for(libraries, environment)
            .unwrap()
            .into_iter()
            .map(|plan| DownloadJob {
                relative_path: plan.relative_path.clone(),
                path: plan.relative_path.join_under(root),
                url: plan.source_url.expect("profile source"),
                name: plan.name,
                expected: plan.expected,
                is_native: plan.is_native,
            })
            .collect()
    }

    fn streamed_proof(job: DownloadJob, sha1: [u8; 20], size: u64) -> ExactLibraryDownloadProof {
        ExactLibraryDownloadProof::new_bound_for_test(
            job.relative_path,
            job.is_native,
            job.url,
            job.expected,
            size,
            sha1,
        )
    }

    #[test]
    fn consumes_exact_and_streamed_subsets_one_to_one() {
        let mut declarations = AuthenticatedExactLibraryDeclarations::empty();
        declarations.insert(exact("exact.jar", false)).unwrap();

        let sealed = merge_selected_library_declarations(
            vec![
                plan("exact.jar", true, false, false),
                plan("stream.jar", false, true, true),
            ],
            declarations,
            vec![stream("stream.jar", true)],
            version_contract(),
        )
        .unwrap();

        assert_eq!(sealed.len(), 2);
    }

    #[test]
    fn exact_declaration_does_not_require_artifact_url() {
        let mut declarations = AuthenticatedExactLibraryDeclarations::empty();
        declarations.insert(exact("exact.jar", false)).unwrap();

        assert!(
            merge_selected_library_declarations(
                vec![plan("exact.jar", true, false, false)],
                declarations,
                Vec::new(),
                version_contract(),
            )
            .is_ok()
        );
    }

    #[test]
    fn incomplete_declaration_requires_artifact_url() {
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("stream.jar", false, false, false)],
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![stream("stream.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::MissingStreamSource)
        ));
    }

    #[test]
    fn rejects_missing_extra_duplicate_kind_and_contract_drift() {
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("missing.jar", false, true, false)],
                AuthenticatedExactLibraryDeclarations::empty(),
                Vec::new(),
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::MissingDeclaration)
        ));
        assert!(matches!(
            merge_selected_library_declarations(
                Vec::new(),
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![stream("extra.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::ExtraDeclaration)
        ));
        let mut exact_and_stream = AuthenticatedExactLibraryDeclarations::empty();
        exact_and_stream
            .insert(exact("duplicate.jar", false))
            .unwrap();
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("duplicate.jar", true, true, false)],
                exact_and_stream,
                vec![stream("duplicate.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("native.jar", false, true, true)],
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![stream("native.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::KindDrift)
        ));
        let drift = StreamedExactLibraryProof::from_authenticated_stream(
            ArtifactRelativePath::new("drift.jar").unwrap(),
            SealedLibraryKind::Library,
            "https://example.invalid/library.jar".to_string(),
            ExpectedIntegrity {
                size: Some(7),
                sha1: Some("0101010101010101010101010101010101010101".to_string()),
            },
            [2; 20],
            7,
        )
        .unwrap();
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("drift.jar", true, true, false)],
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![drift],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));
    }

    #[test]
    fn preclassification_rejects_exact_job_url_expected_and_destination_drift() {
        let root = std::path::PathBuf::from("/managed/libraries");
        let selected = plan("exact.jar", true, true, false);
        let job = || DownloadJob {
            relative_path: selected.relative_path.clone(),
            path: selected.relative_path.join_under(&root),
            url: selected.source_url.clone().unwrap(),
            name: selected.name.clone(),
            expected: selected.expected.clone(),
            is_native: false,
        };
        let pending =
            || seal_exact_plan_subset(vec![selected.clone()], version_contract()).unwrap();

        let (_, classified) = pending().classify_jobs(&root, vec![job()]).unwrap();
        assert_eq!(classified.len(), 1);
        assert_eq!(
            classified[0].acquisition,
            LibraryAcquisition::ExactDeclaration
        );

        let mut url_drift = job();
        url_drift.url = "https://other.invalid/library.jar".to_string();
        assert!(matches!(
            pending().classify_jobs(&root, vec![url_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let mut expected_drift = job();
        expected_drift.expected.size = Some(8);
        assert!(matches!(
            pending().classify_jobs(&root, vec![expected_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let mut destination_drift = job();
        destination_drift.path = root.join("other.jar");
        assert!(matches!(
            pending().classify_jobs(&root, vec![destination_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));
    }

    #[test]
    fn fabric_strips_bogus_integrity_and_streams_every_selected_path() {
        let environment = crate::rules::default_environment();
        let coordinate = "net.fabricmc:fabric-loader:0.16.14";
        let path = "net/fabricmc/fabric-loader/0.16.14/fabric-loader-0.16.14.jar";
        let pending = seal_profile_exact_library_declarations(
            profile_fragment(vec![profile_library(
                coordinate,
                path,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                99,
            )]),
            profile_proof(coordinate, None, None),
            LoaderComponentId::Fabric,
            &environment,
        )
        .expect("Fabric declarations");
        let (libraries, sealed_environment) = pending.profile_plan_inputs().unwrap();
        assert!(libraries[0].sha1.is_empty());
        assert!(libraries[0].sha256.is_empty());
        assert!(libraries[0].checksums.is_empty());
        assert_eq!(libraries[0].size, 0);
        let jobs = jobs_for(libraries, sealed_environment);
        let (pending, classified) = pending
            .classify_jobs(Path::new("/managed/libraries"), jobs)
            .expect("classified Fabric jobs");
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].acquisition, LibraryAcquisition::FreshStream);

        let sealed = pending
            .seal_streamed(vec![streamed_proof(
                classified.into_iter().next().unwrap().job,
                [0xbb; 20],
                17,
            )])
            .expect("observed Fabric declaration");
        let authored = &sealed.profile_contract().unwrap().0.libraries;
        assert_eq!(authored[0].sha1, encode_sha1([0xbb; 20]));
        assert_eq!(authored[0].size, 17);
        let artifact = authored[0]
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .unwrap();
        assert_eq!(artifact.sha1, encode_sha1([0xbb; 20]));
        assert_eq!(artifact.size, 17);
    }

    #[test]
    fn quilt_exact_pair_applies_only_to_required_primary_and_authors_all_streams() {
        let environment = crate::rules::default_environment();
        let classifier = crate::rules::native_classifier_key();
        let paired_coordinate = "org.quiltmc:quilt-loader:0.29.2";
        let paired_path = "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar";
        let native_path =
            format!("org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2-{classifier}.jar");
        let mut paired = profile_library(
            paired_coordinate,
            paired_path,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            99,
        );
        paired.natives = HashMap::from([(environment.os_name.clone(), classifier.clone())]);
        paired.downloads.as_mut().unwrap().classifiers.insert(
            classifier.clone(),
            LibraryArtifact {
                path: native_path.clone(),
                sha1: "cccccccccccccccccccccccccccccccccccccccc".to_string(),
                size: 98,
                url: "https://example.invalid/native.jar".to_string(),
            },
        );
        let unpaired_coordinate = "org.quiltmc:hashed:1.21.5";
        let unpaired_path = "org/quiltmc/hashed/1.21.5/hashed-1.21.5.jar";
        let extra_coordinate = "example:profile-extra:1";
        let extra_path = "example/profile-extra/1/profile-extra-1.jar";
        let proof = ProfileInstallProof::from_test(
            "profile-id".to_string(),
            "1.21.5".to_string(),
            "example.Main".to_string(),
            vec![
                ProfileLibraryProof::from_test(
                    paired_coordinate.to_string(),
                    Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                    Some(11),
                ),
                ProfileLibraryProof::from_test(unpaired_coordinate.to_string(), None, None),
            ],
        );
        let pending = seal_profile_exact_library_declarations(
            profile_fragment(vec![
                paired,
                profile_library(unpaired_coordinate, unpaired_path, "", 77),
                profile_library(
                    extra_coordinate,
                    extra_path,
                    "dddddddddddddddddddddddddddddddddddddddd",
                    76,
                ),
            ]),
            proof,
            LoaderComponentId::Quilt,
            &environment,
        )
        .expect("Quilt declarations");
        let (libraries, sealed_environment) = pending.profile_plan_inputs().unwrap();
        let jobs = jobs_for(libraries, sealed_environment);
        let (pending, classified) = pending
            .classify_jobs(Path::new("/managed/libraries"), jobs)
            .expect("classified Quilt jobs");
        assert_eq!(classified.len(), 4);
        let mut streamed = Vec::new();
        for classified in classified {
            if classified.job.relative_path.as_str() == paired_path {
                assert_eq!(classified.acquisition, LibraryAcquisition::ExactDeclaration);
                continue;
            }
            assert_eq!(classified.acquisition, LibraryAcquisition::FreshStream);
            let (sha1, size) = if classified.job.is_native {
                ([0xcc; 20], 13)
            } else if classified.job.relative_path.as_str() == unpaired_path {
                ([0xbb; 20], 12)
            } else {
                ([0xdd; 20], 14)
            };
            streamed.push(streamed_proof(classified.job, sha1, size));
        }
        let sealed = pending
            .seal_streamed(streamed)
            .expect("sealed Quilt declarations");
        let authored = &sealed.profile_contract().unwrap().0.libraries;
        let paired = &authored[0];
        assert_eq!(paired.sha1, encode_sha1([0xaa; 20]));
        assert_eq!(paired.size, 11);
        let downloads = paired.downloads.as_ref().unwrap();
        assert_eq!(
            downloads.artifact.as_ref().unwrap().sha1,
            encode_sha1([0xaa; 20])
        );
        assert_eq!(
            downloads.classifiers[&classifier].sha1,
            encode_sha1([0xcc; 20])
        );
        assert_eq!(downloads.classifiers[&classifier].size, 13);
        assert_eq!(authored[1].sha1, encode_sha1([0xbb; 20]));
        assert_eq!(authored[1].size, 12);
        assert_eq!(authored[2].sha1, encode_sha1([0xdd; 20]));
        assert_eq!(authored[2].size, 14);
    }

    #[test]
    fn profile_sealing_rejects_partial_pairs_fabric_pairs_and_merge_key_collisions() {
        let environment = crate::rules::default_environment();
        let coordinate = "org.quiltmc:quilt-loader:0.29.2";
        let library = profile_library(
            coordinate,
            "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar",
            "",
            0,
        );
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![library.clone()]),
                profile_proof(
                    coordinate,
                    Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    None,
                ),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::InvalidExactDeclaration)
        ));
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![library.clone()]),
                profile_proof(
                    coordinate,
                    Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    Some(11),
                ),
                LoaderComponentId::Fabric,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::InvalidExactDeclaration)
        ));
        let mut shadow = library;
        shadow.name = "org.quiltmc:quilt-loader:other".to_string();
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![
                    profile_library(
                        coordinate,
                        "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar",
                        "",
                        0,
                    ),
                    shadow,
                ]),
                profile_proof(coordinate, None, None),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::InvalidSelectedPlan)
        ));
        let duplicate_proof = ProfileInstallProof::from_test(
            "profile-id".to_string(),
            "1.21.5".to_string(),
            "example.Main".to_string(),
            vec![
                ProfileLibraryProof::from_test(coordinate.to_string(), None, None),
                ProfileLibraryProof::from_test(coordinate.to_string(), None, None),
            ],
        );
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![profile_library(
                    coordinate,
                    "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar",
                    "",
                    0,
                )]),
                duplicate_proof,
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));
        let shared_path = "example/shared/1/shared-1.jar";
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![
                    profile_library(coordinate, shared_path, "", 0),
                    profile_library("example:other:1", shared_path, "", 0),
                ]),
                profile_proof(coordinate, None, None),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));
    }

    #[test]
    fn profile_sealing_rejects_identity_required_selection_and_native_authorship_drift() {
        let environment = crate::rules::default_environment();
        let coordinate = "org.quiltmc:quilt-loader:0.29.2";
        let path = "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar";

        let mut identity = profile_fragment(vec![profile_library(coordinate, path, "", 0)]);
        identity.main_class = "different.Main".to_string();
        assert!(matches!(
            seal_profile_exact_library_declarations(
                identity,
                profile_proof(coordinate, None, None),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::AncestorMismatch)
        ));

        let mut excluded = profile_library(coordinate, path, "", 0);
        excluded.rules = vec![crate::rules::Rule {
            action: "disallow".to_string(),
            os: None,
            features: None,
        }];
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![excluded]),
                profile_proof(coordinate, None, None),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::InvalidSelectedPlan)
        ));

        let classifier = crate::rules::native_classifier_key();
        let native_only = Library {
            name: coordinate.to_string(),
            url: "https://example.invalid/maven/".to_string(),
            natives: HashMap::from([(environment.os_name.clone(), classifier.clone())]),
            ..Library::default()
        };
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![native_only]),
                profile_proof(coordinate, None, None),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::InvalidSelectedPlan)
        ));

        let native_extra = Library {
            name: "example:native-extra:1".to_string(),
            url: "https://example.invalid/maven/".to_string(),
            natives: HashMap::from([(environment.os_name.clone(), classifier)]),
            ..Library::default()
        };
        assert!(matches!(
            seal_profile_exact_library_declarations(
                profile_fragment(vec![profile_library(coordinate, path, "", 0), native_extra,]),
                profile_proof(coordinate, None, None),
                LoaderComponentId::Quilt,
                &environment,
            ),
            Err(SealedLibraryDeclarationError::InvalidSelectedPlan)
        ));
    }

    #[test]
    fn vanilla_declarations_bind_the_selected_environment() {
        let environment = crate::rules::default_environment();
        let version: VersionJson =
            serde_json::from_str(r#"{"id":"environment-bound"}"#).expect("version");
        let sealed = merge_selected_library_declarations(
            Vec::new(),
            AuthenticatedExactLibraryDeclarations::empty(),
            Vec::new(),
            LibraryStructure::Vanilla(Box::new(VanillaLibraryStructure {
                version: version.clone(),
                environment: environment.clone(),
            })),
        )
        .expect("sealed empty version");
        assert!(sealed.matches_version(&version, &environment));
        let mut different = environment;
        different.os_arch = "different-arch".to_string();
        assert!(!sealed.matches_version(&version, &different));
    }
}
