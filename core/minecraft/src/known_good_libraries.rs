use crate::portable_path::PortableRelativePath;
use crate::download::library_source::{
    AuthenticatedLocalLibraryBytes, LibraryComponentSourceKind, RetainedLibraryComponentSource,
    RetainedLibrarySourceReplay,
};
use crate::download::{
    AuthenticatedSelectedArtifactSource, DownloadJob, ExactLibraryDownloadProof,
    LibraryArtifactPlan, library_artifact_plans_for,
};
use crate::launch::{Library, VersionJson, effective_java_version_for, library_merge_key};
use crate::loaders::providers::ProfileInstallProof;
use crate::loaders::{
    AuthenticatedEmbeddedMavenArtifact, AuthenticatedInstallerLibraryInputs,
    AuthenticatedInstallerLibraryParts, VerifiedProcessorOutputs,
};
use crate::loaders::{LoaderProfileFragment, types::LoaderComponentId};
use crate::rules::Environment;
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedLibraryKind {
    Library,
    Native,
}

struct SealedExactLibraryDeclaration {
    path: PortableRelativePath,
    kind: SealedLibraryKind,
    sha1: [u8; 20],
    size: u64,
    provider_url: Option<String>,
}

pub(crate) struct SealedExactLibraryDeclarations {
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    structure: LibraryStructure,
}

pub(crate) struct PendingExactLibraryDeclarations {
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    selected: BTreeMap<PortableRelativePath, LibraryArtifactPlan>,
    structure: LibraryStructure,
}

pub(crate) struct PendingStreamedLibraryDeclarations {
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    selected: BTreeMap<PortableRelativePath, LibraryArtifactPlan>,
    structure: LibraryStructure,
}

enum LibraryStructure {
    Vanilla(Box<VanillaLibraryStructure>),
    Profile(Box<ProfileLibraryStructure>),
    Installer(Box<InstallerLibraryStructure>),
}

struct VanillaLibraryStructure {
    version: VersionJson,
    environment: Environment,
}

struct ProfileLibraryStructure {
    fragment: LoaderProfileFragment,
    environment: Environment,
}

struct InstallerLibraryStructure {
    libraries: Vec<Library>,
    environment: Environment,
}

struct InstallerTerminalOutputContract {
    path: PortableRelativePath,
    sha1: [u8; 20],
    size: Option<u64>,
}

pub(crate) struct BoundInstallerLibraryDeclarations {
    selected: BTreeMap<PortableRelativePath, InstallerSelectedLibrary>,
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    embedded: BTreeMap<PortableRelativePath, AuthenticatedEmbeddedMavenArtifact>,
    workspace_embedded: Vec<AuthenticatedEmbeddedMavenArtifact>,
    structure: InstallerLibraryStructure,
}

pub(crate) struct PendingInstallerNetworkDeclarations {
    selected: BTreeMap<PortableRelativePath, InstallerSelectedLibrary>,
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    embedded: BTreeMap<PortableRelativePath, AuthenticatedEmbeddedMavenArtifact>,
    workspace_embedded: Vec<AuthenticatedEmbeddedMavenArtifact>,
    structure: InstallerLibraryStructure,
}

pub(crate) struct PendingInstallerReconstructionDeclarations {
    selected: BTreeMap<PortableRelativePath, InstallerSelectedLibrary>,
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    embedded: BTreeMap<PortableRelativePath, AuthenticatedEmbeddedMavenArtifact>,
    workspace_embedded: Vec<AuthenticatedEmbeddedMavenArtifact>,
    structure: InstallerLibraryStructure,
}

pub(crate) struct PendingInstallerReconstructionTerminalDeclarations {
    selected: BTreeMap<PortableRelativePath, InstallerSelectedLibrary>,
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    embedded: BTreeMap<PortableRelativePath, AuthenticatedEmbeddedMavenArtifact>,
    workspace_embedded: Vec<AuthenticatedEmbeddedMavenArtifact>,
    structure: InstallerLibraryStructure,
}

pub(crate) struct PendingInstallerTerminalDeclarations {
    selected: BTreeMap<PortableRelativePath, InstallerSelectedLibrary>,
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    embedded: BTreeMap<PortableRelativePath, AuthenticatedEmbeddedMavenArtifact>,
    workspace_embedded: Vec<AuthenticatedEmbeddedMavenArtifact>,
    structure: InstallerLibraryStructure,
    network_sources: Vec<RetainedLibraryComponentSource>,
}

pub(crate) struct SealedInstallerLibrarySources {
    declarations: SealedExactLibraryDeclarations,
    sources: Vec<RetainedLibraryComponentSource>,
}

impl SealedInstallerLibrarySources {
    pub(crate) fn into_parts(
        self,
    ) -> (
        SealedExactLibraryDeclarations,
        Vec<RetainedLibraryComponentSource>,
    ) {
        (self.declarations, self.sources)
    }
}

struct InstallerSelectedLibrary {
    plan: LibraryArtifactPlan,
    producer: InstallerLibraryProducer,
}

enum InstallerLibraryProducer {
    ExactNetwork,
    FreshNetwork,
    Embedded,
    Terminal(InstallerTerminalOutputContract),
}

pub(crate) struct AuthenticatedVanillaLibraryDeclarationSource {
    declarations: PendingExactLibraryDeclarations,
    source: AuthenticatedSelectedArtifactSource,
}

impl AuthenticatedVanillaLibraryDeclarationSource {
    pub(crate) fn into_parts(
        self,
    ) -> (
        PendingExactLibraryDeclarations,
        AuthenticatedSelectedArtifactSource,
    ) {
        (self.declarations, self.source)
    }
}

pub(crate) struct ClassifiedLibraryDownload {
    job: DownloadJob,
    acquisition: LibraryAcquisition,
}

pub(crate) struct ClassifiedLibraryReconstruction {
    plan: LibraryArtifactPlan,
    acquisition: LibraryAcquisition,
}

impl ClassifiedLibraryDownload {
    pub(crate) fn job(&self) -> &DownloadJob {
        &self.job
    }

    #[cfg(test)]
    pub(crate) fn acquisition(&self) -> LibraryAcquisition {
        self.acquisition
    }

    pub(crate) fn into_parts(self) -> (DownloadJob, LibraryAcquisition) {
        (self.job, self.acquisition)
    }

    #[cfg(test)]
    pub(crate) fn from_test(job: DownloadJob, acquisition: LibraryAcquisition) -> Self {
        Self { job, acquisition }
    }
}

impl ClassifiedLibraryReconstruction {
    pub(crate) fn into_parts(self) -> (LibraryArtifactPlan, LibraryAcquisition) {
        (self.plan, self.acquisition)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryAcquisition {
    ExactDeclaration,
    FreshStream,
}

struct StreamedExactLibraryProof {
    path: PortableRelativePath,
    kind: SealedLibraryKind,
    sha1: [u8; 20],
    size: u64,
    provider_url: String,
    expected: crate::download::ExpectedIntegrity,
}

impl StreamedExactLibraryProof {
    fn from_authenticated_stream(
        path: PortableRelativePath,
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

pub(crate) fn bind_installer_library_declarations(
    inputs: AuthenticatedInstallerLibraryInputs,
    environment: Environment,
) -> Result<BoundInstallerLibraryDeclarations, SealedLibraryDeclarationError> {
    let AuthenticatedInstallerLibraryParts {
        libraries,
        embedded_artifacts,
        terminal_outputs,
    } = inputs.into_parts();
    let terminal_contracts = terminal_outputs
        .into_iter()
        .map(|(path, sha1, size)| InstallerTerminalOutputContract { path, sha1, size })
        .collect::<Vec<_>>();
    let plans = library_artifact_plans_for(&libraries, &environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    let mut selected_paths = BTreeMap::new();
    let mut portable_selected = BTreeMap::new();
    for plan in plans {
        let portable = plan.relative_path.key();
        if selected_paths.contains_key(&plan.relative_path)
            || portable_selected
                .insert(portable, plan.relative_path.clone())
                .is_some()
        {
            return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
        }
        selected_paths.insert(plan.relative_path.clone(), plan);
    }

    let mut terminals = BTreeMap::new();
    for terminal in terminal_contracts {
        let portable = terminal.path.key();
        let selected_path = portable_selected
            .get(&portable)
            .ok_or(SealedLibraryDeclarationError::ExtraDeclaration)?;
        if selected_path != &terminal.path
            || terminals.insert(terminal.path.clone(), terminal).is_some()
        {
            return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
        }
    }

    let mut selected_embedded = BTreeMap::new();
    let mut workspace_embedded = Vec::new();
    for artifact in embedded_artifacts {
        let portable = artifact.relative_path().key();
        match portable_selected.get(&portable) {
            Some(selected_path) if selected_path != artifact.relative_path() => {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
            Some(_) => {
                if selected_embedded
                    .insert(artifact.relative_path().clone(), artifact)
                    .is_some()
                {
                    return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
                }
            }
            None => workspace_embedded.push(artifact),
        }
    }

    let mut selected = BTreeMap::new();
    let mut entries = BTreeMap::new();
    for (path, plan) in selected_paths {
        let producer = match (terminals.remove(&path), selected_embedded.get(&path)) {
            (Some(_), Some(_)) => {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
            (Some(contract), None) => {
                validate_plan_contract_optional(&plan, contract.sha1, contract.size)?;
                InstallerLibraryProducer::Terminal(contract)
            }
            (None, Some(artifact)) => {
                let sha1 = Sha1::digest(artifact.bytes()).into();
                let size = artifact.bytes().len() as u64;
                validate_plan_contract(&plan, sha1, size)?;
                insert_exact_declaration(&mut entries, &plan, sha1, size, false)?;
                InstallerLibraryProducer::Embedded
            }
            (None, None) => {
                if plan.source_url.is_none() {
                    return Err(SealedLibraryDeclarationError::MissingStreamSource);
                }
                match (plan.expected.sha1.as_deref(), plan.expected.size) {
                    (Some(sha1), Some(size)) if size > 0 => {
                        let sha1 = decode_sha1(sha1)?;
                        insert_exact_declaration(&mut entries, &plan, sha1, size, true)?;
                        InstallerLibraryProducer::ExactNetwork
                    }
                    _ => InstallerLibraryProducer::FreshNetwork,
                }
            }
        };
        selected.insert(path, InstallerSelectedLibrary { plan, producer });
    }
    if !terminals.is_empty() {
        return Err(SealedLibraryDeclarationError::ExtraDeclaration);
    }
    Ok(BoundInstallerLibraryDeclarations {
        selected,
        entries,
        embedded: selected_embedded,
        workspace_embedded,
        structure: InstallerLibraryStructure {
            libraries,
            environment,
        },
    })
}

fn insert_exact_declaration(
    entries: &mut BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
    plan: &LibraryArtifactPlan,
    sha1: [u8; 20],
    size: u64,
    independently_downloadable: bool,
) -> Result<(), SealedLibraryDeclarationError> {
    if size == 0
        || entries
            .insert(
                plan.relative_path.clone(),
                SealedExactLibraryDeclaration {
                    path: plan.relative_path.clone(),
                    kind: kind_for_plan(plan),
                    sha1,
                    size,
                    provider_url: independently_downloadable
                        .then(|| plan.source_url.clone())
                        .flatten(),
                },
            )
            .is_some()
    {
        return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
    }
    Ok(())
}

fn validate_plan_contract_optional(
    plan: &LibraryArtifactPlan,
    sha1: [u8; 20],
    size: Option<u64>,
) -> Result<(), SealedLibraryDeclarationError> {
    if size.is_some_and(|size| size == 0)
        || matches!((plan.expected.size, size), (Some(expected), Some(actual)) if expected != actual)
        || plan
            .expected
            .sha1
            .as_deref()
            .is_some_and(|expected| !decode_sha1(expected).is_ok_and(|expected| expected == sha1))
    {
        return Err(SealedLibraryDeclarationError::ContractDrift);
    }
    Ok(())
}

impl BoundInstallerLibraryDeclarations {
    pub(crate) fn into_network_jobs(
        self,
    ) -> Result<
        (
            PendingInstallerNetworkDeclarations,
            Vec<ClassifiedLibraryDownload>,
        ),
        SealedLibraryDeclarationError,
    > {
        let mut jobs = Vec::new();
        for selected in self.selected.values() {
            let acquisition = match selected.producer {
                InstallerLibraryProducer::ExactNetwork => LibraryAcquisition::ExactDeclaration,
                InstallerLibraryProducer::FreshNetwork => LibraryAcquisition::FreshStream,
                InstallerLibraryProducer::Embedded | InstallerLibraryProducer::Terminal(_) => {
                    continue;
                }
            };
            let plan = &selected.plan;
            let url = plan
                .source_url
                .clone()
                .ok_or(SealedLibraryDeclarationError::MissingStreamSource)?;
            jobs.push(ClassifiedLibraryDownload {
                acquisition,
                job: DownloadJob {
                    relative_path: plan.relative_path.clone(),
                    url,
                    name: plan.name.clone(),
                    expected: plan.expected.clone(),
                    is_native: plan.is_native,
                },
            });
        }
        Ok((
            PendingInstallerNetworkDeclarations {
                selected: self.selected,
                entries: self.entries,
                embedded: self.embedded,
                workspace_embedded: self.workspace_embedded,
                structure: self.structure,
            },
            jobs,
        ))
    }

    pub(crate) fn into_reconstruction_jobs(
        self,
    ) -> (
        PendingInstallerReconstructionDeclarations,
        Vec<ClassifiedLibraryReconstruction>,
    ) {
        let jobs = self
            .selected
            .values()
            .filter_map(|selected| {
                let acquisition = match selected.producer {
                    InstallerLibraryProducer::ExactNetwork => LibraryAcquisition::ExactDeclaration,
                    InstallerLibraryProducer::FreshNetwork => LibraryAcquisition::FreshStream,
                    InstallerLibraryProducer::Embedded | InstallerLibraryProducer::Terminal(_) => {
                        return None;
                    }
                };
                Some(ClassifiedLibraryReconstruction {
                    plan: selected.plan.clone(),
                    acquisition,
                })
            })
            .collect();
        (
            PendingInstallerReconstructionDeclarations {
                selected: self.selected,
                entries: self.entries,
                embedded: self.embedded,
                workspace_embedded: self.workspace_embedded,
                structure: self.structure,
            },
            jobs,
        )
    }
}

impl PendingInstallerNetworkDeclarations {
    pub(crate) fn embedded_maven_artifact(
        &self,
        path: &PortableRelativePath,
    ) -> Option<&AuthenticatedEmbeddedMavenArtifact> {
        self.embedded.get(path).or_else(|| {
            self.workspace_embedded
                .iter()
                .find(|artifact| artifact.relative_path() == path)
        })
    }

    pub(crate) fn complete_network(
        self,
        sources: Vec<RetainedLibraryComponentSource>,
    ) -> Result<PendingInstallerTerminalDeclarations, SealedLibraryDeclarationError> {
        let mut sources =
            sources
                .into_iter()
                .try_fold(BTreeMap::new(), |mut sources, source| {
                    let path = source.relative_path().clone();
                    if sources.insert(path, source).is_some() {
                        return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
                    }
                    Ok(sources)
                })?;
        let mut entries = self.entries;
        let mut network_sources = Vec::new();
        for (path, selected) in &self.selected {
            let exact = matches!(selected.producer, InstallerLibraryProducer::ExactNetwork);
            if !exact && !matches!(selected.producer, InstallerLibraryProducer::FreshNetwork) {
                continue;
            }
            let source = sources
                .remove(path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            let kind = match source.source_kind() {
                LibraryComponentSourceKind::Library => SealedLibraryKind::Library,
                LibraryComponentSourceKind::NativeLibrary => SealedLibraryKind::Native,
            };
            let (provider_url, expected) = source
                .network_origin()
                .ok_or(SealedLibraryDeclarationError::ContractDrift)?;
            if kind != kind_for_plan(&selected.plan)
                || provider_url != selected.plan.source_url.as_deref().unwrap_or_default()
                || expected != &selected.plan.expected
            {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            validate_plan_contract(
                &selected.plan,
                source.observed_sha1(),
                source.observed_size(),
            )?;
            if exact {
                let declaration = entries
                    .get(path)
                    .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
                if declaration.sha1 != source.observed_sha1()
                    || declaration.size != source.observed_size()
                {
                    return Err(SealedLibraryDeclarationError::ContractDrift);
                }
            } else {
                insert_exact_declaration(
                    &mut entries,
                    &selected.plan,
                    source.observed_sha1(),
                    source.observed_size(),
                    true,
                )?;
            }
            network_sources.push(source);
        }
        if !sources.is_empty() {
            return Err(SealedLibraryDeclarationError::ExtraDeclaration);
        }
        Ok(PendingInstallerTerminalDeclarations {
            selected: self.selected,
            entries,
            embedded: self.embedded,
            workspace_embedded: self.workspace_embedded,
            structure: self.structure,
            network_sources,
        })
    }
}

impl PendingInstallerReconstructionDeclarations {
    pub(crate) fn embedded_maven_artifact(
        &self,
        path: &PortableRelativePath,
    ) -> Option<&AuthenticatedEmbeddedMavenArtifact> {
        self.embedded.get(path).or_else(|| {
            self.workspace_embedded
                .iter()
                .find(|artifact| artifact.relative_path() == path)
        })
    }

    pub(crate) fn complete_network(
        self,
        streamed: Vec<ExactLibraryDownloadProof>,
    ) -> Result<PendingInstallerReconstructionTerminalDeclarations, SealedLibraryDeclarationError>
    {
        let mut streamed =
            streamed
                .into_iter()
                .try_fold(BTreeMap::new(), |mut streamed, proof| {
                    let (path, is_native, provider_url, expected, size, sha1) = proof.into_parts();
                    let proof = StreamedExactLibraryProof::from_authenticated_stream(
                        path.clone(),
                        if is_native {
                            SealedLibraryKind::Native
                        } else {
                            SealedLibraryKind::Library
                        },
                        provider_url,
                        expected,
                        sha1,
                        size,
                    )?;
                    if streamed.insert(path, proof).is_some() {
                        return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
                    }
                    Ok(streamed)
                })?;
        let mut entries = self.entries;
        for (path, selected) in &self.selected {
            if !matches!(selected.producer, InstallerLibraryProducer::FreshNetwork) {
                continue;
            }
            let proof = streamed
                .remove(path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            if proof.path != *path
                || proof.kind != kind_for_plan(&selected.plan)
                || proof.provider_url != selected.plan.source_url.as_deref().unwrap_or_default()
                || proof.expected != selected.plan.expected
            {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            validate_plan_contract(&selected.plan, proof.sha1, proof.size)?;
            insert_exact_declaration(&mut entries, &selected.plan, proof.sha1, proof.size, true)?;
        }
        if !streamed.is_empty() {
            return Err(SealedLibraryDeclarationError::ExtraDeclaration);
        }
        Ok(PendingInstallerReconstructionTerminalDeclarations {
            selected: self.selected,
            entries,
            embedded: self.embedded,
            workspace_embedded: self.workspace_embedded,
            structure: self.structure,
        })
    }
}

impl PendingInstallerReconstructionTerminalDeclarations {
    pub(crate) fn embedded_maven_artifact(
        &self,
        path: &PortableRelativePath,
    ) -> Option<&AuthenticatedEmbeddedMavenArtifact> {
        self.embedded.get(path).or_else(|| {
            self.workspace_embedded
                .iter()
                .find(|artifact| artifact.relative_path() == path)
        })
    }

    pub(crate) fn seal_declared_terminal_outputs(
        self,
        retain_sources: bool,
    ) -> Result<
        (
            SealedExactLibraryDeclarations,
            Vec<AuthenticatedLocalLibraryBytes>,
        ),
        SealedLibraryDeclarationError,
    > {
        if retain_sources
            && self
                .selected
                .values()
                .any(|selected| matches!(selected.producer, InstallerLibraryProducer::Terminal(_)))
        {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        let mut entries = self.entries;
        for selected in self.selected.values() {
            let InstallerLibraryProducer::Terminal(contract) = &selected.producer else {
                continue;
            };
            let size = contract
                .size
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            validate_plan_contract(&selected.plan, contract.sha1, size)?;
            insert_exact_declaration(&mut entries, &selected.plan, contract.sha1, size, false)?;
        }
        if entries.len() != self.selected.len() {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        let declarations = SealedExactLibraryDeclarations {
            entries,
            structure: LibraryStructure::Installer(Box::new(self.structure)),
        };
        let mut sources = Vec::new();
        if retain_sources {
            collect_reconstruction_embedded_sources(&self.selected, self.embedded, &mut sources)?;
        }
        Ok((declarations, sources))
    }

    pub(crate) fn seal_observed_terminal_outputs(
        self,
        outputs: VerifiedProcessorOutputs,
        retain_sources: bool,
    ) -> Result<
        (
            SealedExactLibraryDeclarations,
            Vec<AuthenticatedLocalLibraryBytes>,
        ),
        SealedLibraryDeclarationError,
    > {
        let mut outputs = outputs
            .into_entries()
            .into_iter()
            .map(|(path, output)| {
                let (bytes, size, sha1) = output.into_parts();
                let observed_sha1: [u8; 20] = Sha1::digest(&bytes).into();
                if size == 0 || size != bytes.len() as u64 || sha1 != observed_sha1 {
                    return Err(SealedLibraryDeclarationError::ContractDrift);
                }
                Ok((path, (bytes, size, sha1)))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut entries = self.entries;
        let mut sources = Vec::new();
        for (path, selected) in &self.selected {
            let InstallerLibraryProducer::Terminal(contract) = &selected.producer else {
                continue;
            };
            let (bytes, size, sha1) = outputs
                .remove(path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            if sha1 != contract.sha1 || contract.size.is_some_and(|expected| expected != size) {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            validate_plan_contract(&selected.plan, sha1, size)?;
            insert_exact_declaration(&mut entries, &selected.plan, sha1, size, false)?;
            if retain_sources {
                sources.push(authenticated_local_bytes_candidate(
                    path.clone(),
                    &selected.plan,
                    bytes,
                )?);
            }
        }
        if !outputs.is_empty() {
            return Err(SealedLibraryDeclarationError::ExtraDeclaration);
        }
        if entries.len() != self.selected.len() {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        if retain_sources {
            collect_reconstruction_embedded_sources(&self.selected, self.embedded, &mut sources)?;
        }
        Ok((
            SealedExactLibraryDeclarations {
                entries,
                structure: LibraryStructure::Installer(Box::new(self.structure)),
            },
            sources,
        ))
    }
}

fn collect_reconstruction_embedded_sources(
    selected: &BTreeMap<PortableRelativePath, InstallerSelectedLibrary>,
    embedded: BTreeMap<PortableRelativePath, AuthenticatedEmbeddedMavenArtifact>,
    sources: &mut Vec<AuthenticatedLocalLibraryBytes>,
) -> Result<(), SealedLibraryDeclarationError> {
    for (path, artifact) in embedded {
        let selected = selected
            .get(&path)
            .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
        if !matches!(selected.producer, InstallerLibraryProducer::Embedded) {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
        let (artifact_path, bytes) = artifact.into_parts();
        sources.push(authenticated_local_bytes_candidate(
            artifact_path,
            &selected.plan,
            bytes,
        )?);
    }
    Ok(())
}

fn authenticated_local_bytes_candidate(
    path: PortableRelativePath,
    plan: &LibraryArtifactPlan,
    bytes: Vec<u8>,
) -> Result<AuthenticatedLocalLibraryBytes, SealedLibraryDeclarationError> {
    let size = bytes.len() as u64;
    let sha1 = Sha1::digest(&bytes).into();
    validate_plan_contract(plan, sha1, size)?;
    AuthenticatedLocalLibraryBytes::new(path, component_kind_for_plan(plan), bytes, size, sha1)
        .map_err(|_| SealedLibraryDeclarationError::ContractDrift)
}

impl PendingInstallerTerminalDeclarations {
    pub(crate) fn embedded_maven_artifact(
        &self,
        path: &PortableRelativePath,
    ) -> Option<&AuthenticatedEmbeddedMavenArtifact> {
        self.embedded.get(path).or_else(|| {
            self.workspace_embedded
                .iter()
                .find(|artifact| artifact.relative_path() == path)
        })
    }

    pub(crate) fn replay_network_source(
        &self,
        path: &PortableRelativePath,
    ) -> Result<Option<RetainedLibrarySourceReplay>, crate::loaders::types::LoaderError> {
        let Some(source) = self
            .network_sources
            .iter()
            .find(|source| source.relative_path() == path)
        else {
            return Ok(None);
        };
        source.replay().map(Some)
    }

    pub(crate) fn seal_terminal_outputs(
        self,
        outputs: VerifiedProcessorOutputs,
    ) -> Result<SealedInstallerLibrarySources, SealedLibraryDeclarationError> {
        self.seal_outputs(outputs)
    }

    pub(crate) fn seal_without_terminal_outputs(
        self,
    ) -> Result<SealedInstallerLibrarySources, SealedLibraryDeclarationError> {
        if self
            .selected
            .values()
            .any(|selected| matches!(selected.producer, InstallerLibraryProducer::Terminal(_)))
            || self.entries.len() != self.selected.len()
        {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        let declarations = SealedExactLibraryDeclarations {
            entries: self.entries,
            structure: LibraryStructure::Installer(Box::new(self.structure)),
        };
        let mut sources = self.network_sources;
        for (path, artifact) in self.embedded {
            if !matches!(
                self.selected.get(&path).map(|selected| &selected.producer),
                Some(InstallerLibraryProducer::Embedded)
            ) {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            let selected = self
                .selected
                .get(&path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            let (artifact_path, bytes) = artifact.into_parts();
            sources.push(authenticated_local_component_source(
                artifact_path,
                &selected.plan,
                bytes,
            )?);
        }
        validate_installer_source_union(&declarations, &sources)?;
        Ok(SealedInstallerLibrarySources {
            declarations,
            sources,
        })
    }

    fn seal_outputs(
        self,
        outputs: VerifiedProcessorOutputs,
    ) -> Result<SealedInstallerLibrarySources, SealedLibraryDeclarationError> {
        let mut outputs = outputs
            .into_entries()
            .into_iter()
            .map(|(path, output)| {
                let (bytes, size, sha1) = output.into_parts();
                let actual_sha1: [u8; 20] = Sha1::digest(&bytes).into();
                if size == 0 || size != bytes.len() as u64 || sha1 != actual_sha1 {
                    return Err(SealedLibraryDeclarationError::ContractDrift);
                }
                Ok((path, (bytes, size, sha1)))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut entries = self.entries;
        let mut sources = self.network_sources;
        for (path, selected) in &self.selected {
            let InstallerLibraryProducer::Terminal(contract) = &selected.producer else {
                continue;
            };
            let output = outputs
                .remove(path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            let (bytes, size, sha1) = output;
            if sha1 != contract.sha1 || contract.size.is_some_and(|expected| expected != size) {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            validate_plan_contract(&selected.plan, sha1, size)?;
            insert_exact_declaration(&mut entries, &selected.plan, sha1, size, false)?;
            sources.push(
                RetainedLibraryComponentSource::from_authenticated_local_bytes(
                    path.clone(),
                    component_kind_for_plan(&selected.plan),
                    bytes,
                    size,
                    sha1,
                )
                .map_err(|_| SealedLibraryDeclarationError::ContractDrift)?,
            );
        }
        if !outputs.is_empty() {
            return Err(SealedLibraryDeclarationError::ExtraDeclaration);
        }
        for (path, artifact) in self.embedded {
            if !matches!(
                self.selected.get(&path).map(|selected| &selected.producer),
                Some(InstallerLibraryProducer::Embedded)
            ) {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            let selected = self
                .selected
                .get(&path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            let (artifact_path, bytes) = artifact.into_parts();
            sources.push(authenticated_local_component_source(
                artifact_path,
                &selected.plan,
                bytes,
            )?);
        }
        if entries.len() != self.selected.len() {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        let declarations = SealedExactLibraryDeclarations {
            entries,
            structure: LibraryStructure::Installer(Box::new(self.structure)),
        };
        validate_installer_source_union(&declarations, &sources)?;
        Ok(SealedInstallerLibrarySources {
            declarations,
            sources,
        })
    }
}

fn authenticated_local_component_source(
    path: PortableRelativePath,
    plan: &LibraryArtifactPlan,
    bytes: Vec<u8>,
) -> Result<RetainedLibraryComponentSource, SealedLibraryDeclarationError> {
    let size = bytes.len() as u64;
    let sha1 = Sha1::digest(&bytes).into();
    validate_plan_contract(plan, sha1, size)?;
    RetainedLibraryComponentSource::from_authenticated_local_bytes(
        path,
        component_kind_for_plan(plan),
        bytes,
        size,
        sha1,
    )
    .map_err(|_| SealedLibraryDeclarationError::ContractDrift)
}

fn component_kind_for_plan(plan: &LibraryArtifactPlan) -> LibraryComponentSourceKind {
    if plan.is_native {
        LibraryComponentSourceKind::NativeLibrary
    } else {
        LibraryComponentSourceKind::Library
    }
}

fn validate_installer_source_union(
    declarations: &SealedExactLibraryDeclarations,
    sources: &[RetainedLibraryComponentSource],
) -> Result<(), SealedLibraryDeclarationError> {
    let mut paths = BTreeSet::new();
    let mut portable_paths = BTreeSet::new();
    for source in sources {
        let path = source.relative_path();
        if !paths.insert(path.clone()) || !portable_paths.insert(path.key()) {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
        let expected = declarations
            .entries
            .get(path)
            .ok_or(SealedLibraryDeclarationError::ExtraDeclaration)?;
        let kind = match source.source_kind() {
            LibraryComponentSourceKind::Library => SealedLibraryKind::Library,
            LibraryComponentSourceKind::NativeLibrary => SealedLibraryKind::Native,
        };
        if kind != expected.kind
            || source.observed_sha1() != expected.sha1
            || source.observed_size() != expected.size
        {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
    }
    if paths.len() != declarations.entries.len() {
        return Err(SealedLibraryDeclarationError::MissingDeclaration);
    }
    Ok(())
}

impl SealedExactLibraryDeclarations {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(
        &self,
        path: &PortableRelativePath,
    ) -> Option<(SealedLibraryKind, [u8; 20], u64, Option<&str>)> {
        self.entries.get(path).map(|entry| {
            (
                entry.kind,
                entry.sha1,
                entry.size,
                entry.provider_url.as_deref(),
            )
        })
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
            LibraryStructure::Installer(_) => None,
        }
    }

    pub(crate) fn installer_contract(&self) -> Option<(&[Library], &Environment)> {
        match &self.structure {
            LibraryStructure::Installer(contract) => {
                Some((&contract.libraries, &contract.environment))
            }
            LibraryStructure::Vanilla(_) | LibraryStructure::Profile(_) => None,
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
            LibraryStructure::Installer(_) => None,
        }
    }

    pub(crate) fn classify_jobs(
        self,
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
            if plan.source_url.as_deref() != Some(job.url.as_str()) || job.expected != plan.expected
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
    source: AuthenticatedSelectedArtifactSource,
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
    Ok(AuthenticatedVanillaLibraryDeclarationSource {
        declarations,
        source,
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
            provider_url: plan.source_url.clone(),
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
    entries: BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
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
                    provider_url: Some(proof.provider_url),
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
        LibraryStructure::Installer(contract) => LibraryStructure::Installer(contract),
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
                provider_url: plan.source_url.clone(),
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
        PortableRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
    }));
    matches += downloads
        .classifiers
        .values()
        .filter(|artifact| {
            PortableRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
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
    entries: &BTreeMap<PortableRelativePath, SealedExactLibraryDeclaration>,
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
            && PortableRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path)
        {
            artifact.sha1.clone_from(&digest);
            artifact.size = size;
            matches += 1;
        }
        for artifact in downloads.classifiers.values_mut() {
            if PortableRelativePath::new(&artifact.path).as_ref() == Ok(&plan.relative_path) {
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

    fn plan(path: &str, exact: bool, source: bool, native: bool) -> LibraryArtifactPlan {
        LibraryArtifactPlan {
            relative_path: PortableRelativePath::new(path).unwrap(),
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
            path: PortableRelativePath::new(path).unwrap(),
            kind: if native {
                SealedLibraryKind::Native
            } else {
                SealedLibraryKind::Library
            },
            sha1: [1; 20],
            size: 7,
            provider_url: None,
        }
    }

    fn stream(path: &str, native: bool) -> StreamedExactLibraryProof {
        StreamedExactLibraryProof::from_authenticated_stream(
            PortableRelativePath::new(path).unwrap(),
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

    fn without_download_source(mut library: Library) -> Library {
        library.url.clear();
        if let Some(artifact) = library
            .downloads
            .as_mut()
            .and_then(|downloads| downloads.artifact.as_mut())
        {
            artifact.url.clear();
        }
        library
    }

    fn retained_network_source(
        classified: &ClassifiedLibraryDownload,
        sha1: [u8; 20],
        size: u64,
    ) -> RetainedLibraryComponentSource {
        RetainedLibraryComponentSource::from_test_identity(
            classified.job.relative_path.clone(),
            classified.job.is_native,
            classified.job.url.clone(),
            classified.job.expected.clone(),
            size,
            sha1,
        )
    }

    fn jobs_for(libraries: &[Library], environment: &Environment) -> Vec<DownloadJob> {
        library_artifact_plans_for(libraries, environment)
            .unwrap()
            .into_iter()
            .map(|plan| DownloadJob {
                relative_path: plan.relative_path,
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
            PortableRelativePath::new("drift.jar").unwrap(),
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
    fn preclassification_rejects_exact_job_url_and_expected_drift() {
        let selected = plan("exact.jar", true, true, false);
        let job = || DownloadJob {
            relative_path: selected.relative_path.clone(),
            url: selected.source_url.clone().unwrap(),
            name: selected.name.clone(),
            expected: selected.expected.clone(),
            is_native: false,
        };
        let pending =
            || seal_exact_plan_subset(vec![selected.clone()], version_contract()).unwrap();

        let (_, classified) = pending().classify_jobs(vec![job()]).unwrap();
        assert_eq!(classified.len(), 1);
        assert_eq!(
            classified[0].acquisition,
            LibraryAcquisition::ExactDeclaration
        );

        let mut url_drift = job();
        url_drift.url = "https://other.invalid/library.jar".to_string();
        assert!(matches!(
            pending().classify_jobs(vec![url_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let mut expected_drift = job();
        expected_drift.expected.size = Some(8);
        assert!(matches!(
            pending().classify_jobs(vec![expected_drift]),
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
        let (pending, classified) = pending.classify_jobs(jobs).expect("classified Fabric jobs");
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
        let (pending, classified) = pending.classify_jobs(jobs).expect("classified Quilt jobs");
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

    #[test]
    fn installer_classifier_binds_exact_fresh_embedded_and_terminal_once() {
        let environment = crate::rules::default_environment();
        let exact_path = "example/exact/1/exact-1.jar";
        let fresh_path = "example/fresh/1/fresh-1.jar";
        let embedded_path = PortableRelativePath::new("example/embedded/1/embedded-1.jar").unwrap();
        let terminal_path = PortableRelativePath::new("example/terminal/1/terminal-1.jar").unwrap();
        let embedded_bytes = b"authenticated embedded".to_vec();
        let terminal_bytes = b"verified terminal".to_vec();
        let embedded_sha1: [u8; 20] = Sha1::digest(&embedded_bytes).into();
        let terminal_sha1: [u8; 20] = Sha1::digest(&terminal_bytes).into();
        let libraries = vec![
            profile_library("example:exact:1", exact_path, &encode_sha1([1; 20]), 7),
            profile_library("example:fresh:1", fresh_path, "", 0),
            without_download_source(profile_library(
                "example:embedded:1",
                embedded_path.as_str(),
                &encode_sha1(embedded_sha1),
                embedded_bytes.len() as i64,
            )),
            without_download_source(profile_library(
                "example:terminal:1",
                terminal_path.as_str(),
                &encode_sha1(terminal_sha1),
                terminal_bytes.len() as i64,
            )),
        ];
        let bound = bind_installer_library_declarations(
            AuthenticatedInstallerLibraryInputs::from_test(
                libraries,
                vec![(embedded_path.clone(), embedded_bytes.clone())],
                vec![(terminal_path.clone(), terminal_sha1, None)],
            ),
            environment,
        )
        .expect("four producer classifier");
        let (pending, jobs) = bound.into_network_jobs().expect("network transition");
        assert_eq!(jobs.len(), 2);
        let exact = jobs
            .iter()
            .find(|classified| classified.job.relative_path.as_str() == exact_path)
            .unwrap();
        let fresh = jobs
            .iter()
            .find(|classified| classified.job.relative_path.as_str() == fresh_path)
            .unwrap();
        assert_eq!(exact.acquisition, LibraryAcquisition::ExactDeclaration);
        assert_eq!(fresh.acquisition, LibraryAcquisition::FreshStream);
        let pending = pending
            .complete_network(vec![
                retained_network_source(exact, [1; 20], 7),
                retained_network_source(fresh, [2; 20], 8),
            ])
            .expect("retained network sources");
        let sealed = pending
            .seal_terminal_outputs(VerifiedProcessorOutputs::from_test_terminal(vec![(
                terminal_path.clone(),
                terminal_bytes,
            )]))
            .expect("terminal declaration");
        let (sealed, sources) = sealed.into_parts();
        assert_eq!(sealed.len(), 4);
        let exact_path = PortableRelativePath::new(exact_path).expect("exact path");
        let fresh_path = PortableRelativePath::new(fresh_path).expect("fresh path");
        assert_eq!(
            sealed.get(&exact_path).and_then(|entry| entry.3),
            Some("https://example.invalid/library.jar")
        );
        assert_eq!(
            sealed.get(&fresh_path).and_then(|entry| entry.3),
            Some("https://example.invalid/library.jar")
        );
        assert_eq!(sealed.get(&embedded_path).and_then(|entry| entry.3), None);
        assert_eq!(sealed.get(&terminal_path).and_then(|entry| entry.3), None);
        assert_eq!(sources.len(), 4);
        assert_eq!(
            sources
                .iter()
                .map(|source| source.relative_path().clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([exact_path, fresh_path, embedded_path, terminal_path])
        );
    }

    #[test]
    fn installer_reconstruction_seals_only_exact_declared_terminals_without_publications() {
        let terminal_path = PortableRelativePath::new("example/terminal/1/terminal-1.jar").unwrap();
        let terminal_sha1 = [3; 20];
        let fresh_path = PortableRelativePath::new("example/fresh/1/fresh-1.jar").unwrap();
        let libraries = vec![
            profile_library(
                "example:exact:1",
                "example/exact/1/exact-1.jar",
                &encode_sha1([1; 20]),
                7,
            ),
            profile_library("example:fresh:1", fresh_path.as_str(), "", 0),
            without_download_source(profile_library(
                "example:terminal:1",
                terminal_path.as_str(),
                &encode_sha1(terminal_sha1),
                9,
            )),
        ];
        let bound = bind_installer_library_declarations(
            AuthenticatedInstallerLibraryInputs::from_test(
                libraries,
                Vec::new(),
                vec![(terminal_path.clone(), terminal_sha1, Some(9))],
            ),
            crate::rules::default_environment(),
        )
        .expect("reconstruction declarations");
        let (pending, jobs) = bound.into_reconstruction_jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            jobs.iter()
                .filter(|job| job.acquisition == LibraryAcquisition::FreshStream)
                .count(),
            1
        );
        let fresh = jobs
            .iter()
            .find(|job| job.plan.relative_path == fresh_path)
            .expect("fresh source job");
        let streamed = ExactLibraryDownloadProof::new_bound_for_test(
            fresh_path,
            false,
            fresh.plan.source_url.clone().expect("fresh source URL"),
            fresh.plan.expected.clone(),
            8,
            [2; 20],
        );
        let (sealed, sources) = pending
            .complete_network(vec![streamed])
            .expect("fresh reconstruction source")
            .seal_declared_terminal_outputs(false)
            .expect("declared terminal seal");

        assert!(sources.is_empty());
        assert_eq!(sealed.len(), 3);
        assert_eq!(
            sealed.get(&terminal_path),
            Some((SealedLibraryKind::Library, terminal_sha1, 9, None))
        );
    }

    #[test]
    fn installer_reconstruction_requires_execution_when_terminal_size_is_missing() {
        let terminal_path = PortableRelativePath::new("example/terminal/1/terminal-1.jar").unwrap();
        let bound = bind_installer_library_declarations(
            AuthenticatedInstallerLibraryInputs::from_test(
                vec![without_download_source(profile_library(
                    "example:terminal:1",
                    terminal_path.as_str(),
                    &encode_sha1([4; 20]),
                    0,
                ))],
                Vec::new(),
                vec![(terminal_path, [4; 20], None)],
            ),
            crate::rules::default_environment(),
        )
        .expect("incomplete terminal declaration");
        let (pending, jobs) = bound.into_reconstruction_jobs();
        assert!(jobs.is_empty());
        let pending = pending.complete_network(Vec::new()).expect("no sources");

        assert!(matches!(
            pending.seal_declared_terminal_outputs(false),
            Err(SealedLibraryDeclarationError::MissingDeclaration)
        ));
    }

    #[test]
    fn retained_installer_reconstruction_requires_and_retains_final_local_sources() {
        let embedded_path = PortableRelativePath::new("example/embedded/1/embedded-1.jar").unwrap();
        let embedded_bytes = b"selected embedded reconstruction source".to_vec();
        let embedded_sha1: [u8; 20] = Sha1::digest(&embedded_bytes).into();
        let terminal_path = PortableRelativePath::new("example/terminal/1/terminal-1.jar").unwrap();
        let terminal_bytes = b"verified terminal reconstruction source".to_vec();
        let terminal_sha1: [u8; 20] = Sha1::digest(&terminal_bytes).into();
        let build = || {
            let bound = bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![
                        without_download_source(profile_library(
                            "example:embedded:1",
                            embedded_path.as_str(),
                            &encode_sha1(embedded_sha1),
                            embedded_bytes.len() as i64,
                        )),
                        without_download_source(profile_library(
                            "example:terminal:1",
                            terminal_path.as_str(),
                            &encode_sha1(terminal_sha1),
                            terminal_bytes.len() as i64,
                        )),
                    ],
                    vec![(embedded_path.clone(), embedded_bytes.clone())],
                    vec![(
                        terminal_path.clone(),
                        terminal_sha1,
                        Some(terminal_bytes.len() as u64),
                    )],
                ),
                crate::rules::default_environment(),
            )
            .expect("retained reconstruction declarations");
            let (pending, jobs) = bound.into_reconstruction_jobs();
            assert!(jobs.is_empty());
            pending
                .complete_network(Vec::new())
                .expect("no network sources")
        };

        assert!(matches!(
            build().seal_declared_terminal_outputs(true),
            Err(SealedLibraryDeclarationError::MissingDeclaration)
        ));
        let (declarations, sources) = build()
            .seal_observed_terminal_outputs(
                VerifiedProcessorOutputs::from_test_terminal(vec![(
                    terminal_path.clone(),
                    terminal_bytes,
                )]),
                true,
            )
            .expect("observed retained reconstruction sources");
        assert_eq!(declarations.len(), 2);
        assert_eq!(sources.len(), 2);
        assert_eq!(
            sources
                .iter()
                .map(|source| source.relative_path().clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([embedded_path, terminal_path])
        );
    }

    #[test]
    fn installer_classifier_rejects_aliases_collisions_missing_and_extra_producers() {
        let environment = crate::rules::default_environment();
        let first = profile_library(
            "example:first:1",
            "Example/shared/1/shared-1.jar",
            &encode_sha1([1; 20]),
            7,
        );
        let second = profile_library(
            "example:second:1",
            "example/shared/1/shared-1.jar",
            &encode_sha1([2; 20]),
            8,
        );
        assert!(matches!(
            bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![first, second],
                    Vec::new(),
                    Vec::new(),
                ),
                environment.clone(),
            ),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));

        let selected_path = PortableRelativePath::new("example/selected/1/selected-1.jar").unwrap();
        let bytes = b"one producer".to_vec();
        let sha1: [u8; 20] = Sha1::digest(&bytes).into();
        let selected = without_download_source(profile_library(
            "example:selected:1",
            selected_path.as_str(),
            &encode_sha1(sha1),
            bytes.len() as i64,
        ));
        assert!(matches!(
            bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![selected.clone()],
                    vec![(selected_path.clone(), bytes.clone())],
                    vec![(selected_path.clone(), sha1, Some(bytes.len() as u64))],
                ),
                environment.clone(),
            ),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));
        assert!(matches!(
            bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![selected],
                    Vec::new(),
                    Vec::new(),
                ),
                environment.clone(),
            ),
            Err(SealedLibraryDeclarationError::MissingStreamSource)
        ));
        let unselected = PortableRelativePath::new("example/other/1/other-1.jar").unwrap();
        assert!(matches!(
            bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![profile_library(
                        "example:network:1",
                        "example/network/1/network-1.jar",
                        "",
                        0,
                    )],
                    Vec::new(),
                    vec![(unselected, [3; 20], None)],
                ),
                environment,
            ),
            Err(SealedLibraryDeclarationError::ExtraDeclaration)
        ));

        let embedded_path = PortableRelativePath::new("example/embedded/1/embedded-1.jar").unwrap();
        let embedded_bytes = b"embedded only".to_vec();
        let embedded_sha1: [u8; 20] = Sha1::digest(&embedded_bytes).into();
        let bound = bind_installer_library_declarations(
            AuthenticatedInstallerLibraryInputs::from_test(
                vec![without_download_source(profile_library(
                    "example:embedded:1",
                    embedded_path.as_str(),
                    &encode_sha1(embedded_sha1),
                    embedded_bytes.len() as i64,
                ))],
                vec![(embedded_path, embedded_bytes)],
                Vec::new(),
            ),
            crate::rules::default_environment(),
        )
        .unwrap();
        let (pending, jobs) = bound.into_network_jobs().unwrap();
        assert!(jobs.is_empty());
        let pending = pending.complete_network(Vec::new()).unwrap();
        let extra_output = PortableRelativePath::new("example/output/1/output-1.jar").unwrap();
        assert!(matches!(
            pending.seal_terminal_outputs(VerifiedProcessorOutputs::from_test_terminal(vec![(
                extra_output,
                b"unselected output".to_vec(),
            )])),
            Err(SealedLibraryDeclarationError::ExtraDeclaration)
        ));
    }

    #[test]
    fn installer_network_completion_rejects_missing_duplicate_extra_and_source_contract_drift() {
        let build = || {
            let bound = bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![profile_library(
                        "example:fresh:1",
                        "example/fresh/1/fresh-1.jar",
                        "",
                        0,
                    )],
                    Vec::new(),
                    Vec::new(),
                ),
                crate::rules::default_environment(),
            )
            .unwrap();
            bound.into_network_jobs().unwrap()
        };
        let (pending, _jobs) = build();
        assert!(matches!(
            pending.complete_network(Vec::new()),
            Err(SealedLibraryDeclarationError::MissingDeclaration)
        ));
        let (_, jobs) = build();
        let valid = retained_network_source(&jobs[0], [4; 20], 9);
        let (pending, _) = build();
        let duplicate = RetainedLibraryComponentSource::from_test_identity(
            jobs[0].job.relative_path.clone(),
            false,
            jobs[0].job.url.clone(),
            jobs[0].job.expected.clone(),
            9,
            [4; 20],
        );
        assert!(matches!(
            pending.complete_network(vec![valid, duplicate]),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));
        let (pending, jobs) = build();
        let valid = retained_network_source(&jobs[0], [4; 20], 9);
        let extra = RetainedLibraryComponentSource::from_test_identity(
            PortableRelativePath::new("example/extra/1/extra-1.jar").unwrap(),
            false,
            jobs[0].job.url.clone(),
            jobs[0].job.expected.clone(),
            9,
            [4; 20],
        );
        assert!(matches!(
            pending.complete_network(vec![valid, extra]),
            Err(SealedLibraryDeclarationError::ExtraDeclaration)
        ));
        for drift in 0..3 {
            let (pending, jobs) = build();
            let job = &jobs[0].job;
            let source = RetainedLibraryComponentSource::from_test_identity(
                job.relative_path.clone(),
                drift == 1,
                if drift == 0 {
                    "https://different.invalid/library.jar".to_string()
                } else {
                    job.url.clone()
                },
                if drift == 2 {
                    ExpectedIntegrity {
                        size: Some(1),
                        sha1: None,
                    }
                } else {
                    job.expected.clone()
                },
                9,
                [4; 20],
            );
            assert!(matches!(
                pending.complete_network(vec![source]),
                Err(SealedLibraryDeclarationError::ContractDrift)
            ));
        }

        for (sha1, size) in [([8; 20], 7), ([7; 20], 8)] {
            let path = "example/exact/1/exact-1.jar";
            let bound = bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![profile_library(
                        "example:exact:1",
                        path,
                        &encode_sha1([7; 20]),
                        7,
                    )],
                    Vec::new(),
                    Vec::new(),
                ),
                crate::rules::default_environment(),
            )
            .unwrap();
            let (pending, jobs) = bound.into_network_jobs().unwrap();
            assert!(matches!(
                pending.complete_network(vec![retained_network_source(&jobs[0], sha1, size)]),
                Err(SealedLibraryDeclarationError::ContractDrift)
            ));
        }
    }

    #[test]
    fn installer_component_source_union_rejects_missing_extra_duplicate_alias_and_contract_drift() {
        let build = || {
            let path = PortableRelativePath::new("Example/embedded/1/embedded-1.jar").unwrap();
            let bytes = b"selected embedded".to_vec();
            let sha1: [u8; 20] = Sha1::digest(&bytes).into();
            let library = without_download_source(profile_library(
                "example:embedded:1",
                path.as_str(),
                &encode_sha1(sha1),
                bytes.len() as i64,
            ));
            let bound = bind_installer_library_declarations(
                AuthenticatedInstallerLibraryInputs::from_test(
                    vec![library],
                    vec![(path.clone(), bytes.clone())],
                    Vec::new(),
                ),
                crate::rules::default_environment(),
            )
            .unwrap();
            let (pending, jobs) = bound.into_network_jobs().unwrap();
            assert!(jobs.is_empty());
            let pending = pending.complete_network(Vec::new()).unwrap();
            let sealed = pending.seal_without_terminal_outputs().unwrap();
            let (declarations, sources) = sealed.into_parts();
            assert_eq!(sources.len(), 1);
            (declarations, path, bytes, sha1)
        };
        let (declarations, _, _, _) = build();
        assert!(matches!(
            validate_installer_source_union(&declarations, &[]),
            Err(SealedLibraryDeclarationError::MissingDeclaration)
        ));

        let (declarations, _, _, _) = build();
        let extra_bytes = b"foreign".to_vec();
        let extra_sha1 = Sha1::digest(&extra_bytes).into();
        let extra = RetainedLibraryComponentSource::from_authenticated_local_bytes(
            PortableRelativePath::new("example/extra/1/extra-1.jar").unwrap(),
            LibraryComponentSourceKind::Library,
            extra_bytes,
            7,
            extra_sha1,
        )
        .unwrap();
        assert!(matches!(
            validate_installer_source_union(&declarations, &[extra]),
            Err(SealedLibraryDeclarationError::ExtraDeclaration)
        ));

        let source_for =
            |path: PortableRelativePath, kind: LibraryComponentSourceKind, bytes: Vec<u8>| {
                let size = bytes.len() as u64;
                let sha1 = Sha1::digest(&bytes).into();
                RetainedLibraryComponentSource::from_authenticated_local_bytes(
                    path, kind, bytes, size, sha1,
                )
                .unwrap()
            };
        let (declarations, path, bytes, _) = build();
        let first = source_for(
            path.clone(),
            LibraryComponentSourceKind::Library,
            bytes.clone(),
        );
        let duplicate = source_for(
            path.clone(),
            LibraryComponentSourceKind::Library,
            bytes.clone(),
        );
        assert!(matches!(
            validate_installer_source_union(&declarations, &[first, duplicate]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let first = source_for(path, LibraryComponentSourceKind::Library, bytes.clone());
        let alias = source_for(
            PortableRelativePath::new("example/embedded/1/embedded-1.jar").unwrap(),
            LibraryComponentSourceKind::Library,
            bytes.clone(),
        );
        assert!(matches!(
            validate_installer_source_union(&declarations, &[first, alias]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let (declarations, path, bytes, _) = build();
        let wrong_kind = source_for(path, LibraryComponentSourceKind::NativeLibrary, bytes);
        assert!(matches!(
            validate_installer_source_union(&declarations, &[wrong_kind]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let (declarations, path, mut bytes, _) = build();
        bytes[0] ^= 1;
        let wrong_integrity = source_for(path, LibraryComponentSourceKind::Library, bytes);
        assert!(matches!(
            validate_installer_source_union(&declarations, &[wrong_integrity]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));
    }

    #[test]
    fn installer_terminal_without_processor_size_uses_verified_output_size() {
        let environment = crate::rules::default_environment();
        let path = PortableRelativePath::new("example/terminal/1/terminal-1.jar").unwrap();
        let bytes = b"verified terminal output".to_vec();
        let sha1: [u8; 20] = Sha1::digest(&bytes).into();
        let library = profile_library(
            "example:terminal:1",
            path.as_str(),
            &encode_sha1(sha1),
            bytes.len() as i64,
        );
        let bound = bind_installer_library_declarations(
            AuthenticatedInstallerLibraryInputs::from_test(
                vec![library],
                Vec::new(),
                vec![(path.clone(), sha1, None)],
            ),
            environment,
        )
        .expect("terminal classification");
        let (pending, jobs) = bound.into_network_jobs().expect("network transition");
        assert!(jobs.is_empty());
        let pending = pending.complete_network(Vec::new()).expect("no network");
        let sealed = pending
            .seal_terminal_outputs(VerifiedProcessorOutputs::from_test_terminal(vec![(
                path.clone(),
                bytes.clone(),
            )]))
            .expect("verified terminal output");
        let (sealed, sources) = sealed.into_parts();
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sealed.get(&path),
            Some((SealedLibraryKind::Library, sha1, bytes.len() as u64, None,))
        );
    }
}
