use super::api::validate_loader_build_record_identity;
use super::compose::LoaderProfileFragment;
use super::source::VerifiedLoaderSource;
use super::types::{
    LoaderArtifactKind, LoaderBuildRecord, LoaderComponentId, LoaderInstallSource,
    LoaderInstallStrategy,
};
use crate::portable_path::{PortablePathKey, PortableRelativePath};
use crate::download::{DownloadError, library_artifact_plans_for};
use crate::known_good_libraries::{
    BoundInstallerLibraryDeclarations, ClassifiedLibraryDownload, ClassifiedLibraryReconstruction,
    PendingInstallerNetworkDeclarations, PendingInstallerReconstructionDeclarations,
    PendingInstallerReconstructionTerminalDeclarations, PendingInstallerTerminalDeclarations,
    SealedExactLibraryDeclarations, SealedInstallerLibrarySources,
    bind_installer_library_declarations,
};
use crate::launch::{Library, maven_to_path};
use crate::rules::default_environment;
use serde::{Deserialize, Deserializer, de};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::Path;
use thiserror::Error;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

#[cfg(not(test))]
const MAX_INSTALLER_PROFILE_ENTRY_BYTES: u64 = 8 << 20;
#[cfg(test)]
const MAX_INSTALLER_PROFILE_ENTRY_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_INSTALLER_EMBEDDED_ENTRY_BYTES: u64 = 128 << 20;
#[cfg(test)]
const MAX_INSTALLER_EMBEDDED_ENTRY_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_INSTALLER_ENTRY_COUNT: usize = 65_536;
#[cfg(test)]
const MAX_INSTALLER_ENTRY_COUNT: usize = 64;
#[cfg(not(test))]
const MAX_INSTALLER_EMBEDDED_TOTAL_BYTES: u64 = 512 << 20;
#[cfg(test)]
const MAX_INSTALLER_EMBEDDED_TOTAL_BYTES: u64 = 4096;
#[cfg(not(test))]
const MAX_FORGE_PROCESSORS: usize = 256;
#[cfg(test)]
const MAX_FORGE_PROCESSORS: usize = 8;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_DATA: usize = 256;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_DATA: usize = 8;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_OUTPUTS: usize = 1024;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_OUTPUTS: usize = 16;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_CLASSPATH: usize = 256;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_CLASSPATH: usize = 16;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_ARGS: usize = 256;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_ARGS: usize = 16;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_STRING_BYTES: usize = 4096;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_STRING_BYTES: usize = 256;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_DECLARATION_BYTES: usize = 2 << 20;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_DECLARATION_BYTES: usize = 4096;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_DATA_ENTRY_BYTES: u64 = 128 << 20;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_DATA_ENTRY_BYTES: u64 = 1024;
#[cfg(not(test))]
const MAX_FORGE_PROCESSOR_DATA_TOTAL_BYTES: u64 = 512 << 20;
#[cfg(test)]
const MAX_FORGE_PROCESSOR_DATA_TOTAL_BYTES: u64 = 4096;

#[derive(Debug, Error)]
pub enum ForgeInstallerError {
    #[error("open installer zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("installer io: {0}")]
    Io(#[from] std::io::Error),
    #[error("installer json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("version.json not found in installer")]
    MissingVersionJson,
    #[error("invalid installer entry path")]
    InvalidEntryPath,
    #[error("installer entry {name} is too large")]
    EntryTooLarge { name: String },
    #[error("installer contains too many entries")]
    TooManyEntries,
    #[error("installer embedded entries exceed the aggregate size limit")]
    EmbeddedEntriesTooLarge,
    #[error("installer contains a duplicate entry: {name}")]
    DuplicateEntry { name: String },
    #[error("installer declares a missing embedded entry: {name}")]
    MissingDeclaredEntry { name: String },
    #[error("installer contains conflicting embedded Maven artifacts")]
    ConflictingEmbeddedArtifact,
    #[error("installer contains conflicting declarations for library {name}")]
    ConflictingLibraryDeclaration { name: String },
    #[error("installer contains an undeclared embedded Maven artifact: {name}")]
    UndeclaredEmbeddedArtifact { name: String },
    #[error("installer contains portable case-fold path aliases")]
    PortablePathAlias,
    #[error("installer identity does not match the live loader build")]
    IdentityMismatch,
    #[error("Forge installer declares too many processors")]
    TooManyForgeProcessors,
    #[error("Forge installer declares too many processor data entries")]
    TooManyForgeProcessorData,
    #[error("Forge installer declares too many processor outputs")]
    TooManyForgeProcessorOutputs,
    #[error("Forge processor declarations exceed their structural limits")]
    ForgeProcessorDeclarationsTooLarge,
    #[error("Forge processor declaration is invalid")]
    InvalidForgeProcessor,
    #[error("Forge processor data declaration is invalid")]
    InvalidForgeProcessorData,
    #[error("Forge processor installer data entry is missing")]
    MissingForgeProcessorData,
    #[error("Forge processor installer data entry is too large")]
    ForgeProcessorDataEntryTooLarge,
    #[error("Forge processor installer data exceeds the aggregate size limit")]
    ForgeProcessorDataTooLarge,
    #[error("Forge client processor does not declare authenticated outputs")]
    MissingForgeProcessorOutputs,
    #[error("Forge processor output declaration is invalid")]
    InvalidForgeProcessorOutput,
    #[error("multiple Forge processors declare the same output")]
    MultipleForgeProcessorProducers,
    #[error("Forge processor declarations contain a portable case-fold alias")]
    ForgeProcessorPortableAlias,
    #[error("Forge processor declarations contain a dependency cycle")]
    ForgeProcessorDependencyCycle,
    #[error("Forge processor input does not have an authenticated artifact contract")]
    InvalidForgeProcessorArtifactContract,
    #[error("Forge processor terminal output does not match the final library inventory")]
    InvalidForgeProcessorFinalOutput,
    #[error("download failed: {0}")]
    Download(#[from] DownloadError),
}

#[derive(Debug)]
pub(crate) struct AuthenticatedForgeInstallerPlan {
    source: VerifiedLoaderSource,
    version: LoaderProfileFragment,
    install_profile_json: Option<Vec<u8>>,
    libraries: Vec<Library>,
    embedded_maven_artifacts: Vec<AuthenticatedEmbeddedMavenArtifact>,
    strip_client_meta: bool,
}

pub(crate) struct BoundForgeInstallerPlan {
    authenticated: AuthenticatedForgeInstallerPlan,
    processor_disposition: BoundProcessorDisposition,
}

pub(crate) struct BoundForgeProcessorExecution {
    plan: BoundProcessorPlan,
    continuation: BoundForgeInstallerContinuation,
}

pub(crate) enum BoundForgeInstallExecution {
    Run(Box<BoundForgeProcessorExecution>),
    Continue(Box<BoundForgeInstallerContinuation>),
    UnsupportedMissingOutputs,
}

pub(crate) struct BoundForgeInstallerContinuation {
    source: VerifiedLoaderSource,
    version: LoaderProfileFragment,
    library_declarations: InstallerDeclarationState,
    strip_client_meta: bool,
}

enum InstallerDeclarationState {
    Bound(BoundInstallerLibraryDeclarations),
    NetworkPending(PendingInstallerNetworkDeclarations),
    TerminalPending(PendingInstallerTerminalDeclarations),
    ReconstructionPending(PendingInstallerReconstructionDeclarations),
    ReconstructionTerminal(PendingInstallerReconstructionTerminalDeclarations),
}

pub(crate) struct PendingForgeNetworkInstall {
    execution: BoundForgeInstallExecution,
    jobs: Vec<ClassifiedLibraryDownload>,
}

pub(crate) struct PendingForgeInstallExecution {
    execution: BoundForgeInstallExecution,
}

pub(crate) struct PendingForgeReconstructionSources {
    execution: BoundForgeInstallExecution,
    jobs: Vec<ClassifiedLibraryReconstruction>,
    required_execution_inputs: BTreeSet<PortableRelativePath>,
}

pub(crate) struct PendingForgeReconstructionExecution {
    execution: BoundForgeInstallExecution,
}

impl BoundForgeInstallExecution {
    pub(crate) fn into_network_install(
        self,
    ) -> Result<PendingForgeNetworkInstall, ForgeInstallerError> {
        let (execution, jobs) = match self {
            Self::Run(execution) => {
                let BoundForgeProcessorExecution { plan, continuation } = *execution;
                let (continuation, jobs) = continuation.into_network_install()?;
                (
                    Self::Run(Box::new(BoundForgeProcessorExecution {
                        plan,
                        continuation,
                    })),
                    jobs,
                )
            }
            Self::Continue(continuation) => {
                let (continuation, jobs) = continuation.into_network_install()?;
                (Self::Continue(Box::new(continuation)), jobs)
            }
            Self::UnsupportedMissingOutputs => {
                return Err(ForgeInstallerError::MissingForgeProcessorOutputs);
            }
        };
        Ok(PendingForgeNetworkInstall { execution, jobs })
    }

    pub(crate) fn into_reconstruction_sources(
        self,
    ) -> Result<PendingForgeReconstructionSources, ForgeInstallerError> {
        let (execution, jobs) = match self {
            Self::Run(execution) => {
                let BoundForgeProcessorExecution { plan, continuation } = *execution;
                let (continuation, jobs) = continuation.into_reconstruction_sources()?;
                (
                    Self::Run(Box::new(BoundForgeProcessorExecution {
                        plan,
                        continuation,
                    })),
                    jobs,
                )
            }
            Self::Continue(continuation) => {
                let (continuation, jobs) = continuation.into_reconstruction_sources()?;
                (Self::Continue(Box::new(continuation)), jobs)
            }
            Self::UnsupportedMissingOutputs => {
                return Err(ForgeInstallerError::MissingForgeProcessorOutputs);
            }
        };
        let required_execution_inputs = match &execution {
            Self::Run(execution) => execution
                .plan
                .input_artifacts
                .iter()
                .filter(|(_, contract)| {
                    matches!(contract.source, BoundProcessorInputSource::Download)
                })
                .map(|(path, _)| path.clone())
                .collect(),
            Self::Continue(_) | Self::UnsupportedMissingOutputs => BTreeSet::new(),
        };
        Ok(PendingForgeReconstructionSources {
            execution,
            jobs,
            required_execution_inputs,
        })
    }
}

impl PendingForgeNetworkInstall {
    pub(crate) fn into_parts(
        self,
    ) -> (PendingForgeInstallExecution, Vec<ClassifiedLibraryDownload>) {
        (
            PendingForgeInstallExecution {
                execution: self.execution,
            },
            self.jobs,
        )
    }
}

impl PendingForgeInstallExecution {
    pub(crate) fn complete_network(
        self,
        sources: Vec<crate::download::library_source::RetainedLibraryComponentSource>,
    ) -> Result<BoundForgeInstallExecution, ForgeInstallerError> {
        match self.execution {
            BoundForgeInstallExecution::Run(execution) => {
                let BoundForgeProcessorExecution { plan, continuation } = *execution;
                let continuation = continuation.complete_network(sources)?;
                Ok(BoundForgeInstallExecution::Run(Box::new(
                    BoundForgeProcessorExecution { plan, continuation },
                )))
            }
            BoundForgeInstallExecution::Continue(continuation) => {
                let continuation = continuation.complete_network(sources)?;
                Ok(BoundForgeInstallExecution::Continue(Box::new(continuation)))
            }
            BoundForgeInstallExecution::UnsupportedMissingOutputs => {
                Err(ForgeInstallerError::MissingForgeProcessorOutputs)
            }
        }
    }
}

impl PendingForgeReconstructionSources {
    pub(crate) fn into_parts(
        self,
    ) -> (
        PendingForgeReconstructionExecution,
        Vec<ClassifiedLibraryReconstruction>,
        BTreeSet<PortableRelativePath>,
    ) {
        (
            PendingForgeReconstructionExecution {
                execution: self.execution,
            },
            self.jobs,
            self.required_execution_inputs,
        )
    }
}

impl PendingForgeReconstructionExecution {
    pub(crate) fn complete_sources(
        self,
        streamed: Vec<crate::download::ExactLibraryDownloadProof>,
    ) -> Result<BoundForgeInstallExecution, ForgeInstallerError> {
        match self.execution {
            BoundForgeInstallExecution::Run(execution) => {
                let BoundForgeProcessorExecution { plan, continuation } = *execution;
                let continuation = continuation.complete_reconstruction_sources(streamed)?;
                Ok(BoundForgeInstallExecution::Run(Box::new(
                    BoundForgeProcessorExecution { plan, continuation },
                )))
            }
            BoundForgeInstallExecution::Continue(continuation) => {
                let continuation = continuation.complete_reconstruction_sources(streamed)?;
                Ok(BoundForgeInstallExecution::Continue(Box::new(continuation)))
            }
            BoundForgeInstallExecution::UnsupportedMissingOutputs => {
                Err(ForgeInstallerError::MissingForgeProcessorOutputs)
            }
        }
    }
}

pub(crate) struct VerifiedInstallerReceiptSource {
    source: VerifiedLoaderSource,
    version: LoaderProfileFragment,
    strip_client_meta: bool,
}

pub(crate) struct AuthenticatedInstallerReceiptInput {
    source: VerifiedInstallerReceiptSource,
    libraries: SealedInstallerLibrarySources,
}

pub(crate) struct AuthenticatedInstallerReconstructionInput {
    source: VerifiedInstallerReceiptSource,
    library_declarations: SealedExactLibraryDeclarations,
    local_library_sources: Vec<crate::download::library_source::AuthenticatedLocalLibraryBytes>,
}

pub(crate) struct VerifiedInstallerClientBytes {
    bytes: Vec<u8>,
    transformation: InstallerClientTransformation,
    base_size: u64,
    base_sha1: [u8; 20],
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InstallerClientTransformation {
    ExactBase,
    StripSignedMetadata,
}

pub(crate) enum BoundProcessorDisposition {
    TypedRunnable(BoundProcessorPlan),
    LegacyNoClientWork,
    EmptyClientWork,
    UnsupportedMissingOutputs,
}

pub(crate) struct BoundProcessorPlan {
    pub(super) steps: Vec<BoundProcessorStep>,
    pub(super) data: BTreeMap<String, BoundProcessorData>,
    pub(super) installer_data: BTreeMap<PortableRelativePath, Vec<u8>>,
    pub(super) input_artifacts: BTreeMap<PortableRelativePath, BoundProcessorInputContract>,
}

pub(super) struct BoundProcessorStep {
    pub(super) jar: BoundProcessorArtifact,
    pub(super) classpath: Vec<BoundProcessorArtifact>,
    pub(super) args: Vec<BoundProcessorArgument>,
    pub(super) outputs: Vec<BoundProcessorOutput>,
}

pub(super) enum BoundProcessorArgument {
    Artifact(BoundProcessorArtifact),
    OutputArtifact(BoundProcessorArtifact),
    Template(Vec<BoundProcessorArgumentPart>),
}

pub(super) enum BoundProcessorArgumentPart {
    Literal(String),
    DataToken(String),
    OutputToken(String),
    BuiltinToken(ProcessorBuiltinToken),
}

#[derive(Clone, Copy)]
pub(super) enum ProcessorBuiltinToken {
    MinecraftJar,
    Side,
    MinecraftVersion,
    Root,
    LibraryDir,
    Installer,
}

#[derive(Clone)]
pub(super) struct BoundProcessorArtifact {
    pub(super) coordinate: String,
    pub(super) relative_path: PortableRelativePath,
}

pub(super) enum BoundProcessorData {
    Artifact(BoundProcessorArtifact),
    InstallerData(PortableRelativePath),
    Literal(String),
}

pub(super) struct BoundProcessorOutput {
    pub(super) artifact: BoundProcessorArtifact,
    pub(super) sha1: [u8; 20],
    pub(super) role: BoundProcessorOutputRole,
}

#[derive(Clone)]
pub(super) struct BoundProcessorInputContract {
    pub(super) sha1: [u8; 20],
    pub(super) size: Option<u64>,
    pub(super) source: BoundProcessorInputSource,
}

#[derive(Clone, Copy)]
pub(super) enum BoundProcessorInputSource {
    Download,
    Embedded,
}

pub(super) enum BoundProcessorNetworkInput {
    Retained(crate::download::library_source::RetainedLibrarySourceReplay),
    ReconstructionWorkspace,
}

#[derive(Clone, Copy)]
pub(super) enum BoundProcessorOutputRole {
    Intermediate,
    Terminal { expected_size: Option<u64> },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedEmbeddedMavenArtifact {
    relative_path: PortableRelativePath,
    bytes: Vec<u8>,
}

pub(crate) struct AuthenticatedInstallerLibraryInputs {
    libraries: Vec<Library>,
    embedded_artifacts: Vec<AuthenticatedEmbeddedMavenArtifact>,
    terminal_outputs: Vec<(PortableRelativePath, [u8; 20], Option<u64>)>,
}

pub(crate) struct AuthenticatedInstallerLibraryParts {
    pub(crate) libraries: Vec<Library>,
    pub(crate) embedded_artifacts: Vec<AuthenticatedEmbeddedMavenArtifact>,
    pub(crate) terminal_outputs: Vec<(PortableRelativePath, [u8; 20], Option<u64>)>,
}

impl AuthenticatedInstallerLibraryInputs {
    pub(crate) fn into_parts(self) -> AuthenticatedInstallerLibraryParts {
        AuthenticatedInstallerLibraryParts {
            libraries: self.libraries,
            embedded_artifacts: self.embedded_artifacts,
            terminal_outputs: self.terminal_outputs,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_test(
        libraries: Vec<Library>,
        embedded_artifacts: Vec<(PortableRelativePath, Vec<u8>)>,
        terminal_outputs: Vec<(PortableRelativePath, [u8; 20], Option<u64>)>,
    ) -> Self {
        Self {
            libraries,
            embedded_artifacts: embedded_artifacts
                .into_iter()
                .map(
                    |(relative_path, bytes)| AuthenticatedEmbeddedMavenArtifact {
                        relative_path,
                        bytes,
                    },
                )
                .collect(),
            terminal_outputs,
        }
    }
}

#[cfg(test)]
impl AuthenticatedForgeInstallerPlan {
    pub(crate) fn source_bytes(&self) -> &[u8] {
        self.source.bytes()
    }

    pub(crate) fn libraries(&self) -> &[Library] {
        &self.libraries
    }

    pub(crate) fn embedded_maven_artifacts(&self) -> &[AuthenticatedEmbeddedMavenArtifact] {
        &self.embedded_maven_artifacts
    }

    pub(crate) fn strip_client_meta(&self) -> bool {
        self.strip_client_meta
    }
}

impl BoundForgeInstallerPlan {
    #[cfg(test)]
    pub(crate) fn processor_disposition(&self) -> &BoundProcessorDisposition {
        &self.processor_disposition
    }

    pub(crate) fn into_install_execution(
        self,
    ) -> Result<BoundForgeInstallExecution, ForgeInstallerError> {
        let Self {
            authenticated,
            processor_disposition,
        } = self;
        let terminal_outputs = match &processor_disposition {
            BoundProcessorDisposition::TypedRunnable(plan) => plan
                .steps
                .iter()
                .flat_map(|step| &step.outputs)
                .filter_map(|output| match output.role {
                    BoundProcessorOutputRole::Intermediate => None,
                    BoundProcessorOutputRole::Terminal { expected_size } => Some((
                        output.artifact.relative_path.clone(),
                        output.sha1,
                        expected_size,
                    )),
                })
                .collect(),
            BoundProcessorDisposition::LegacyNoClientWork
            | BoundProcessorDisposition::EmptyClientWork
            | BoundProcessorDisposition::UnsupportedMissingOutputs => Vec::new(),
        };
        let AuthenticatedForgeInstallerPlan {
            source,
            version,
            install_profile_json: _,
            libraries,
            embedded_maven_artifacts,
            strip_client_meta,
        } = authenticated;
        let library_declarations = bind_installer_library_declarations(
            AuthenticatedInstallerLibraryInputs {
                libraries,
                embedded_artifacts: embedded_maven_artifacts,
                terminal_outputs,
            },
            default_environment(),
        )
        .map_err(|_| ForgeInstallerError::InvalidForgeProcessorArtifactContract)?;
        let continuation = BoundForgeInstallerContinuation {
            source,
            version,
            library_declarations: InstallerDeclarationState::Bound(library_declarations),
            strip_client_meta,
        };
        Ok(match processor_disposition {
            BoundProcessorDisposition::TypedRunnable(plan) => {
                BoundForgeInstallExecution::Run(Box::new(BoundForgeProcessorExecution {
                    plan,
                    continuation,
                }))
            }
            BoundProcessorDisposition::LegacyNoClientWork
            | BoundProcessorDisposition::EmptyClientWork => {
                BoundForgeInstallExecution::Continue(Box::new(continuation))
            }
            BoundProcessorDisposition::UnsupportedMissingOutputs => {
                BoundForgeInstallExecution::UnsupportedMissingOutputs
            }
        })
    }
}

impl BoundForgeProcessorExecution {
    fn has_exact_terminal_declarations(&self) -> bool {
        let mut saw_terminal = false;
        for output in self.plan.steps.iter().flat_map(|step| &step.outputs) {
            match output.role {
                BoundProcessorOutputRole::Intermediate => {}
                BoundProcessorOutputRole::Terminal {
                    expected_size: Some(_),
                } => saw_terminal = true,
                BoundProcessorOutputRole::Terminal {
                    expected_size: None,
                } => return false,
            }
        }
        saw_terminal
    }

    pub(super) fn into_parts(self) -> (BoundForgeInstallerContinuation, BoundProcessorPlan) {
        (self.continuation, self.plan)
    }

    pub(crate) fn into_declared_reconstruction(
        self: Box<Self>,
    ) -> Result<BoundForgeInstallerContinuation, Box<Self>> {
        if !self.has_exact_terminal_declarations() {
            return Err(self);
        }
        Ok(self.continuation)
    }
}

impl BoundForgeInstallerContinuation {
    pub(super) fn source_bytes(&self) -> &[u8] {
        self.source.bytes()
    }

    pub(super) fn embedded_maven_artifact(
        &self,
        path: &PortableRelativePath,
    ) -> Option<&AuthenticatedEmbeddedMavenArtifact> {
        match &self.library_declarations {
            InstallerDeclarationState::NetworkPending(declarations) => {
                declarations.embedded_maven_artifact(path)
            }
            InstallerDeclarationState::TerminalPending(declarations) => {
                declarations.embedded_maven_artifact(path)
            }
            InstallerDeclarationState::ReconstructionPending(declarations) => {
                declarations.embedded_maven_artifact(path)
            }
            InstallerDeclarationState::ReconstructionTerminal(declarations) => {
                declarations.embedded_maven_artifact(path)
            }
            InstallerDeclarationState::Bound(_) => None,
        }
    }

    pub(super) fn network_input_source(
        &self,
        path: &PortableRelativePath,
    ) -> Result<BoundProcessorNetworkInput, crate::loaders::types::LoaderError> {
        match &self.library_declarations {
            InstallerDeclarationState::TerminalPending(declarations) => declarations
                .replay_network_source(path)?
                .map(BoundProcessorNetworkInput::Retained)
                .ok_or_else(|| {
                    crate::loaders::types::LoaderError::Verify(
                        "retained processor network source is missing".to_string(),
                    )
                }),
            InstallerDeclarationState::ReconstructionTerminal(_) => {
                Ok(BoundProcessorNetworkInput::ReconstructionWorkspace)
            }
            InstallerDeclarationState::Bound(_)
            | InstallerDeclarationState::NetworkPending(_)
            | InstallerDeclarationState::ReconstructionPending(_) => {
                Err(crate::loaders::types::LoaderError::Verify(
                    "processor network source authority is incomplete".to_string(),
                ))
            }
        }
    }

    pub(super) fn matches_execution_identity(
        &self,
        target_version_id: &str,
        minecraft_version: &str,
    ) -> bool {
        self.version.inherits_from == minecraft_version
            && self.source.matches_logical_identity(target_version_id)
    }

    pub(crate) fn into_receipt_input(
        self,
    ) -> Result<AuthenticatedInstallerReceiptInput, ForgeInstallerError> {
        self.into_receipt_input_inner(None)
    }

    pub(crate) fn into_observed_receipt_input(
        self,
        outputs: crate::loaders::VerifiedProcessorOutputs,
    ) -> Result<AuthenticatedInstallerReceiptInput, ForgeInstallerError> {
        self.into_receipt_input_inner(Some(outputs))
    }

    fn into_receipt_input_inner(
        self,
        outputs: Option<crate::loaders::VerifiedProcessorOutputs>,
    ) -> Result<AuthenticatedInstallerReceiptInput, ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::TerminalPending(library_declarations) = library_declarations
        else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let sealed = match outputs {
            Some(outputs) => library_declarations.seal_terminal_outputs(outputs),
            None => library_declarations.seal_without_terminal_outputs(),
        };
        let libraries =
            sealed.map_err(|_| ForgeInstallerError::InvalidForgeProcessorFinalOutput)?;
        Ok(AuthenticatedInstallerReceiptInput {
            source: VerifiedInstallerReceiptSource {
                source,
                version,
                strip_client_meta,
            },
            libraries,
        })
    }

    pub(crate) fn into_reconstruction_receipt_input(
        self,
        retain_library_sources: bool,
    ) -> Result<AuthenticatedInstallerReconstructionInput, ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::ReconstructionTerminal(library_declarations) =
            library_declarations
        else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let (library_declarations, local_library_sources) = library_declarations
            .seal_declared_terminal_outputs(retain_library_sources)
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorFinalOutput)?;
        Ok(AuthenticatedInstallerReconstructionInput {
            source: VerifiedInstallerReceiptSource {
                source,
                version,
                strip_client_meta,
            },
            library_declarations,
            local_library_sources,
        })
    }

    pub(crate) fn into_observed_reconstruction_receipt_input(
        self,
        outputs: crate::loaders::VerifiedProcessorOutputs,
        retain_library_sources: bool,
    ) -> Result<AuthenticatedInstallerReconstructionInput, ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::ReconstructionTerminal(library_declarations) =
            library_declarations
        else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let (library_declarations, local_library_sources) = library_declarations
            .seal_observed_terminal_outputs(outputs, retain_library_sources)
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorFinalOutput)?;
        Ok(AuthenticatedInstallerReconstructionInput {
            source: VerifiedInstallerReceiptSource {
                source,
                version,
                strip_client_meta,
            },
            library_declarations,
            local_library_sources,
        })
    }

    fn into_network_install(
        self,
    ) -> Result<(Self, Vec<ClassifiedLibraryDownload>), ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::Bound(library_declarations) = library_declarations else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let (library_declarations, jobs) = library_declarations
            .into_network_jobs()
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorArtifactContract)?;
        Ok((
            Self {
                source,
                version,
                library_declarations: InstallerDeclarationState::NetworkPending(
                    library_declarations,
                ),
                strip_client_meta,
            },
            jobs,
        ))
    }

    fn into_reconstruction_sources(
        self,
    ) -> Result<(Self, Vec<ClassifiedLibraryReconstruction>), ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::Bound(library_declarations) = library_declarations else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let (library_declarations, jobs) = library_declarations.into_reconstruction_jobs();
        Ok((
            Self {
                source,
                version,
                library_declarations: InstallerDeclarationState::ReconstructionPending(
                    library_declarations,
                ),
                strip_client_meta,
            },
            jobs,
        ))
    }

    fn complete_network(
        self,
        sources: Vec<crate::download::library_source::RetainedLibraryComponentSource>,
    ) -> Result<Self, ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::NetworkPending(library_declarations) = library_declarations
        else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let library_declarations = library_declarations
            .complete_network(sources)
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorArtifactContract)?;
        Ok(Self {
            source,
            version,
            library_declarations: InstallerDeclarationState::TerminalPending(library_declarations),
            strip_client_meta,
        })
    }

    fn complete_reconstruction_sources(
        self,
        streamed: Vec<crate::download::ExactLibraryDownloadProof>,
    ) -> Result<Self, ForgeInstallerError> {
        let Self {
            source,
            version,
            library_declarations,
            strip_client_meta,
        } = self;
        let InstallerDeclarationState::ReconstructionPending(library_declarations) =
            library_declarations
        else {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        };
        let library_declarations = library_declarations
            .complete_network(streamed)
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorArtifactContract)?;
        Ok(Self {
            source,
            version,
            library_declarations: InstallerDeclarationState::ReconstructionTerminal(
                library_declarations,
            ),
            strip_client_meta,
        })
    }
}

impl AuthenticatedInstallerReceiptInput {
    pub(crate) fn version(&self) -> &LoaderProfileFragment {
        self.source.version()
    }

    pub(crate) fn derive_child_client_bytes(
        &self,
        authenticated_base_bytes: &[u8],
    ) -> Result<VerifiedInstallerClientBytes, ForgeInstallerError> {
        self.source
            .derive_child_client_bytes(authenticated_base_bytes)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        VerifiedInstallerReceiptSource,
        SealedInstallerLibrarySources,
    ) {
        (self.source, self.libraries)
    }
}

impl AuthenticatedInstallerReconstructionInput {
    pub(crate) fn version(&self) -> &LoaderProfileFragment {
        self.source.version()
    }

    pub(crate) fn derive_child_client_bytes(
        &self,
        authenticated_base_bytes: &[u8],
    ) -> Result<VerifiedInstallerClientBytes, ForgeInstallerError> {
        self.source
            .derive_child_client_bytes(authenticated_base_bytes)
    }

    pub(crate) fn take_local_library_sources(
        &mut self,
    ) -> Vec<crate::download::library_source::AuthenticatedLocalLibraryBytes> {
        std::mem::take(&mut self.local_library_sources)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        VerifiedInstallerReceiptSource,
        SealedExactLibraryDeclarations,
        Vec<crate::download::library_source::AuthenticatedLocalLibraryBytes>,
    ) {
        (
            self.source,
            self.library_declarations,
            self.local_library_sources,
        )
    }
}

impl VerifiedInstallerReceiptSource {
    pub(crate) fn source_bytes(&self) -> &[u8] {
        self.source.bytes()
    }

    pub(crate) fn version(&self) -> &LoaderProfileFragment {
        &self.version
    }

    pub(crate) fn matches_record(&self, record: &LoaderBuildRecord) -> bool {
        matches!(
            &record.install_source,
            LoaderInstallSource::InstallerJar { url }
                if self.source.matches_contract(url, &record.version_id)
        )
    }

    pub(crate) fn derive_child_client_bytes(
        &self,
        authenticated_base_bytes: &[u8],
    ) -> Result<VerifiedInstallerClientBytes, ForgeInstallerError> {
        let (bytes, transformation) = if self.strip_client_meta {
            (
                strip_signed_metadata_in_memory(authenticated_base_bytes, "Minecraft client")?,
                InstallerClientTransformation::StripSignedMetadata,
            )
        } else {
            (
                authenticated_base_bytes.to_vec(),
                InstallerClientTransformation::ExactBase,
            )
        };
        Ok(VerifiedInstallerClientBytes {
            bytes,
            transformation,
            base_size: authenticated_base_bytes.len() as u64,
            base_sha1: Sha1::digest(authenticated_base_bytes).into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_test(version_libraries: Vec<Library>, strip_client_meta: bool) -> Self {
        let version = LoaderProfileFragment {
            main_class: "example.LoaderMain".to_string(),
            libraries: version_libraries,
            ..LoaderProfileFragment::default()
        };
        Self {
            source: VerifiedLoaderSource::from_test_bytes(b"verified installer".to_vec()),
            version,
            strip_client_meta,
        }
    }
}

impl VerifiedInstallerClientBytes {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn matches_derivation(
        &self,
        source: &VerifiedInstallerReceiptSource,
        authenticated_base_bytes: &[u8],
    ) -> bool {
        let actual_base_sha1: [u8; 20] = Sha1::digest(authenticated_base_bytes).into();
        self.base_size == authenticated_base_bytes.len() as u64
            && self.base_sha1 == actual_base_sha1
            && self.transformation
                == if source.strip_client_meta {
                    InstallerClientTransformation::StripSignedMetadata
                } else {
                    InstallerClientTransformation::ExactBase
                }
    }
}

impl AuthenticatedEmbeddedMavenArtifact {
    pub(crate) fn relative_path(&self) -> &PortableRelativePath {
        &self.relative_path
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_parts(self) -> (PortableRelativePath, Vec<u8>) {
        (self.relative_path, self.bytes)
    }
}

#[derive(Debug, Deserialize)]
struct LegacyInstallProfile {
    install: LegacyInstallData,
    #[serde(default)]
    minecraft: String,
    #[serde(rename = "versionInfo")]
    version_info: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct LegacyInstallData {
    path: String,
    #[serde(rename = "filePath")]
    file_path: String,
    target: String,
    #[serde(default)]
    minecraft: String,
    #[serde(default, rename = "stripMeta")]
    strip_meta: bool,
}

#[derive(Debug, Deserialize)]
struct InstallProfileDeclarations {
    #[serde(default)]
    spec: Option<i32>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    minecraft: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    libraries: Vec<Library>,
    #[serde(default)]
    processors: Vec<ProcessorDeclaration>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    data: BTreeMap<String, ProcessorDataDeclaration>,
}

#[derive(Debug, Deserialize)]
struct ProcessorDeclaration {
    #[serde(default)]
    jar: String,
    #[serde(default)]
    classpath: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    sides: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    outputs: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ProcessorDataDeclaration {
    #[serde(default)]
    client: String,
}

fn deserialize_unique_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<V>(PhantomData<V>);

    impl<'de, V> de::Visitor<'de> for UniqueMapVisitor<V>
    where
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<String, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map with unique keys")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if values.insert(key, value).is_some() {
                    return Err(de::Error::custom("duplicate map key"));
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

pub(crate) fn plan_authenticated_installer(
    source: VerifiedLoaderSource,
) -> Result<AuthenticatedForgeInstallerPlan, ForgeInstallerError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(source.bytes()))?;
    if archive.len() > MAX_INSTALLER_ENTRY_COUNT {
        return Err(ForgeInstallerError::TooManyEntries);
    }

    let mut authored_version_json = None;
    let mut install_profile_json = None;
    let mut embedded_candidates = Vec::new();
    let mut embedded_casefold = HashMap::new();
    let mut source_entry_counts = HashMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        *source_entry_counts.entry(name.clone()).or_insert(0_usize) += 1;
        match name.as_str() {
            "version.json" => {
                if authored_version_json.is_some() {
                    return Err(ForgeInstallerError::DuplicateEntry { name });
                }
                authored_version_json = Some(read_bounded_entry(
                    &mut entry,
                    "version.json",
                    MAX_INSTALLER_PROFILE_ENTRY_BYTES,
                )?);
            }
            "install_profile.json" => {
                if install_profile_json.is_some() {
                    return Err(ForgeInstallerError::DuplicateEntry { name });
                }
                install_profile_json = Some(read_bounded_entry(
                    &mut entry,
                    "install_profile.json",
                    MAX_INSTALLER_PROFILE_ENTRY_BYTES,
                )?);
            }
            _ => {
                let Some(relative) = name.strip_prefix("maven/") else {
                    continue;
                };
                if relative.is_empty() || entry.is_dir() || relative.ends_with('/') {
                    continue;
                }
                let path = PortableRelativePath::new_exact(relative)
                    .map_err(|_| ForgeInstallerError::InvalidEntryPath)?;
                match insert_portable_path(&mut embedded_casefold, &path)? {
                    true => {}
                    false => return Err(ForgeInstallerError::DuplicateEntry { name }),
                }
                embedded_candidates.push((index, path, relative.to_string()));
            }
        }
    }

    let effective_version_json = match (
        authored_version_json.as_ref(),
        install_profile_json.as_deref(),
    ) {
        (Some(version_json), _) => version_json.clone(),
        (None, Some(profile)) => extract_legacy_version_info(profile)?,
        (None, None) => return Err(ForgeInstallerError::MissingVersionJson),
    };
    let version_fragment =
        serde_json::from_slice::<LoaderProfileFragment>(&effective_version_json)?;
    let install_info = install_profile_json
        .as_deref()
        .map(serde_json::from_slice::<InstallProfileDeclarations>)
        .transpose()?;
    let libraries = merge_libraries_by_name(
        &version_fragment.libraries,
        install_info
            .as_ref()
            .map(|info| info.libraries.as_slice())
            .unwrap_or(&[]),
    )?;
    let mut allowed = declared_embedded_maven_paths(&libraries, install_info.as_ref())?;
    let legacy_profile = install_profile_json
        .as_deref()
        .and_then(|profile| serde_json::from_slice::<LegacyInstallProfile>(profile).ok());
    let legacy_path = legacy_profile
        .as_ref()
        .map(legacy_root_artifact_path)
        .transpose()?;
    if let Some(path) = legacy_path.as_ref() {
        insert_portable_path(&mut allowed, path)?;
    }

    let mut embedded = BTreeMap::new();
    let mut embedded_total = 0_u64;
    for (index, path, source_name) in embedded_candidates {
        if !allowed.contains_key(&portable_path_key(&path)) {
            return Err(ForgeInstallerError::UndeclaredEmbeddedArtifact { name: source_name });
        }
        let mut entry = archive.by_index(index)?;
        let bytes = read_embedded_entry(&mut entry, &source_name, &mut embedded_total)?;
        embedded.insert(path, bytes);
    }
    if let (Some(profile), Some(path)) = (legacy_profile.as_ref(), legacy_path) {
        add_legacy_root_artifact(
            &mut archive,
            profile,
            path,
            &source_entry_counts,
            &mut embedded_casefold,
            &mut embedded,
            &mut embedded_total,
        )?;
    }
    drop(archive);

    Ok(AuthenticatedForgeInstallerPlan {
        source,
        version: version_fragment,
        install_profile_json,
        libraries,
        embedded_maven_artifacts: embedded
            .into_iter()
            .map(
                |(relative_path, bytes)| AuthenticatedEmbeddedMavenArtifact {
                    relative_path,
                    bytes,
                },
            )
            .collect(),
        strip_client_meta: legacy_profile.is_some_and(|profile| profile.install.strip_meta),
    })
}

pub(crate) fn bind_authenticated_installer_plan(
    mut authenticated: AuthenticatedForgeInstallerPlan,
    record: &LoaderBuildRecord,
) -> Result<BoundForgeInstallerPlan, ForgeInstallerError> {
    if validate_loader_build_record_identity(record).is_err()
        || record.component_name != record.component_id.display_name()
        || record.artifact_kind != LoaderArtifactKind::InstallerJar
        || !matches!(
            &record.install_source,
            LoaderInstallSource::InstallerJar { url } if !url.is_empty()
        )
    {
        return Err(ForgeInstallerError::IdentityMismatch);
    }
    let install_profile = authenticated
        .install_profile_json
        .as_deref()
        .map(serde_json::from_slice::<InstallProfileDeclarations>)
        .transpose()?;
    let legacy_profile = authenticated
        .install_profile_json
        .as_deref()
        .and_then(|profile| serde_json::from_slice::<LegacyInstallProfile>(profile).ok());
    if legacy_profile.is_some()
        && install_profile
            .as_ref()
            .is_some_and(|profile| !profile.processors.is_empty() || !profile.data.is_empty())
    {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }

    let root_artifact = match (record.component_id, record.strategy) {
        (LoaderComponentId::Forge, LoaderInstallStrategy::ForgeModern) => {
            if legacy_profile.is_some() {
                return Err(ForgeInstallerError::IdentityMismatch);
            }
            validate_effective_version_identity(
                &mut authenticated.version,
                record,
                EffectiveProfileShape::Modern,
            )?;
            let expected_version = format!(
                "{}-forge-{}",
                record.minecraft_version, record.loader_version
            );
            let expected_path = format!(
                "net.minecraftforge:forge:{}-{}:shim",
                record.minecraft_version, record.loader_version
            );
            validate_modern_install_profile(
                install_profile.as_ref(),
                record,
                1,
                "forge",
                &expected_version,
                Some(&expected_path),
            )?;
            RootArtifact::Universal
        }
        (LoaderComponentId::NeoForge, LoaderInstallStrategy::NeoForgeModern) => {
            if legacy_profile.is_some() {
                return Err(ForgeInstallerError::IdentityMismatch);
            }
            validate_effective_version_identity(
                &mut authenticated.version,
                record,
                EffectiveProfileShape::Modern,
            )?;
            validate_modern_install_profile(
                install_profile.as_ref(),
                record,
                1,
                "NeoForge",
                &format!("neoforge-{}", record.loader_version),
                None,
            )?;
            RootArtifact::Universal
        }
        (LoaderComponentId::Forge, LoaderInstallStrategy::ForgeLegacyInstaller) => {
            if let Some(legacy_profile) = legacy_profile.as_ref() {
                validate_effective_version_identity(
                    &mut authenticated.version,
                    record,
                    EffectiveProfileShape::LegacyVersionInfo,
                )?;
                validate_legacy_install_profile(legacy_profile, record, &authenticated.version)?;
                RootArtifact::Universal
            } else {
                validate_effective_version_identity(
                    &mut authenticated.version,
                    record,
                    EffectiveProfileShape::Modern,
                )?;
                let expected_version = format!(
                    "{}-forge-{}",
                    record.minecraft_version, record.loader_version
                );
                let expected_path = format!(
                    "net.minecraftforge:forge:{}-{}",
                    record.minecraft_version, record.loader_version
                );
                validate_modern_install_profile(
                    install_profile.as_ref(),
                    record,
                    0,
                    "forge",
                    &expected_version,
                    Some(&expected_path),
                )?;
                RootArtifact::Plain
            }
        }
        _ => return Err(ForgeInstallerError::IdentityMismatch),
    };
    validate_component_root_libraries(&authenticated.libraries, record, root_artifact)?;
    let processor_disposition = if legacy_profile.is_some() {
        BoundProcessorDisposition::LegacyNoClientWork
    } else {
        let profile = install_profile
            .as_ref()
            .ok_or(ForgeInstallerError::IdentityMismatch)?;
        validate_processor_disposition_bounds(profile)?;
        let mut has_client_work = false;
        let mut has_missing_client_outputs = false;
        for processor in &profile.processors {
            if validate_processor_sides(&processor.sides)? {
                has_client_work = true;
                has_missing_client_outputs |= processor.outputs.is_empty();
            }
        }
        if !has_client_work {
            BoundProcessorDisposition::EmptyClientWork
        } else if record.component_id == LoaderComponentId::NeoForge && has_missing_client_outputs {
            BoundProcessorDisposition::UnsupportedMissingOutputs
        } else {
            BoundProcessorDisposition::TypedRunnable(bind_forge_processor_plan(
                authenticated.source.bytes(),
                profile,
                &authenticated.libraries,
                &authenticated.embedded_maven_artifacts,
            )?)
        }
    };

    Ok(BoundForgeInstallerPlan {
        authenticated,
        processor_disposition,
    })
}

impl BoundProcessorPlan {
    fn is_structurally_complete(&self) -> bool {
        self.steps.iter().all(|step| {
            processor_artifact_is_valid(&step.jar)
                && step.classpath.iter().all(processor_artifact_is_valid)
                && step.args.iter().all(|arg| match arg {
                    BoundProcessorArgument::Artifact(artifact)
                    | BoundProcessorArgument::OutputArtifact(artifact) => {
                        processor_artifact_is_valid(artifact)
                    }
                    BoundProcessorArgument::Template(parts) => {
                        !parts.is_empty()
                            && parts.iter().all(|part| match part {
                                BoundProcessorArgumentPart::Literal(value) => !value.is_empty(),
                                BoundProcessorArgumentPart::DataToken(token) => {
                                    self.data.contains_key(token)
                                }
                                BoundProcessorArgumentPart::OutputToken(token) => {
                                    self.data.contains_key(token)
                                }
                                BoundProcessorArgumentPart::BuiltinToken(token) => matches!(
                                    token,
                                    ProcessorBuiltinToken::MinecraftJar
                                        | ProcessorBuiltinToken::Side
                                        | ProcessorBuiltinToken::MinecraftVersion
                                        | ProcessorBuiltinToken::Root
                                        | ProcessorBuiltinToken::LibraryDir
                                        | ProcessorBuiltinToken::Installer
                                ),
                            })
                    }
                })
                && !step.outputs.is_empty()
                && step.outputs.iter().all(|output| {
                    processor_artifact_is_valid(&output.artifact)
                        && match output.role {
                            BoundProcessorOutputRole::Intermediate => true,
                            BoundProcessorOutputRole::Terminal { expected_size } => {
                                expected_size.is_none_or(|size| size > 0)
                            }
                        }
                })
        }) && self.data.iter().all(|(key, value)| {
            valid_processor_token(key)
                && match value {
                    BoundProcessorData::Artifact(artifact) => processor_artifact_is_valid(artifact),
                    BoundProcessorData::InstallerData(path) => {
                        self.installer_data.contains_key(path)
                    }
                    BoundProcessorData::Literal(value) => !value.is_empty(),
                }
        }) && self.installer_data.iter().all(|(path, bytes)| {
            !path.as_str().is_empty() && bytes.len() as u64 <= MAX_FORGE_PROCESSOR_DATA_ENTRY_BYTES
        }) && self.input_artifacts.iter().all(|(path, contract)| {
            !path.as_str().is_empty()
                && contract.size.is_none_or(|size| size > 0)
                && matches!(
                    contract.source,
                    BoundProcessorInputSource::Download | BoundProcessorInputSource::Embedded
                )
        })
    }
}

fn processor_artifact_is_valid(artifact: &BoundProcessorArtifact) -> bool {
    !artifact.coordinate.is_empty() && !artifact.relative_path.as_str().is_empty()
}

fn bind_forge_processor_plan(
    installer: &[u8],
    profile: &InstallProfileDeclarations,
    libraries: &[Library],
    embedded: &[AuthenticatedEmbeddedMavenArtifact],
) -> Result<BoundProcessorPlan, ForgeInstallerError> {
    if profile.processors.len() > MAX_FORGE_PROCESSORS {
        return Err(ForgeInstallerError::TooManyForgeProcessors);
    }
    if profile.data.len() > MAX_FORGE_PROCESSOR_DATA {
        return Err(ForgeInstallerError::TooManyForgeProcessorData);
    }
    let output_count = profile
        .processors
        .iter()
        .try_fold(0_usize, |count, processor| {
            count.checked_add(processor.outputs.len())
        })
        .ok_or(ForgeInstallerError::TooManyForgeProcessorOutputs)?;
    if output_count > MAX_FORGE_PROCESSOR_OUTPUTS {
        return Err(ForgeInstallerError::TooManyForgeProcessorOutputs);
    }
    validate_processor_declaration_bounds(profile)?;
    let artifact_contracts = resolved_processor_artifact_contracts(libraries, embedded)?;

    validate_processor_data_keys(&profile.data)?;
    let referenced_data = referenced_client_processor_data(profile)?;
    let data = referenced_data
        .into_iter()
        .map(|key| {
            let declaration = profile
                .data
                .get(&key)
                .ok_or(ForgeInstallerError::InvalidForgeProcessorData)?;
            parse_processor_data(&declaration.client).map(|value| (key, value))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let requested_data = data
        .values()
        .filter_map(|value| match value {
            BoundProcessorData::InstallerData(path) => Some(path.clone()),
            BoundProcessorData::Artifact(_) | BoundProcessorData::Literal(_) => None,
        })
        .collect::<BTreeSet<_>>();
    let installer_data = extract_authenticated_processor_data(installer, &requested_data)?;

    let mut steps = Vec::new();
    let mut producers = HashMap::<PortablePathKey, (String, usize)>::new();
    let mut dependencies = Vec::<BTreeMap<PortablePathKey, String>>::new();
    for declaration in &profile.processors {
        let is_client = validate_processor_sides(&declaration.sides)?;
        if !is_client {
            continue;
        }
        if declaration.outputs.is_empty() {
            return Err(ForgeInstallerError::MissingForgeProcessorOutputs);
        }

        let jar = parse_processor_artifact(&declaration.jar)?;
        let classpath = declaration
            .classpath
            .iter()
            .map(|coordinate| parse_processor_artifact(coordinate))
            .collect::<Result<Vec<_>, _>>()?;
        let step_index = steps.len();
        let mut outputs = Vec::with_capacity(declaration.outputs.len());
        let mut current_outputs = BTreeMap::new();
        for (target, sha1) in &declaration.outputs {
            let artifact = resolve_processor_output_artifact(target, &data)?;
            let sha1 = resolve_processor_output_sha1(sha1, &data)?;
            let portable = portable_path_key(&artifact.relative_path);
            let output = BoundProcessorOutput {
                artifact,
                sha1,
                role: BoundProcessorOutputRole::Intermediate,
            };
            match producers.get(&portable) {
                Some((existing, _)) if existing == output.artifact.relative_path.as_str() => {
                    return Err(ForgeInstallerError::MultipleForgeProcessorProducers);
                }
                Some(_) => return Err(ForgeInstallerError::ForgeProcessorPortableAlias),
                None => {
                    producers.insert(
                        portable.clone(),
                        (
                            output.artifact.relative_path.as_str().to_string(),
                            step_index,
                        ),
                    );
                }
            }
            current_outputs.insert(portable, output.artifact.relative_path.as_str().to_string());
            outputs.push(output);
        }

        let mut consumed = BTreeMap::new();
        record_processor_dependency(&mut consumed, &jar)?;
        for artifact in &classpath {
            record_processor_dependency(&mut consumed, artifact)?;
        }
        let args = declaration
            .args
            .iter()
            .map(|arg| parse_processor_argument(arg, &data, &current_outputs, &mut consumed))
            .collect::<Result<Vec<_>, _>>()?;
        dependencies.push(consumed);
        steps.push(BoundProcessorStep {
            jar,
            classpath,
            args,
            outputs,
        });
    }

    let (input_artifacts, consumed_outputs) = bind_processor_dependency_contracts(
        &dependencies,
        &producers,
        &artifact_contracts.external_inputs,
    )?;
    classify_processor_outputs(
        &mut steps,
        &consumed_outputs,
        &artifact_contracts.final_inventory,
    )?;
    let plan = BoundProcessorPlan {
        steps,
        data,
        installer_data,
        input_artifacts,
    };
    if !plan.is_structurally_complete() {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    Ok(plan)
}

fn validate_processor_disposition_bounds(
    profile: &InstallProfileDeclarations,
) -> Result<(), ForgeInstallerError> {
    if profile.processors.len() > MAX_FORGE_PROCESSORS {
        return Err(ForgeInstallerError::TooManyForgeProcessors);
    }
    if profile.data.len() > MAX_FORGE_PROCESSOR_DATA {
        return Err(ForgeInstallerError::TooManyForgeProcessorData);
    }
    let output_count = profile
        .processors
        .iter()
        .try_fold(0_usize, |count, processor| {
            count.checked_add(processor.outputs.len())
        })
        .ok_or(ForgeInstallerError::TooManyForgeProcessorOutputs)?;
    if output_count > MAX_FORGE_PROCESSOR_OUTPUTS {
        return Err(ForgeInstallerError::TooManyForgeProcessorOutputs);
    }
    validate_processor_declaration_bounds(profile)
}

fn validate_processor_declaration_bounds(
    profile: &InstallProfileDeclarations,
) -> Result<(), ForgeInstallerError> {
    let mut total = 0_usize;
    let mut account = |value: &str| -> Result<(), ForgeInstallerError> {
        if value.len() > MAX_FORGE_PROCESSOR_STRING_BYTES {
            return Err(ForgeInstallerError::ForgeProcessorDeclarationsTooLarge);
        }
        total = total
            .checked_add(value.len())
            .ok_or(ForgeInstallerError::ForgeProcessorDeclarationsTooLarge)?;
        if total > MAX_FORGE_PROCESSOR_DECLARATION_BYTES {
            return Err(ForgeInstallerError::ForgeProcessorDeclarationsTooLarge);
        }
        Ok(())
    };
    for processor in &profile.processors {
        if processor.classpath.len() > MAX_FORGE_PROCESSOR_CLASSPATH
            || processor.args.len() > MAX_FORGE_PROCESSOR_ARGS
            || processor.sides.len() > 3
        {
            return Err(ForgeInstallerError::ForgeProcessorDeclarationsTooLarge);
        }
        account(&processor.jar)?;
        for value in processor
            .classpath
            .iter()
            .chain(&processor.args)
            .chain(&processor.sides)
        {
            account(value)?;
        }
        for (target, digest) in &processor.outputs {
            account(target)?;
            account(digest)?;
        }
    }
    for (key, value) in &profile.data {
        account(key)?;
        account(&value.client)?;
    }
    Ok(())
}

struct ResolvedProcessorArtifactContracts {
    external_inputs: HashMap<PortablePathKey, ExactProcessorInputContract>,
    final_inventory: HashMap<PortablePathKey, ExactProcessorFinalContract>,
}

#[derive(Clone)]
struct ExactProcessorInputContract {
    path: PortableRelativePath,
    contract: BoundProcessorInputContract,
}

struct ExactProcessorFinalContract {
    path: PortableRelativePath,
    sha1: Option<[u8; 20]>,
    size: Option<u64>,
}

fn resolved_processor_artifact_contracts(
    libraries: &[Library],
    embedded: &[AuthenticatedEmbeddedMavenArtifact],
) -> Result<ResolvedProcessorArtifactContracts, ForgeInstallerError> {
    let plans = library_artifact_plans_for(libraries, &default_environment())
        .map_err(|_| ForgeInstallerError::InvalidForgeProcessorArtifactContract)?;
    let mut external_inputs = HashMap::new();
    let mut final_inventory = HashMap::new();
    for plan in plans {
        let sha1 = match plan.expected.sha1.as_deref() {
            Some(value) => Some(
                decode_sha1(value)
                    .ok_or(ForgeInstallerError::InvalidForgeProcessorArtifactContract)?,
            ),
            None => None,
        };
        if let Some(sha1) = sha1
            && plan.source_url.is_some()
        {
            let input = ExactProcessorInputContract {
                path: plan.relative_path.clone(),
                contract: BoundProcessorInputContract {
                    sha1,
                    size: plan.expected.size,
                    source: BoundProcessorInputSource::Download,
                },
            };
            insert_exact_input_contract(&mut external_inputs, input)?;
        }
        insert_exact_final_contract(
            &mut final_inventory,
            ExactProcessorFinalContract {
                path: plan.relative_path,
                sha1,
                size: plan.expected.size,
            },
        )?;
    }
    for artifact in embedded {
        let mut sha1 = [0_u8; 20];
        sha1.copy_from_slice(&Sha1::digest(artifact.bytes()));
        let embedded_contract = ExactProcessorInputContract {
            path: artifact.relative_path().clone(),
            contract: BoundProcessorInputContract {
                sha1,
                size: Some(artifact.bytes().len() as u64),
                source: BoundProcessorInputSource::Embedded,
            },
        };
        let portable = portable_path_key(&embedded_contract.path);
        if let Some(final_contract) = final_inventory.get(&portable)
            && (final_contract.path != embedded_contract.path
                || final_contract
                    .sha1
                    .is_some_and(|sha1| sha1 != embedded_contract.contract.sha1)
                || final_contract
                    .size
                    .is_some_and(|size| Some(size) != embedded_contract.contract.size))
        {
            return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
        }
        if let Some(existing) = external_inputs.get(&portable) {
            if existing.path != embedded_contract.path
                || existing.contract.sha1 != embedded_contract.contract.sha1
                || existing
                    .contract
                    .size
                    .is_some_and(|size| Some(size) != embedded_contract.contract.size)
            {
                return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
            }
            external_inputs.insert(portable, embedded_contract);
        } else {
            insert_exact_input_contract(&mut external_inputs, embedded_contract)?;
        }
    }
    Ok(ResolvedProcessorArtifactContracts {
        external_inputs,
        final_inventory,
    })
}

fn insert_exact_input_contract(
    contracts: &mut HashMap<PortablePathKey, ExactProcessorInputContract>,
    contract: ExactProcessorInputContract,
) -> Result<(), ForgeInstallerError> {
    let portable = portable_path_key(&contract.path);
    match contracts.get(&portable) {
        Some(existing) if existing.path != contract.path => {
            Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        }
        Some(existing)
            if existing.contract.sha1 != contract.contract.sha1
                || existing.contract.size != contract.contract.size =>
        {
            Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract)
        }
        Some(_) => Ok(()),
        None => {
            contracts.insert(portable, contract);
            Ok(())
        }
    }
}

fn insert_exact_final_contract(
    contracts: &mut HashMap<PortablePathKey, ExactProcessorFinalContract>,
    contract: ExactProcessorFinalContract,
) -> Result<(), ForgeInstallerError> {
    let portable = portable_path_key(&contract.path);
    match contracts.get(&portable) {
        Some(existing) if existing.path != contract.path => {
            Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        }
        Some(existing) if existing.sha1 != contract.sha1 || existing.size != contract.size => {
            Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract)
        }
        Some(_) => Ok(()),
        None => {
            contracts.insert(portable, contract);
            Ok(())
        }
    }
}

fn validate_processor_data_keys(
    data: &BTreeMap<String, ProcessorDataDeclaration>,
) -> Result<(), ForgeInstallerError> {
    let mut casefold = HashSet::new();
    for key in data.keys() {
        if !valid_processor_token(key)
            || processor_builtin_token(key)
            || !casefold.insert(key.to_ascii_lowercase())
        {
            return Err(ForgeInstallerError::InvalidForgeProcessorData);
        }
    }
    Ok(())
}

fn referenced_client_processor_data(
    profile: &InstallProfileDeclarations,
) -> Result<BTreeSet<String>, ForgeInstallerError> {
    let mut referenced = BTreeSet::new();
    for processor in &profile.processors {
        if !validate_processor_sides(&processor.sides)? {
            continue;
        }
        for value in processor.outputs.keys().chain(processor.outputs.values()) {
            if let Some(token) = exact_delimited(value, '{', '}') {
                if !valid_processor_token(token) {
                    return Err(ForgeInstallerError::InvalidForgeProcessor);
                }
                referenced.insert(token.to_string());
            }
        }
        for arg in &processor.args {
            collect_processor_argument_tokens(arg, &mut referenced)?;
        }
    }
    referenced.retain(|token| !processor_builtin_token(token));
    Ok(referenced)
}

fn collect_processor_argument_tokens(
    arg: &str,
    referenced: &mut BTreeSet<String>,
) -> Result<(), ForgeInstallerError> {
    let mut chars = arg.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        match character {
            '\\' => {
                chars
                    .next()
                    .ok_or(ForgeInstallerError::InvalidForgeProcessor)?;
            }
            '\'' => quoted = !quoted,
            '{' if !quoted => {
                let mut token = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{' | '[' | ']') | None => {
                            return Err(ForgeInstallerError::InvalidForgeProcessor);
                        }
                        Some(value) => token.push(value),
                    }
                }
                if !valid_processor_token(&token) {
                    return Err(ForgeInstallerError::InvalidForgeProcessor);
                }
                referenced.insert(token);
            }
            _ => {}
        }
    }
    if quoted {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    Ok(())
}

fn valid_processor_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn processor_builtin_token(token: &str) -> bool {
    processor_builtin_token_kind(token).is_some()
}

fn processor_builtin_token_kind(token: &str) -> Option<ProcessorBuiltinToken> {
    match token {
        "MINECRAFT_JAR" => Some(ProcessorBuiltinToken::MinecraftJar),
        "SIDE" => Some(ProcessorBuiltinToken::Side),
        "MINECRAFT_VERSION" => Some(ProcessorBuiltinToken::MinecraftVersion),
        "ROOT" => Some(ProcessorBuiltinToken::Root),
        "LIBRARY_DIR" => Some(ProcessorBuiltinToken::LibraryDir),
        "INSTALLER" => Some(ProcessorBuiltinToken::Installer),
        _ => None,
    }
}

fn parse_processor_data(value: &str) -> Result<BoundProcessorData, ForgeInstallerError> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(ForgeInstallerError::InvalidForgeProcessorData);
    }
    if value.starts_with('[') || value.ends_with(']') {
        let coordinate = exact_delimited(value, '[', ']')
            .ok_or(ForgeInstallerError::InvalidForgeProcessorData)?;
        return parse_processor_artifact(coordinate).map(BoundProcessorData::Artifact);
    }
    if let Some(source_path) = value.strip_prefix('/') {
        if source_path.is_empty() || source_path.starts_with('/') || source_path.contains('\\') {
            return Err(ForgeInstallerError::InvalidForgeProcessorData);
        }
        let path = PortableRelativePath::new_exact(source_path)
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorData)?;
        return Ok(BoundProcessorData::InstallerData(path));
    }
    let literal =
        exact_delimited(value, '\'', '\'').ok_or(ForgeInstallerError::InvalidForgeProcessorData)?;
    Ok(BoundProcessorData::Literal(literal.to_string()))
}

fn validate_processor_sides(sides: &[String]) -> Result<bool, ForgeInstallerError> {
    let mut seen = HashSet::new();
    for side in sides {
        if !matches!(side.as_str(), "client" | "server" | "extract") || !seen.insert(side.as_str())
        {
            return Err(ForgeInstallerError::InvalidForgeProcessor);
        }
    }
    Ok(sides.is_empty() || seen.contains("client"))
}

fn parse_processor_artifact(
    coordinate: &str,
) -> Result<BoundProcessorArtifact, ForgeInstallerError> {
    if coordinate.is_empty() || coordinate.trim() != coordinate {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    let mut extension_parts = coordinate.split('@');
    let base = extension_parts
        .next()
        .ok_or(ForgeInstallerError::InvalidForgeProcessor)?;
    if extension_parts
        .next()
        .is_some_and(|extension| extension.is_empty())
        || extension_parts.next().is_some()
    {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    let parts = base.split(':').collect::<Vec<_>>();
    if !matches!(parts.len(), 3 | 4) || parts.iter().any(|part| part.is_empty()) {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    let relative_path = PortableRelativePath::from_path(&maven_to_path(coordinate))
        .map_err(|_| ForgeInstallerError::InvalidForgeProcessor)?;
    Ok(BoundProcessorArtifact {
        coordinate: coordinate.to_string(),
        relative_path,
    })
}

fn parse_processor_argument(
    arg: &str,
    data: &BTreeMap<String, BoundProcessorData>,
    current_outputs: &BTreeMap<PortablePathKey, String>,
    consumed: &mut BTreeMap<PortablePathKey, String>,
) -> Result<BoundProcessorArgument, ForgeInstallerError> {
    if arg.is_empty() || arg.chars().any(char::is_control) {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    if let Some(coordinate) = exact_delimited(arg, '[', ']') {
        let artifact = parse_processor_artifact(coordinate)?;
        let portable = portable_path_key(&artifact.relative_path);
        if let Some(expected) = current_outputs.get(&portable) {
            if expected != artifact.relative_path.as_str() {
                return Err(ForgeInstallerError::ForgeProcessorPortableAlias);
            }
            return Ok(BoundProcessorArgument::OutputArtifact(artifact));
        }
        record_processor_dependency(consumed, &artifact)?;
        return Ok(BoundProcessorArgument::Artifact(artifact));
    }

    let mut chars = arg.chars().peekable();
    let mut quoted = false;
    let mut literal = String::new();
    let mut parts = Vec::new();
    while let Some(character) = chars.next() {
        match character {
            '\\' => {
                literal.push(
                    chars
                        .next()
                        .ok_or(ForgeInstallerError::InvalidForgeProcessor)?,
                );
            }
            '\'' => {
                quoted = !quoted;
            }
            '{' if !quoted => {
                push_processor_literal(&mut parts, &mut literal);
                let mut token = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{' | '[' | ']') | None => {
                            return Err(ForgeInstallerError::InvalidForgeProcessor);
                        }
                        Some(value) => token.push(value),
                    }
                }
                if !valid_processor_token(&token) {
                    return Err(ForgeInstallerError::InvalidForgeProcessor);
                }
                if let Some(value) = data.get(&token) {
                    if let BoundProcessorData::Artifact(artifact) = value {
                        let portable = portable_path_key(&artifact.relative_path);
                        if let Some(expected) = current_outputs.get(&portable) {
                            if expected != artifact.relative_path.as_str() {
                                return Err(ForgeInstallerError::ForgeProcessorPortableAlias);
                            }
                            parts.push(BoundProcessorArgumentPart::OutputToken(token));
                            continue;
                        }
                        record_processor_dependency(consumed, artifact)?;
                    }
                    parts.push(BoundProcessorArgumentPart::DataToken(token));
                } else if let Some(token) = processor_builtin_token_kind(&token) {
                    parts.push(BoundProcessorArgumentPart::BuiltinToken(token));
                } else {
                    return Err(ForgeInstallerError::InvalidForgeProcessor);
                }
            }
            '}' | '[' | ']' if !quoted => {
                return Err(ForgeInstallerError::InvalidForgeProcessor);
            }
            value => literal.push(value),
        }
    }
    if quoted {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    push_processor_literal(&mut parts, &mut literal);
    if parts.is_empty() {
        return Err(ForgeInstallerError::InvalidForgeProcessor);
    }
    Ok(BoundProcessorArgument::Template(parts))
}

fn push_processor_literal(parts: &mut Vec<BoundProcessorArgumentPart>, literal: &mut String) {
    if !literal.is_empty() {
        parts.push(BoundProcessorArgumentPart::Literal(std::mem::take(literal)));
    }
}

fn resolve_processor_output_artifact(
    value: &str,
    data: &BTreeMap<String, BoundProcessorData>,
) -> Result<BoundProcessorArtifact, ForgeInstallerError> {
    if let Some(coordinate) = exact_delimited(value, '[', ']') {
        return parse_processor_artifact(coordinate)
            .map_err(|_| ForgeInstallerError::InvalidForgeProcessorOutput);
    }
    let token = exact_delimited(value, '{', '}')
        .filter(|token| valid_processor_token(token))
        .ok_or(ForgeInstallerError::InvalidForgeProcessorOutput)?;
    match data.get(token) {
        Some(BoundProcessorData::Artifact(artifact)) => Ok(artifact.clone()),
        Some(BoundProcessorData::InstallerData(_) | BoundProcessorData::Literal(_)) | None => {
            Err(ForgeInstallerError::InvalidForgeProcessorOutput)
        }
    }
}

fn resolve_processor_output_sha1(
    value: &str,
    data: &BTreeMap<String, BoundProcessorData>,
) -> Result<[u8; 20], ForgeInstallerError> {
    let declared = if let Some(token) = exact_delimited(value, '{', '}') {
        if !valid_processor_token(token) {
            return Err(ForgeInstallerError::InvalidForgeProcessorOutput);
        }
        match data.get(token) {
            Some(BoundProcessorData::Literal(value)) => value.as_str(),
            Some(BoundProcessorData::Artifact(_) | BoundProcessorData::InstallerData(_)) | None => {
                return Err(ForgeInstallerError::InvalidForgeProcessorOutput);
            }
        }
    } else {
        exact_delimited(value, '\'', '\'')
            .ok_or(ForgeInstallerError::InvalidForgeProcessorOutput)?
    };
    decode_sha1(declared).ok_or(ForgeInstallerError::InvalidForgeProcessorOutput)
}

fn decode_sha1(value: &str) -> Option<[u8; 20]> {
    if value.len() != 40 {
        return None;
    }
    let mut sha1 = [0_u8; 20];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        sha1[index] = hex_nibble(pair[0])
            .checked_mul(16)?
            .checked_add(hex_nibble(pair[1]))?;
    }
    Some(sha1)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => u8::MAX,
    }
}

fn exact_delimited(value: &str, open: char, close: char) -> Option<&str> {
    value
        .strip_prefix(open)?
        .strip_suffix(close)
        .filter(|inner| !inner.is_empty() && !inner.contains(open) && !inner.contains(close))
}

fn bind_processor_dependency_contracts(
    dependencies: &[BTreeMap<PortablePathKey, String>],
    producers: &HashMap<PortablePathKey, (String, usize)>,
    external: &HashMap<PortablePathKey, ExactProcessorInputContract>,
) -> Result<
    (
        BTreeMap<PortableRelativePath, BoundProcessorInputContract>,
        HashSet<PortablePathKey>,
    ),
    ForgeInstallerError,
> {
    let mut edges = vec![Vec::new(); dependencies.len()];
    for (consumer, inputs) in dependencies.iter().enumerate() {
        for input in inputs.keys() {
            if let Some((_, producer)) = producers.get(input) {
                edges[consumer].push(*producer);
            }
        }
    }
    let mut state = vec![0_u8; edges.len()];
    for node in 0..edges.len() {
        if processor_graph_has_cycle(node, &edges, &mut state) {
            return Err(ForgeInstallerError::ForgeProcessorDependencyCycle);
        }
    }
    let mut input_contracts = BTreeMap::new();
    let mut consumed_outputs = HashSet::new();
    for (consumer, inputs) in dependencies.iter().enumerate() {
        for (input, exact) in inputs {
            match producers.get(input) {
                Some((produced, _)) if produced != exact => {
                    return Err(ForgeInstallerError::ForgeProcessorPortableAlias);
                }
                Some((_, producer)) if *producer >= consumer => {
                    return Err(ForgeInstallerError::InvalidForgeProcessor);
                }
                Some(_) => {
                    consumed_outputs.insert(input.clone());
                }
                None if external
                    .get(input)
                    .is_some_and(|contract| contract.path.as_str() == exact) =>
                {
                    let contract = external
                        .get(input)
                        .ok_or(ForgeInstallerError::InvalidForgeProcessorArtifactContract)?;
                    input_contracts.insert(contract.path.clone(), contract.contract.clone());
                }
                None if external.contains_key(input) => {
                    return Err(ForgeInstallerError::ForgeProcessorPortableAlias);
                }
                None => {
                    return Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract);
                }
            }
        }
    }
    Ok((input_contracts, consumed_outputs))
}

fn classify_processor_outputs(
    steps: &mut [BoundProcessorStep],
    consumed_outputs: &HashSet<PortablePathKey>,
    final_inventory: &HashMap<PortablePathKey, ExactProcessorFinalContract>,
) -> Result<(), ForgeInstallerError> {
    for output in steps.iter_mut().flat_map(|step| &mut step.outputs) {
        let portable = portable_path_key(&output.artifact.relative_path);
        let consumed_by_later_step = consumed_outputs.contains(&portable);
        if let Some(contract) = final_inventory.get(&portable) {
            if contract.path != output.artifact.relative_path
                || contract.sha1.is_some_and(|sha1| sha1 != output.sha1)
            {
                return Err(ForgeInstallerError::InvalidForgeProcessorFinalOutput);
            }
            output.role = BoundProcessorOutputRole::Terminal {
                expected_size: contract.size,
            };
        } else if consumed_by_later_step {
            output.role = BoundProcessorOutputRole::Intermediate;
        } else {
            return Err(ForgeInstallerError::InvalidForgeProcessorFinalOutput);
        }
    }
    Ok(())
}

fn record_processor_dependency(
    dependencies: &mut BTreeMap<PortablePathKey, String>,
    artifact: &BoundProcessorArtifact,
) -> Result<(), ForgeInstallerError> {
    let portable = portable_path_key(&artifact.relative_path);
    match dependencies.get(&portable) {
        Some(existing) if existing != artifact.relative_path.as_str() => {
            Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        }
        Some(_) => Ok(()),
        None => {
            dependencies.insert(portable, artifact.relative_path.as_str().to_string());
            Ok(())
        }
    }
}

fn processor_graph_has_cycle(node: usize, edges: &[Vec<usize>], state: &mut [u8]) -> bool {
    match state[node] {
        1 => return true,
        2 => return false,
        _ => {}
    }
    state[node] = 1;
    if edges[node]
        .iter()
        .any(|dependency| processor_graph_has_cycle(*dependency, edges, state))
    {
        return true;
    }
    state[node] = 2;
    false
}

fn extract_authenticated_processor_data(
    installer: &[u8],
    requested: &BTreeSet<PortableRelativePath>,
) -> Result<BTreeMap<PortableRelativePath, Vec<u8>>, ForgeInstallerError> {
    if requested.is_empty() {
        return Ok(BTreeMap::new());
    }
    let requested_portable = requested
        .iter()
        .map(|path| (portable_path_key(path), path.as_str()))
        .collect::<HashMap<_, _>>();
    if requested_portable.len() != requested.len() {
        return Err(ForgeInstallerError::ForgeProcessorPortableAlias);
    }

    let mut archive = ZipArchive::new(std::io::Cursor::new(installer))?;
    let mut located = BTreeMap::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let authored_name = entry.name();
        let Ok(source_path) = PortableRelativePath::new_exact(authored_name) else {
            continue;
        };
        let portable = portable_path_key(&source_path);
        let Some(expected) = requested_portable.get(&portable) else {
            continue;
        };
        if authored_name != *expected || entry.is_dir() || authored_name.ends_with('/') {
            return Err(ForgeInstallerError::ForgeProcessorPortableAlias);
        }
        if located.insert(source_path, index).is_some() {
            return Err(ForgeInstallerError::InvalidForgeProcessorData);
        }
    }
    if located.len() != requested.len() {
        return Err(ForgeInstallerError::MissingForgeProcessorData);
    }

    let mut extracted = BTreeMap::new();
    let mut total = 0_u64;
    for (path, index) in located {
        let mut entry = archive.by_index(index)?;
        if entry.size() > MAX_FORGE_PROCESSOR_DATA_ENTRY_BYTES {
            return Err(ForgeInstallerError::ForgeProcessorDataEntryTooLarge);
        }
        if entry.size() > MAX_FORGE_PROCESSOR_DATA_TOTAL_BYTES.saturating_sub(total) {
            return Err(ForgeInstallerError::ForgeProcessorDataTooLarge);
        }
        let bytes = read_bounded_entry(
            &mut entry,
            "processor data",
            MAX_FORGE_PROCESSOR_DATA_ENTRY_BYTES,
        )
        .map_err(|error| match error {
            ForgeInstallerError::EntryTooLarge { .. } => {
                ForgeInstallerError::ForgeProcessorDataEntryTooLarge
            }
            other => other,
        })?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or(ForgeInstallerError::ForgeProcessorDataTooLarge)?;
        if total > MAX_FORGE_PROCESSOR_DATA_TOTAL_BYTES {
            return Err(ForgeInstallerError::ForgeProcessorDataTooLarge);
        }
        extracted.insert(path, bytes);
    }
    Ok(extracted)
}

#[derive(Clone, Copy)]
enum EffectiveProfileShape {
    Modern,
    LegacyVersionInfo,
}

#[derive(Clone, Copy)]
enum RootArtifact {
    Plain,
    Universal,
}

fn validate_effective_version_identity(
    version: &mut LoaderProfileFragment,
    record: &LoaderBuildRecord,
    shape: EffectiveProfileShape,
) -> Result<(), ForgeInstallerError> {
    let expected_id = match record.component_id {
        LoaderComponentId::Forge => {
            format!(
                "{}-forge-{}",
                record.minecraft_version, record.loader_version
            )
        }
        LoaderComponentId::NeoForge => format!("neoforge-{}", record.loader_version),
        LoaderComponentId::Fabric | LoaderComponentId::Quilt => {
            return Err(ForgeInstallerError::IdentityMismatch);
        }
    };
    let valid_parent = version.inherits_from == record.minecraft_version
        || (matches!(shape, EffectiveProfileShape::LegacyVersionInfo)
            && version.inherits_from.is_empty());
    let valid_assets = version.assets.is_empty()
        || (matches!(shape, EffectiveProfileShape::LegacyVersionInfo)
            && legacy_assets_alias_matches(&version.assets, &record.minecraft_version));
    if version.id != expected_id
        || !valid_parent
        || (!version.kind.is_empty() && version.kind != "release")
        || version.asset_index.is_some()
        || !valid_assets
        || version.downloads.is_some()
        || version.java_version.is_some()
        || version
            .logging
            .as_ref()
            .is_some_and(|logging| *logging != crate::launch::LoggingConf::default())
    {
        return Err(ForgeInstallerError::IdentityMismatch);
    }
    version.inherits_from = record.minecraft_version.clone();
    version.assets.clear();
    version.logging = None;
    Ok(())
}

fn legacy_assets_alias_matches(assets: &str, minecraft_version: &str) -> bool {
    if assets == minecraft_version {
        return true;
    }
    let mut parts = assets.split('.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.chars().all(|value| value.is_ascii_digit())
        && minor.chars().all(|value| value.is_ascii_digit())
        && minecraft_version
            .strip_prefix(assets)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn validate_modern_install_profile(
    profile: Option<&InstallProfileDeclarations>,
    record: &LoaderBuildRecord,
    expected_spec: i32,
    expected_profile: &str,
    expected_version: &str,
    expected_path: Option<&str>,
) -> Result<(), ForgeInstallerError> {
    let profile = profile.ok_or(ForgeInstallerError::IdentityMismatch)?;
    if profile.spec != Some(expected_spec)
        || profile.profile.as_deref() != Some(expected_profile)
        || profile.version.as_deref() != Some(expected_version)
        || profile.minecraft.as_deref() != Some(record.minecraft_version.as_str())
        || profile.path.as_deref() != expected_path
    {
        return Err(ForgeInstallerError::IdentityMismatch);
    }
    Ok(())
}

fn validate_legacy_install_profile(
    profile: &LegacyInstallProfile,
    record: &LoaderBuildRecord,
    version: &LoaderProfileFragment,
) -> Result<(), ForgeInstallerError> {
    let minecraft = legacy_profile_minecraft(profile);
    let expected_version_id = format!(
        "{}-forge-{}",
        record.minecraft_version, record.loader_version
    );
    let historical_target = format!(
        "{}-Forge{}",
        record.minecraft_version, record.loader_version
    );
    let normalized_library = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        minecraft,
    )
    .ok_or(ForgeInstallerError::IdentityMismatch)?;
    let normalized_version_id = normalize_legacy_forge_version_id(&profile.install.path, minecraft)
        .ok_or(ForgeInstallerError::IdentityMismatch)?;
    let expected_root = format!(
        "net.minecraftforge:forge:{}-{}",
        record.minecraft_version, record.loader_version
    );

    if minecraft != record.minecraft_version
        || (!profile.minecraft.is_empty() && profile.minecraft != record.minecraft_version)
        || (!profile.install.minecraft.is_empty()
            && profile.install.minecraft != record.minecraft_version)
        || normalized_version_id != expected_version_id
        || version.id != normalized_version_id
        || !normalized_library.starts_with(&format!("{expected_root}:"))
        || (profile.install.target != expected_version_id
            && profile.install.target != historical_target)
    {
        return Err(ForgeInstallerError::IdentityMismatch);
    }
    Ok(())
}

fn validate_component_root_libraries(
    libraries: &[Library],
    record: &LoaderBuildRecord,
    required_artifact: RootArtifact,
) -> Result<(), ForgeInstallerError> {
    let expected = match record.component_id {
        LoaderComponentId::Forge => format!(
            "net.minecraftforge:forge:{}-{}",
            record.minecraft_version, record.loader_version
        ),
        LoaderComponentId::NeoForge => {
            format!("net.neoforged:neoforge:{}", record.loader_version)
        }
        LoaderComponentId::Fabric | LoaderComponentId::Quilt => {
            return Err(ForgeInstallerError::IdentityMismatch);
        }
    };
    let mut roots = HashSet::new();
    let mut required = 0_usize;
    for library in libraries {
        let Some((root, classifier)) = component_root_coordinate(&library.name)? else {
            continue;
        };
        roots.insert(root.to_string());
        if root != expected {
            return Err(ForgeInstallerError::IdentityMismatch);
        }
        if matches!(
            (required_artifact, classifier),
            (RootArtifact::Plain, None) | (RootArtifact::Universal, Some("universal"))
        ) {
            required += 1;
        }
    }
    if roots.len() != 1 || required != 1 {
        return Err(ForgeInstallerError::IdentityMismatch);
    }
    Ok(())
}

fn component_root_coordinate(
    coordinate: &str,
) -> Result<Option<(&str, Option<&str>)>, ForgeInstallerError> {
    let coordinate = coordinate
        .split_once('@')
        .map_or(coordinate, |(value, _)| value);
    let mut parts = coordinate.split(':');
    let Some(group) = parts.next() else {
        return Ok(None);
    };
    let Some(artifact) = parts.next() else {
        return Ok(None);
    };
    let recognized = matches!(
        (group, artifact),
        ("net.minecraftforge", "forge")
            | ("net.minecraftforge", "minecraftforge")
            | ("net.neoforged", "neoforge")
    );
    if !recognized {
        return Ok(None);
    }
    let version = parts.next().ok_or(ForgeInstallerError::IdentityMismatch)?;
    let classifier = parts.next();
    if version.is_empty() || classifier.is_some_and(str::is_empty) || parts.next().is_some() {
        return Err(ForgeInstallerError::IdentityMismatch);
    }
    let root_len = coordinate
        .rfind(':')
        .filter(|_| classifier.is_some())
        .unwrap_or(coordinate.len());
    Ok(Some((&coordinate[..root_len], classifier)))
}

fn merge_libraries_by_name(
    primary: &[Library],
    secondary: &[Library],
) -> Result<Vec<Library>, ForgeInstallerError> {
    let mut seen = HashMap::new();
    let mut merged = Vec::with_capacity(primary.len() + secondary.len());

    for library in primary.iter().chain(secondary.iter()) {
        if let Some(existing) = seen.get(&library.name) {
            if existing != library {
                return Err(ForgeInstallerError::ConflictingLibraryDeclaration {
                    name: library.name.clone(),
                });
            }
        } else {
            seen.insert(library.name.clone(), library.clone());
            merged.push(library.clone());
        }
    }

    Ok(merged)
}

fn declared_embedded_maven_paths(
    libraries: &[Library],
    install_profile: Option<&InstallProfileDeclarations>,
) -> Result<HashMap<PortablePathKey, String>, ForgeInstallerError> {
    let mut allowed = HashMap::new();
    for library in libraries {
        insert_coordinate_path(&mut allowed, &library.name)?;
        if let Some(downloads) = library.downloads.as_ref() {
            if let Some(artifact) = downloads.artifact.as_ref()
                && !artifact.path.trim().is_empty()
            {
                insert_declared_path(&mut allowed, &artifact.path)?;
            }
            for artifact in downloads.classifiers.values() {
                if !artifact.path.trim().is_empty() {
                    insert_declared_path(&mut allowed, &artifact.path)?;
                }
            }
        }
    }
    if let Some(profile) = install_profile {
        for processor in &profile.processors {
            insert_coordinate_path(&mut allowed, &processor.jar)?;
            for coordinate in &processor.classpath {
                insert_coordinate_path(&mut allowed, coordinate)?;
            }
        }
        for data in profile.data.values() {
            let value = data.client.trim();
            if let Some(coordinate) = value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
            {
                insert_coordinate_path(&mut allowed, coordinate)?;
            }
        }
    }
    Ok(allowed)
}

fn insert_coordinate_path(
    paths: &mut HashMap<PortablePathKey, String>,
    coordinate: &str,
) -> Result<(), ForgeInstallerError> {
    if coordinate.trim().is_empty() {
        return Ok(());
    }
    let path = maven_to_path(coordinate);
    if path.as_os_str().is_empty() {
        return Err(ForgeInstallerError::InvalidEntryPath);
    }
    let path = PortableRelativePath::from_path(&path)
        .map_err(|_| ForgeInstallerError::InvalidEntryPath)?;
    insert_portable_path(paths, &path).map(|_| ())
}

fn insert_declared_path(
    paths: &mut HashMap<PortablePathKey, String>,
    path: &str,
) -> Result<(), ForgeInstallerError> {
    let portable =
        PortableRelativePath::new_exact(path).map_err(|_| ForgeInstallerError::InvalidEntryPath)?;
    insert_portable_path(paths, &portable).map(|_| ())
}

fn insert_portable_path(
    paths: &mut HashMap<PortablePathKey, String>,
    path: &PortableRelativePath,
) -> Result<bool, ForgeInstallerError> {
    let key = portable_path_key(path);
    match paths.get(&key) {
        Some(existing) if existing == path.as_str() => Ok(false),
        Some(_) => Err(ForgeInstallerError::PortablePathAlias),
        None => {
            paths.insert(key, path.as_str().to_string());
            Ok(true)
        }
    }
}

fn portable_path_key(path: &PortableRelativePath) -> PortablePathKey {
    path.key()
}

fn legacy_root_artifact_path(
    profile: &LegacyInstallProfile,
) -> Result<PortableRelativePath, ForgeInstallerError> {
    let minecraft = legacy_profile_minecraft(profile);
    let normalized_library = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        minecraft,
    )
    .ok_or(ForgeInstallerError::InvalidEntryPath)?;
    PortableRelativePath::from_path(&maven_to_path(&normalized_library))
        .map_err(|_| ForgeInstallerError::InvalidEntryPath)
}

fn add_legacy_root_artifact(
    archive: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    profile: &LegacyInstallProfile,
    artifact_path: PortableRelativePath,
    source_entry_counts: &HashMap<String, usize>,
    embedded_casefold: &mut HashMap<PortablePathKey, String>,
    embedded: &mut BTreeMap<PortableRelativePath, Vec<u8>>,
    embedded_total: &mut u64,
) -> Result<(), ForgeInstallerError> {
    let entry_name = profile.install.file_path.trim();
    if entry_name.is_empty() || entry_name.contains('/') || entry_name.contains('\\') {
        return Err(ForgeInstallerError::InvalidEntryPath);
    }
    match source_entry_counts.get(entry_name).copied() {
        Some(1) => {}
        Some(_) => {
            return Err(ForgeInstallerError::DuplicateEntry {
                name: entry_name.to_string(),
            });
        }
        None => {
            return Err(ForgeInstallerError::MissingDeclaredEntry {
                name: entry_name.to_string(),
            });
        }
    }
    let mut entry = archive.by_name(entry_name)?;
    let bytes = read_embedded_entry(&mut entry, entry_name, embedded_total)?;
    let bytes = if profile.install.strip_meta {
        strip_signed_metadata_in_memory(&bytes, entry_name)?
    } else {
        bytes
    };
    if !insert_portable_path(embedded_casefold, &artifact_path)?
        || embedded.insert(artifact_path, bytes).is_some()
    {
        return Err(ForgeInstallerError::ConflictingEmbeddedArtifact);
    }
    Ok(())
}

fn strip_signed_metadata_in_memory(
    data: &[u8],
    name: &str,
) -> Result<Vec<u8>, ForgeInstallerError> {
    let mut source = ZipArchive::new(std::io::Cursor::new(data))?;
    if source.len() > MAX_INSTALLER_ENTRY_COUNT {
        return Err(ForgeInstallerError::TooManyEntries);
    }
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    for index in 0..source.len() {
        let mut entry = source.by_index(index)?;
        let entry_name = entry.name().to_string();
        if legacy_signed_metadata_entry_is_skipped(&entry_name) {
            continue;
        }
        if !seen.insert(entry_name.clone()) {
            return Err(ForgeInstallerError::DuplicateEntry { name: entry_name });
        }
        if entry.is_dir() || entry_name.ends_with('/') {
            writer.add_directory(&entry_name, SimpleFileOptions::default())?;
            continue;
        }

        let bytes = read_embedded_entry(&mut entry, name, &mut total)?;
        writer.start_file(&entry_name, SimpleFileOptions::default())?;
        writer.write_all(&bytes)?;
    }
    let output = writer.finish()?.into_inner();
    if output.len() as u64 > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    Ok(output)
}

fn legacy_signed_metadata_entry_is_skipped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "META-INF/MANIFEST.MF"
        || upper.ends_with(".SF")
        || upper.ends_with(".RSA")
        || upper.ends_with(".DSA")
}

fn read_bounded_entry(
    file: &mut zip::read::ZipFile<'_>,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, ForgeInstallerError> {
    if file.size() > max_bytes {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    let mut data = Vec::new();
    let mut bounded = (&mut *file).take(max_bytes + 1);
    bounded.read_to_end(&mut data)?;
    if data.len() as u64 > max_bytes || data.len() as u64 != file.size() {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    Ok(data)
}

fn read_embedded_entry(
    file: &mut zip::read::ZipFile<'_>,
    name: &str,
    total: &mut u64,
) -> Result<Vec<u8>, ForgeInstallerError> {
    if file.size() > MAX_INSTALLER_EMBEDDED_ENTRY_BYTES {
        return Err(ForgeInstallerError::EntryTooLarge {
            name: name.to_string(),
        });
    }
    if file.size() > MAX_INSTALLER_EMBEDDED_TOTAL_BYTES.saturating_sub(*total) {
        return Err(ForgeInstallerError::EmbeddedEntriesTooLarge);
    }
    let bytes = read_bounded_entry(file, name, MAX_INSTALLER_EMBEDDED_ENTRY_BYTES)?;
    *total = total
        .checked_add(bytes.len() as u64)
        .ok_or(ForgeInstallerError::EmbeddedEntriesTooLarge)?;
    if *total > MAX_INSTALLER_EMBEDDED_TOTAL_BYTES {
        return Err(ForgeInstallerError::EmbeddedEntriesTooLarge);
    }
    Ok(bytes)
}

fn extract_legacy_version_info(install_profile: &[u8]) -> Result<Vec<u8>, ForgeInstallerError> {
    let profile = serde_json::from_slice::<LegacyInstallProfile>(install_profile)?;
    let minecraft = legacy_profile_minecraft(&profile).to_string();
    let mut version_info = profile.version_info;

    if let Some(version_id) = normalize_legacy_forge_version_id(&profile.install.path, &minecraft)
        .or_else(|| (!profile.install.target.is_empty()).then(|| profile.install.target.clone()))
    {
        version_info["id"] = serde_json::Value::String(version_id);
    }

    if let Some(normalized_library) = normalize_legacy_forge_library(
        &profile.install.path,
        &profile.install.file_path,
        &minecraft,
    ) && let Some(libraries) = version_info
        .get_mut("libraries")
        .and_then(|value| value.as_array_mut())
    {
        for library in libraries.iter_mut() {
            if library.get("name").and_then(|value| value.as_str())
                == Some(profile.install.path.as_str())
            {
                library["name"] = serde_json::Value::String(normalized_library.clone());
                break;
            }
        }
    }

    Ok(serde_json::to_vec(&version_info)?)
}

fn legacy_profile_minecraft(profile: &LegacyInstallProfile) -> &str {
    let install_minecraft = profile.install.minecraft.trim();
    if install_minecraft.is_empty() {
        profile.minecraft.trim()
    } else {
        install_minecraft
    }
}

fn normalize_legacy_forge_library(path: &str, file_path: &str, minecraft: &str) -> Option<String> {
    let mut parts = path.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let filename = Path::new(file_path).file_stem()?.to_string_lossy();
    if artifact == "minecraftforge" && !minecraft.trim().is_empty() {
        let classifier = if filename.contains("-universal-") {
            "universal"
        } else if filename.contains("-client-") {
            "client"
        } else if filename.contains("-server-") {
            "server"
        } else {
            return None;
        };
        return Some(format!("{group}:forge:{minecraft}-{version}:{classifier}"));
    }

    let prefix = format!("{artifact}-{version}-");
    let classifier = filename.strip_prefix(&prefix)?;
    if classifier.is_empty() {
        return None;
    }
    Some(format!("{group}:{artifact}:{version}:{classifier}"))
}

fn normalize_legacy_forge_version_id(path: &str, minecraft: &str) -> Option<String> {
    let mut parts = path.split(':');
    let _group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if artifact == "minecraftforge" && !minecraft.trim().is_empty() {
        return Some(format!("{minecraft}-forge-{version}"));
    }
    let index = version.find('-')?;
    if index == 0 || index + 1 >= version.len() {
        return None;
    }
    Some(format!(
        "{}-forge-{}",
        &version[..index],
        &version[index + 1..]
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        AuthenticatedForgeInstallerPlan, BoundForgeInstallExecution, BoundForgeInstallerPlan,
        BoundProcessorDisposition, BoundProcessorNetworkInput, BoundProcessorPlan,
        ForgeInstallerError, MAX_INSTALLER_EMBEDDED_ENTRY_BYTES, MAX_INSTALLER_PROFILE_ENTRY_BYTES,
        bind_authenticated_installer_plan, merge_libraries_by_name, normalize_legacy_forge_library,
        normalize_legacy_forge_version_id, plan_authenticated_installer,
    };
    use crate::portable_path::PortableRelativePath;
    use crate::download::ExactLibraryDownloadProof;
    use crate::known_good_libraries::LibraryAcquisition;
    use crate::launch::Library;
    use crate::loaders::source::VerifiedLoaderSource;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    };
    use crate::loaders::{build_id_for, installed_version_id_for};
    use std::io::{Cursor, Read, Write};
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    fn typed_plan(bound: &BoundForgeInstallerPlan) -> &BoundProcessorPlan {
        match bound.processor_disposition() {
            BoundProcessorDisposition::TypedRunnable(plan) => plan,
            _ => panic!("typed processor plan"),
        }
    }

    fn assert_installer_declaration_transition(bound: BoundForgeInstallerPlan) {
        bound
            .into_install_execution()
            .expect("bind installer declarations")
            .into_network_install()
            .expect("classify installer declarations without I/O");
    }

    #[test]
    fn normalizes_legacy_forge_version_id() {
        assert_eq!(
            normalize_legacy_forge_version_id("net.minecraftforge:forge:1.2.4-2.0.0.68", ""),
            Some("1.2.4-forge-2.0.0.68".to_string())
        );
    }

    #[test]
    fn normalizes_legacy_forge_library_classifier() {
        assert_eq!(
            normalize_legacy_forge_library(
                "net.minecraftforge:forge:1.2.4-2.0.0.68",
                "forge-1.2.4-2.0.0.68-universal.zip",
                ""
            ),
            Some("net.minecraftforge:forge:1.2.4-2.0.0.68:universal".to_string())
        );
    }

    #[test]
    fn normalizes_minecraftforge_legacy_coordinates() {
        assert_eq!(
            normalize_legacy_forge_version_id(
                "net.minecraftforge:minecraftforge:9.11.1.1345",
                "1.6.4"
            ),
            Some("1.6.4-forge-9.11.1.1345".to_string())
        );
        assert_eq!(
            normalize_legacy_forge_library(
                "net.minecraftforge:minecraftforge:9.11.1.1345",
                "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                "1.6.4"
            ),
            Some("net.minecraftforge:forge:1.6.4-9.11.1.1345:universal".to_string())
        );
    }

    #[test]
    fn binds_representative_real_format_forge_neoforge_and_legacy_profiles() {
        let forge = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern,
            "1.21.5",
            "55.0.0",
        );
        let (version, install) = modern_binding_profiles(&forge);
        let bound = bind_modern_fixture_with_entries(&forge, &version, &install, &[])
            .expect("modern Forge binding");
        assert_installer_declaration_transition(bound);

        let neoforge = binding_record(
            LoaderComponentId::NeoForge,
            LoaderInstallStrategy::NeoForgeModern,
            "1.21.5",
            "21.5.74",
        );
        let (version, install) = modern_binding_profiles(&neoforge);
        let bound = bind_modern_fixture_with_entries(&neoforge, &version, &install, &[])
            .expect("modern NeoForge binding");
        assert_installer_declaration_transition(bound);

        let legacy = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeLegacyInstaller,
            "1.7.10",
            "10.13.4.1614-1.7.10",
        );
        let install = legacy_binding_profile(&legacy);
        let bound = bind_legacy_fixture_plan(&legacy, &install).expect("legacy Forge binding");
        assert_installer_declaration_transition(bound);

        let legacy_assets_alias = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeLegacyInstaller,
            "1.8.9",
            "11.15.1.2318-1.8.9",
        );
        let install = legacy_binding_profile(&legacy_assets_alias);
        bind_legacy_fixture(&legacy_assets_alias, &install)
            .expect("legacy Forge base-assets alias binding");

        let later_legacy = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeLegacyInstaller,
            "1.12.2",
            "14.23.5.2859",
        );
        let (version, install) = later_legacy_binding_profiles(&later_legacy);
        let bound = bind_modern_fixture_with_entries(&later_legacy, &version, &install, &[])
            .expect("later legacy Forge binding");
        assert_installer_declaration_transition(bound);
    }

    #[test]
    fn classifies_empty_server_only_and_true_legacy_processor_work() {
        let forge = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern,
            "1.21.5",
            "55.0.0",
        );
        let (version, install) = modern_binding_profiles(&forge);
        let empty = bind_modern_fixture_with_entries(&forge, &version, &install, &[])
            .expect("empty Forge profile");
        assert!(matches!(
            empty.processor_disposition(),
            BoundProcessorDisposition::EmptyClientWork
        ));
        let mut server_only = install;
        server_only["processors"] = serde_json::json!([
            {"sides":["server"]},
            {"sides":["extract"]}
        ]);
        let server_only = bind_modern_fixture_with_entries(&forge, &version, &server_only, &[])
            .expect("server-only Forge profile");
        assert!(matches!(
            server_only.processor_disposition(),
            BoundProcessorDisposition::EmptyClientWork
        ));

        let legacy = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeLegacyInstaller,
            "1.7.10",
            "10.13.4.1614-1.7.10",
        );
        let legacy_profile = legacy_binding_profile(&legacy);
        let legacy_bound =
            bind_legacy_fixture_plan(&legacy, &legacy_profile).expect("true legacy profile");
        assert!(matches!(
            legacy_bound.processor_disposition(),
            BoundProcessorDisposition::LegacyNoClientWork
        ));
        assert!(matches!(
            legacy_bound.into_install_execution(),
            Ok(BoundForgeInstallExecution::Continue(_))
        ));

        let mut hybrid = legacy_profile;
        hybrid["processors"] = serde_json::json!([{"sides":["client"]}]);
        assert!(matches!(
            bind_legacy_fixture(&legacy, &hybrid),
            Err(ForgeInstallerError::InvalidForgeProcessor)
        ));
    }

    #[test]
    fn forge_binding_seals_client_outputs_and_ignores_server_only_processors() {
        let record = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern,
            "1.21.5",
            "55.0.0",
        );
        let (version, mut install) = modern_binding_profiles(&record);
        install["data"] = serde_json::json!({
            "BINPATCH": {"client": "/data/client.lzma"},
            "PATCHED": {"client": "[net.minecraftforge:forge:1.21.5-55.0.0:client]"},
            "PATCHED_SHA": {"client": "'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'"},
            "SERVER_ONLY": {"server": "/data/server.lzma"}
        });
        install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .extend([serde_json::json!({
                "name": "net.minecraftforge:binarypatcher:1.1.1:fatjar",
                "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            })]);
        install["processors"] = serde_json::json!([
            {
                "jar": "net.minecraftforge:binarypatcher:1.1.1:fatjar",
                "args": ["{BINPATCH}", "{PATCHED}"],
                "sides": ["client"],
                "outputs": {"{PATCHED}": "{PATCHED_SHA}"}
            },
            {
                "args": ["{SERVER_ONLY}"],
                "sides": ["server"]
            },
            {
                "args": ["{SERVER_ONLY}"],
                "sides": ["extract"]
            }
        ]);

        let bound = bind_modern_fixture_with_entries(
            &record,
            &version,
            &install,
            &[("data/client.lzma", b"patches")],
        )
        .expect("authenticated Forge processor plan");
        let processor_plan = typed_plan(&bound);
        assert_eq!(processor_plan.steps.len(), 1);
        assert_eq!(processor_plan.steps[0].outputs.len(), 1);
        assert_eq!(processor_plan.installer_data.len(), 1);
    }

    #[test]
    fn forge_binding_rejects_runnable_processor_without_outputs() {
        let record = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern,
            "1.21.5",
            "55.0.0",
        );
        let (version, mut install) = modern_binding_profiles(&record);
        install["processors"] = serde_json::json!([{
            "jar": "net.minecraftforge:binarypatcher:1.1.1:fatjar",
            "sides": ["client"]
        }]);

        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &install, &[]),
            Err(ForgeInstallerError::MissingForgeProcessorOutputs)
        ));
    }

    #[test]
    fn reconstruction_skips_only_processors_with_exact_terminal_sizes() {
        let (record, mut version, install) = forge_processor_fixture();
        let bound = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("incomplete terminal authority");
        let BoundForgeInstallExecution::Run(execution) =
            bound.into_install_execution().expect("bound execution")
        else {
            panic!("typed processor execution");
        };
        assert!(!execution.has_exact_terminal_declarations());

        version["libraries"][1]["sha1"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        version["libraries"][1]["size"] = 9.into();
        let bound = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("exact terminal authority");
        let BoundForgeInstallExecution::Run(execution) =
            bound.into_install_execution().expect("bound execution")
        else {
            panic!("typed processor execution");
        };
        assert!(execution.has_exact_terminal_declarations());

        let bound = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("exact reconstruction authority");
        let sources = bound
            .into_install_execution()
            .expect("bound execution")
            .into_reconstruction_sources()
            .expect("source-only reconstruction transition");
        let (pending, jobs, _) = sources.into_parts();
        let proofs = jobs
            .into_iter()
            .filter_map(|job| {
                let (plan, acquisition) = job.into_parts();
                if acquisition == LibraryAcquisition::ExactDeclaration {
                    return None;
                }
                let sha1 = if plan.expected.sha1.is_some() {
                    [0xbb; 20]
                } else {
                    [5; 20]
                };
                Some(ExactLibraryDownloadProof::new_bound_for_test(
                    plan.relative_path,
                    plan.is_native,
                    plan.source_url.expect("fresh source URL"),
                    plan.expected.clone(),
                    plan.expected.size.unwrap_or(7),
                    sha1,
                ))
            })
            .collect();
        let BoundForgeInstallExecution::Run(execution) = pending
            .complete_sources(proofs)
            .expect("authenticated reconstruction sources")
        else {
            panic!("typed processor reconstruction");
        };
        let processor_path = PortableRelativePath::new("example/processor/1.0/processor-1.0.jar")
            .expect("processor path");
        assert!(matches!(
            execution.continuation.network_input_source(&processor_path),
            Ok(BoundProcessorNetworkInput::ReconstructionWorkspace)
        ));
        let continuation = match execution.into_declared_reconstruction() {
            Ok(continuation) => continuation,
            Err(_) => panic!("exact terminal reconstruction"),
        };
        let input = continuation
            .into_reconstruction_receipt_input(false)
            .expect("declared terminal reconstruction input");
        assert_eq!(
            input
                .library_declarations
                .get(
                    &PortableRelativePath::new(
                        "net/minecraftforge/forge/1.21.5-55.0.0/forge-1.21.5-55.0.0-client.jar",
                    )
                    .expect("terminal path"),
                )
                .map(|(_, _, size, _)| size),
            Some(9)
        );
    }

    #[test]
    fn live_processor_network_input_requires_its_retained_source() {
        let (record, version, install) = forge_processor_fixture();
        let execution = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("live processor authority")
            .into_install_execution()
            .expect("live processor execution");
        let network = execution
            .into_network_install()
            .expect("live network transition");
        let (pending, jobs) = network.into_parts();
        let sources = jobs
            .into_iter()
            .map(|classified| {
                let (job, _) = classified.into_parts();
                let sha1 = if job.expected.sha1.is_some() {
                    [0xbb; 20]
                } else {
                    [5; 20]
                };
                crate::download::library_source::RetainedLibraryComponentSource::from_test_identity(
                    job.relative_path,
                    job.is_native,
                    job.url,
                    job.expected.clone(),
                    job.expected.size.unwrap_or(7),
                    sha1,
                )
            })
            .collect();
        let BoundForgeInstallExecution::Run(execution) = pending
            .complete_network(sources)
            .expect("complete live retained sources")
        else {
            panic!("live processor execution");
        };
        let missing = PortableRelativePath::new("example/missing/1/missing-1.jar")
            .expect("missing processor path");
        assert!(
            execution
                .continuation
                .network_input_source(&missing)
                .is_err()
        );
    }

    #[test]
    fn spec_zero_forge_binds_outputs_while_outputless_neoforge_is_unsupported() {
        let forge = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeLegacyInstaller,
            "1.12.2",
            "14.23.5.2859",
        );
        let (version, mut install) = later_legacy_binding_profiles(&forge);
        install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .push(serde_json::json!({
                "name": "example:processor:1.0",
                "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }));
        install["data"] = serde_json::json!({
            "PATCHED": {"client": "[net.minecraftforge:forge:1.12.2-14.23.5.2859]"},
            "PATCHED_SHA": {"client": "'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'"}
        });
        install["processors"] = serde_json::json!([{
            "jar": "example:processor:1.0",
            "args": ["{PATCHED}"],
            "outputs": {"{PATCHED}": "{PATCHED_SHA}"}
        }]);
        let bound = bind_modern_fixture_with_entries(&forge, &version, &install, &[])
            .expect("spec-zero Forge authority");
        assert!(matches!(
            bound.processor_disposition(),
            BoundProcessorDisposition::TypedRunnable(_)
        ));

        let neo = binding_record(
            LoaderComponentId::NeoForge,
            LoaderInstallStrategy::NeoForgeModern,
            "1.21.5",
            "21.5.74",
        );
        let (version, mut install) = modern_binding_profiles(&neo);
        install["processors"] = serde_json::json!([{
            "jar": "net.neoforged.installertools:installertools:2.1.3"
        }]);
        let bound = bind_modern_fixture_with_entries(&neo, &version, &install, &[])
            .expect("NeoForge semantic binding");
        assert!(matches!(
            bound.processor_disposition(),
            BoundProcessorDisposition::UnsupportedMissingOutputs
        ));
        assert!(matches!(
            bound.into_install_execution(),
            Ok(BoundForgeInstallExecution::UnsupportedMissingOutputs)
        ));
    }

    #[test]
    fn processor_maps_reject_duplicate_keys_without_echoing_them() {
        for json in [
            r#"{"data":{"PRIVATE":{"client":"'a'"},"PRIVATE":{"client":"'b'"}}}"#,
            r#"{"processors":[{"outputs":{"{PRIVATE}":"a","{PRIVATE}":"b"}}]}"#,
        ] {
            let error = serde_json::from_str::<super::InstallProfileDeclarations>(json)
                .expect_err("duplicate map key");
            assert!(!error.to_string().contains("PRIVATE"));
        }
    }

    #[test]
    fn processor_binding_rejects_bad_outputs_sides_and_argument_tokens() {
        let (record, version, install) = forge_processor_fixture();
        let mut cases = Vec::new();

        let mut bad_digest = install.clone();
        bad_digest["processors"][0]["outputs"]["{PATCHED}"] = "not-a-sha1".into();
        cases.push(bad_digest);
        let mut unquoted_digest = install.clone();
        unquoted_digest["processors"][0]["outputs"]["{PATCHED}"] =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        cases.push(unquoted_digest);
        let mut unknown_side = install.clone();
        unknown_side["processors"][0]["sides"] = serde_json::json!(["client", "unknown"]);
        cases.push(unknown_side);
        let mut unknown_token = install.clone();
        unknown_token["processors"][0]["args"] = serde_json::json!(["{UNKNOWN}"]);
        cases.push(unknown_token);
        let mut embedded_artifact = install.clone();
        embedded_artifact["processors"][0]["args"] = serde_json::json!(["prefix[g:a:1]"]);
        cases.push(embedded_artifact);
        let mut unquoted_data = install.clone();
        unquoted_data["data"]["PATCHED_SHA"]["client"] = "raw-literal".into();
        cases.push(unquoted_data);
        let mut unsafe_target = install.clone();
        unsafe_target["processors"][0]["outputs"] = serde_json::json!({
            "[example:output:..]":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });
        cases.push(unsafe_target);
        let mut undeclared_input = install.clone();
        undeclared_input["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .retain(|library| library["name"] != "example:processor:1.0");
        cases.push(undeclared_input);

        for case in cases {
            assert!(bind_modern_fixture_with_entries(&record, &version, &case, &[]).is_err());
        }
    }

    #[test]
    fn processor_binding_requires_selected_current_environment_input_contracts() {
        let (record, version, install) = forge_processor_fixture();

        let mut rule_excluded = install.clone();
        rule_excluded["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .last_mut()
            .expect("processor library")["rules"] = serde_json::json!([{"action":"disallow"}]);

        let mut divergent_path = install.clone();
        *divergent_path["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .last_mut()
            .expect("processor library") = serde_json::json!({
            "name":"example:processor:1.0",
            "downloads":{"artifact":{
                "path":"example/other/1.0/other-1.0.jar",
                "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "url":"https://example.test/other.jar"
            }}
        });

        let mut sha256_only = install.clone();
        *sha256_only["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .last_mut()
            .expect("processor library") = serde_json::json!({
            "name":"example:processor:1.0",
            "sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        });

        for invalid in [rule_excluded, divergent_path, sha256_only] {
            assert!(matches!(
                bind_modern_fixture_with_entries(&record, &version, &invalid, &[]),
                Err(ForgeInstallerError::InvalidForgeProcessorArtifactContract)
            ));
        }
    }

    #[test]
    fn processor_binding_accepts_downloader_verified_legacy_maven_sha1() {
        let (record, version, mut install) = forge_processor_fixture();
        *install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .last_mut()
            .expect("processor library") = serde_json::json!({
            "name":"example:processor:1.0",
            "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        });

        bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("legacy Maven SHA-1 is part of the downloader contract");
    }

    #[test]
    fn processor_binding_accepts_retained_authenticated_embedded_input() {
        let (record, version, mut install) = forge_processor_fixture();
        *install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .last_mut()
            .expect("processor library") = serde_json::json!({"name":"example:processor:1.0"});

        let bound = bind_modern_fixture_with_entries(
            &record,
            &version,
            &install,
            &[(
                "maven/example/processor/1.0/processor-1.0.jar",
                b"processor",
            )],
        )
        .expect("embedded processor input contract");
        let plan = typed_plan(&bound);
        let contract = plan
            .input_artifacts
            .values()
            .next()
            .expect("bound input contract");
        assert!(matches!(
            contract.source,
            super::BoundProcessorInputSource::Embedded
        ));
        assert_eq!(contract.size, Some(9));
    }

    #[test]
    fn processor_binding_rejects_non_inventory_and_conflicting_terminal_outputs() {
        let (record, version, install) = forge_processor_fixture();

        let mut non_inventory = install.clone();
        non_inventory["data"]["PATCHED"]["client"] = "[example:unmanaged:1.0]".into();
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &non_inventory, &[]),
            Err(ForgeInstallerError::InvalidForgeProcessorFinalOutput)
        ));

        let mut conflicting_version = version;
        conflicting_version["libraries"][1]["sha1"] =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        let mut conflicting_digest = install;
        conflicting_digest["data"]["PATCHED_SHA"]["client"] =
            "'cccccccccccccccccccccccccccccccccccccccc'".into();
        assert!(matches!(
            bind_modern_fixture_with_entries(
                &record,
                &conflicting_version,
                &conflicting_digest,
                &[],
            ),
            Err(ForgeInstallerError::InvalidForgeProcessorFinalOutput)
        ));
    }

    #[test]
    fn processor_output_digest_authenticates_checksumless_selected_final_library() {
        let (record, version, install) = forge_processor_fixture();

        let bound = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("processor output supplies final SHA-1 authority");
        let plan = typed_plan(&bound);
        assert!(matches!(
            plan.steps[0].outputs[0].role,
            super::BoundProcessorOutputRole::Terminal {
                expected_size: None,
            }
        ));
    }

    #[test]
    fn processor_binding_classifies_consumed_outputs_as_intermediate_only() {
        let (record, version, mut install) = forge_processor_fixture();
        install["data"]["INTERMEDIATE"] =
            serde_json::json!({"client":"[example:intermediate:1.0]"});
        install["data"]["INTERMEDIATE_SHA"] = serde_json::json!({
            "client":"'cccccccccccccccccccccccccccccccccccccccc'"
        });
        install["processors"] = serde_json::json!([
            {
                "jar":"example:processor:1.0",
                "args":["{INTERMEDIATE}"],
                "outputs":{"{INTERMEDIATE}":"{INTERMEDIATE_SHA}"}
            },
            {
                "jar":"example:processor:1.0",
                "args":["{INTERMEDIATE}","{PATCHED}"],
                "outputs":{"{PATCHED}":"{PATCHED_SHA}"}
            }
        ]);

        let bound = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("bound processor chain");
        let plan = typed_plan(&bound);
        assert!(matches!(
            plan.steps[0].outputs[0].role,
            super::BoundProcessorOutputRole::Intermediate
        ));
        assert!(matches!(
            plan.steps[1].outputs[0].role,
            super::BoundProcessorOutputRole::Terminal {
                expected_size: None,
            }
        ));
        let execution = bound.into_install_execution().expect("install execution");
        let (_, jobs) = execution
            .into_network_install()
            .expect("classified network install")
            .into_parts();
        assert!(!jobs.iter().any(|classified| {
            classified.job().relative_path
                == crate::portable_path::PortableRelativePath::new(
                    "example/intermediate/1.0/intermediate-1.0.jar",
                )
                .expect("intermediate path")
        }));
        assert!(!jobs.iter().any(|classified| {
            classified.job().relative_path
                == crate::portable_path::PortableRelativePath::new(
                    "net/minecraftforge/forge/1.21.5-55.0.0/forge-1.21.5-55.0.0-client.jar",
                )
                .expect("terminal path")
        }));
    }

    #[test]
    fn processor_output_can_be_both_final_inventory_and_a_later_input() {
        let (record, version, mut install) = forge_processor_fixture();
        install["data"]["FIRST"] = serde_json::json!({"client":"[example:first:1.0]"});
        install["data"]["FIRST_SHA"] = serde_json::json!({
            "client":"'cccccccccccccccccccccccccccccccccccccccc'"
        });
        install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .push(serde_json::json!({
                "name":"example:first:1.0",
                "sha1":"cccccccccccccccccccccccccccccccccccccccc",
                "size":2
            }));
        install["processors"] = serde_json::json!([
            {
                "jar":"example:processor:1.0",
                "args":["{FIRST}"],
                "outputs":{"{FIRST}":"{FIRST_SHA}"}
            },
            {
                "jar":"example:processor:1.0",
                "args":["{FIRST}","{PATCHED}"],
                "outputs":{"{PATCHED}":"{PATCHED_SHA}"}
            }
        ]);

        let bound = bind_modern_fixture_with_entries(&record, &version, &install, &[])
            .expect("output with final and dependency roles");
        let plan = typed_plan(&bound);
        assert!(matches!(
            plan.steps[0].outputs[0].role,
            super::BoundProcessorOutputRole::Terminal {
                expected_size: Some(2),
            }
        ));
        let execution = bound.into_install_execution().expect("install execution");
        let (_, jobs) = execution
            .into_network_install()
            .expect("classified network install")
            .into_parts();
        assert!(!jobs.iter().any(|classified| {
            classified.job().relative_path
                == crate::portable_path::PortableRelativePath::new(
                    "example/first/1.0/first-1.0.jar",
                )
                .expect("terminal final path")
        }));
    }

    #[test]
    fn processor_contract_errors_do_not_echo_authored_values() {
        for error in [
            ForgeInstallerError::InvalidForgeProcessorArtifactContract,
            ForgeInstallerError::InvalidForgeProcessorFinalOutput,
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains("PRIVATE"));
            assert!(!rendered.contains('/'));
            assert!(!rendered.contains('['));
        }
    }

    #[test]
    fn processor_binding_rejects_aliases_multiple_producers_and_dependency_cycles() {
        let (record, version, install) = forge_processor_fixture();

        let mut multiple = install.clone();
        let duplicate = multiple["processors"][0].clone();
        multiple["processors"]
            .as_array_mut()
            .expect("processors")
            .push(duplicate);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &multiple, &[]),
            Err(ForgeInstallerError::MultipleForgeProcessorProducers)
        ));

        let mut alias = install.clone();
        alias["processors"][0]["outputs"] = serde_json::json!({
            "[Example:Output:1.0]": "'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
            "[example:output:1.0]": "'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'"
        });
        alias["processors"][0]["args"] = serde_json::json!([]);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &alias, &[]),
            Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        ));

        let cycle = cyclic_processor_fixture(&install, true);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &cycle, &[]),
            Err(ForgeInstallerError::ForgeProcessorDependencyCycle)
        ));
        let forward = cyclic_processor_fixture(&install, false);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &forward, &[]),
            Err(ForgeInstallerError::InvalidForgeProcessor)
        ));

        let mut self_dependency = install.clone();
        self_dependency["processors"][0]["args"] = serde_json::json!([]);
        self_dependency["processors"][0]["outputs"] = serde_json::json!({
            "[example:processor:1.0]":"'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'"
        });
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &self_dependency, &[]),
            Err(ForgeInstallerError::ForgeProcessorDependencyCycle)
        ));

        let mut prior = cyclic_processor_fixture(&install, true);
        prior["processors"][0]["args"] = serde_json::json!(["{A}"]);
        prior["processors"][1]["args"] = serde_json::json!(["{A}", "{B}"]);
        bind_modern_fixture_with_entries(&record, &version, &prior, &[])
            .expect("prior authenticated processor output");

        let mut direct_digest = install.clone();
        direct_digest["processors"][0]["outputs"]["{PATCHED}"] =
            "'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'".into();
        bind_modern_fixture_with_entries(&record, &version, &direct_digest, &[])
            .expect("quoted direct output digest");
    }

    #[test]
    fn processor_binding_rejects_case_drifted_dependency_authority() {
        let (record, version, install) = forge_processor_fixture();

        let mut jar_alias = install.clone();
        jar_alias["processors"][0]["jar"] = "Example:processor:1.0".into();
        let Err(jar_alias_error) =
            bind_modern_fixture_with_entries(&record, &version, &jar_alias, &[])
        else {
            panic!("case-drifted processor jar was accepted");
        };
        assert!(
            matches!(
                jar_alias_error,
                ForgeInstallerError::PortablePathAlias
                    | ForgeInstallerError::ForgeProcessorPortableAlias
            ),
            "unexpected static error variant: {jar_alias_error:?}"
        );

        let mut classpath_alias = install.clone();
        classpath_alias["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .push(serde_json::json!({
                "name":"example:support:1.0",
                "sha1":"cccccccccccccccccccccccccccccccccccccccc"
            }));
        classpath_alias["processors"][0]["classpath"] = serde_json::json!(["Example:support:1.0"]);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &classpath_alias, &[]),
            Err(ForgeInstallerError::PortablePathAlias)
                | Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        ));

        let mut current_output_alias = install.clone();
        current_output_alias["data"]["PATCHED_ALIAS"] = serde_json::json!({
            "client":"[net.minecraftforge:Forge:1.21.5-55.0.0:client]"
        });
        current_output_alias["processors"][0]["args"] = serde_json::json!(["{PATCHED_ALIAS}"]);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &current_output_alias, &[]),
            Err(ForgeInstallerError::PortablePathAlias)
                | Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        ));

        let mut prior_output_alias = cyclic_processor_fixture(&install, true);
        prior_output_alias["data"]["A_ALIAS"] =
            serde_json::json!({"client":"[Example:generated-a:1.0]"});
        prior_output_alias["processors"][0]["args"] = serde_json::json!(["{A}"]);
        prior_output_alias["processors"][1]["args"] = serde_json::json!(["{A_ALIAS}", "{B}"]);
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &prior_output_alias, &[]),
            Err(ForgeInstallerError::PortablePathAlias)
                | Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        ));
    }

    #[test]
    fn processor_binding_enforces_installer_data_source_and_size_bounds() {
        let (record, version, mut install) = forge_processor_fixture();
        install["data"]["BINPATCH"] = serde_json::json!({"client":"/data/client.lzma"});
        install["processors"][0]["args"] = serde_json::json!(["{BINPATCH}", "{PATCHED}"]);

        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &install, &[]),
            Err(ForgeInstallerError::MissingForgeProcessorData)
        ));
        assert!(matches!(
            bind_modern_fixture_with_entries(
                &record,
                &version,
                &install,
                &[("Data/client.lzma", b"alias")],
            ),
            Err(ForgeInstallerError::ForgeProcessorPortableAlias)
        ));
        let oversized = vec![b'x'; (super::MAX_FORGE_PROCESSOR_DATA_ENTRY_BYTES + 1) as usize];
        assert!(matches!(
            bind_modern_fixture_with_entries(
                &record,
                &version,
                &install,
                &[("data/client.lzma", oversized.as_slice())],
            ),
            Err(ForgeInstallerError::ForgeProcessorDataEntryTooLarge)
        ));
    }

    #[test]
    fn processor_binding_enforces_declaration_count_and_aggregate_data_bounds() {
        let (record, version, install) = forge_processor_fixture();
        let mut too_many = install.clone();
        too_many["processors"] = serde_json::Value::Array(
            (0..=super::MAX_FORGE_PROCESSORS)
                .map(|_| serde_json::json!({"sides":["server"]}))
                .collect(),
        );
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &too_many, &[]),
            Err(ForgeInstallerError::TooManyForgeProcessors)
        ));
        let too_many_data =
            serde_json::from_value::<super::InstallProfileDeclarations>(serde_json::json!({
                "data": (0..=super::MAX_FORGE_PROCESSOR_DATA)
                    .map(|index| (format!("D{index}"), serde_json::json!({"client":"'x'"})))
                    .collect::<serde_json::Map<_, _>>()
            }))
            .expect("processor data declarations");
        assert!(matches!(
            super::bind_forge_processor_plan(&[], &too_many_data, &[], &[]),
            Err(ForgeInstallerError::TooManyForgeProcessorData)
        ));
        let too_many_outputs =
            serde_json::from_value::<super::InstallProfileDeclarations>(serde_json::json!({
                "processors":[{
                    "sides":["server"],
                    "outputs": (0..=super::MAX_FORGE_PROCESSOR_OUTPUTS)
                        .map(|index| (
                            format!("[g:a{index}:1]"),
                            serde_json::Value::String(
                                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
                            )
                        ))
                        .collect::<serde_json::Map<_, _>>()
                }]
            }))
            .expect("processor output declarations");
        assert!(matches!(
            super::bind_forge_processor_plan(&[], &too_many_outputs, &[], &[]),
            Err(ForgeInstallerError::TooManyForgeProcessorOutputs)
        ));
        let too_many_args =
            serde_json::from_value::<super::InstallProfileDeclarations>(serde_json::json!({
                "processors":[{
                    "args": vec!["x"; super::MAX_FORGE_PROCESSOR_ARGS + 1],
                    "sides":["server"]
                }]
            }))
            .expect("processor argument declarations");
        assert!(matches!(
            super::bind_forge_processor_plan(&[], &too_many_args, &[], &[]),
            Err(ForgeInstallerError::ForgeProcessorDeclarationsTooLarge)
        ));

        let mut aggregate = install.clone();
        for index in 0..5 {
            aggregate["data"][format!("FILE_{index}")] =
                serde_json::json!({"client":format!("/data/{index}.bin")});
        }
        aggregate["processors"][0]["args"] = serde_json::Value::Array(
            (0..5)
                .map(|index| serde_json::Value::String(format!("{{FILE_{index}}}")))
                .chain([serde_json::Value::String("{PATCHED}".to_string())])
                .collect(),
        );
        let bytes = vec![b'x'; 900];
        let entries = (0..5)
            .map(|index| (format!("data/{index}.bin"), bytes.as_slice()))
            .collect::<Vec<_>>();
        let borrowed = entries
            .iter()
            .map(|(name, bytes)| (name.as_str(), *bytes))
            .collect::<Vec<_>>();
        assert!(matches!(
            bind_modern_fixture_with_entries(&record, &version, &aggregate, &borrowed),
            Err(ForgeInstallerError::ForgeProcessorDataTooLarge)
        ));
    }

    #[test]
    fn modern_binding_rejects_effective_identity_root_and_base_override_drift() {
        let record = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern,
            "1.21.5",
            "55.0.0",
        );
        let (version, install) = modern_binding_profiles(&record);
        let mut variants = Vec::new();

        let mut drift = version.clone();
        drift["id"] = "1.21.5-forge-55.0.1".into();
        variants.push((drift, install.clone()));
        let mut drift = version.clone();
        drift["inheritsFrom"] = "1.21.4".into();
        variants.push((drift, install.clone()));
        let mut drift = version.clone();
        drift["type"] = "snapshot".into();
        variants.push((drift, install.clone()));
        let mut drift = version.clone();
        drift["libraries"] = serde_json::json!([]);
        let mut install_without_roots = install.clone();
        install_without_roots["libraries"] = serde_json::json!([]);
        variants.push((drift, install_without_roots));
        let mut drift = version.clone();
        drift["libraries"][0]["name"] = "net.minecraftforge:forge:1.21.5-55.0.1:universal".into();
        variants.push((drift, install.clone()));
        let mut drift = version.clone();
        drift["libraries"] = serde_json::json!([
            {"name":"net.minecraftforge:forge:1.21.5-55.0.0:universal"},
            {"name":"net.neoforged:neoforge:21.5.74:universal"}
        ]);
        variants.push((drift, install.clone()));
        let mut drift = version.clone();
        drift["libraries"] = serde_json::json!([
            {"name":"net.minecraftforge:forge:1.21.5-55.0.0:universal"},
            {"name":"net.minecraftforge:forge:1.21.5-55.0.0:universal@zip"}
        ]);
        variants.push((drift, install.clone()));

        for (field, value) in [
            ("assetIndex", serde_json::json!({})),
            ("assets", serde_json::json!("legacy")),
            ("downloads", serde_json::json!({})),
            ("javaVersion", serde_json::json!({})),
            (
                "logging",
                serde_json::json!({
                    "client": {
                        "argument": "-Dlog.config=hostile",
                        "file": {"id":"hostile.xml"}
                    }
                }),
            ),
        ] {
            let mut drift = version.clone();
            drift[field] = value;
            variants.push((drift, install.clone()));
        }

        for (version, install) in variants {
            assert!(bind_modern_fixture(&record, &version, &install).is_err());
        }
    }

    #[test]
    fn modern_binding_rejects_every_authored_install_identity_field_drift() {
        for record in [
            binding_record(
                LoaderComponentId::Forge,
                LoaderInstallStrategy::ForgeModern,
                "1.21.5",
                "55.0.0",
            ),
            binding_record(
                LoaderComponentId::NeoForge,
                LoaderInstallStrategy::NeoForgeModern,
                "1.21.5",
                "21.5.74",
            ),
        ] {
            let (version, install) = modern_binding_profiles(&record);
            for (field, value) in [
                ("spec", serde_json::json!(99)),
                ("profile", serde_json::json!("wrong")),
                ("version", serde_json::json!("wrong")),
                ("minecraft", serde_json::json!("1.21.4")),
                ("path", serde_json::json!("wrong:root:1.0:shim")),
            ] {
                let mut drift = install.clone();
                drift[field] = value;
                assert!(bind_modern_fixture(&record, &version, &drift).is_err());
            }
            for field in ["spec", "profile", "version", "minecraft"] {
                let mut drift = install.clone();
                drift.as_object_mut().expect("install object").remove(field);
                assert!(bind_modern_fixture(&record, &version, &drift).is_err());
            }
            let mut path_presence_drift = install.clone();
            if record.component_id == LoaderComponentId::Forge {
                path_presence_drift
                    .as_object_mut()
                    .expect("install object")
                    .remove("path");
            } else {
                path_presence_drift["path"] =
                    format!("net.neoforged:neoforge:{}:shim", record.loader_version).into();
            }
            assert!(bind_modern_fixture(&record, &version, &path_presence_drift).is_err());
        }
    }

    #[test]
    fn legacy_binding_rejects_path_minecraft_target_and_parent_drift() {
        let record = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeLegacyInstaller,
            "1.7.10",
            "10.13.4.1614-1.7.10",
        );
        let profile = legacy_binding_profile(&record);
        let mut variants = Vec::new();

        let mut drift = profile.clone();
        drift["install"]["path"] = "net.minecraftforge:forge:1.7.10-10.13.4.1614-wrong".into();
        variants.push(drift);
        let mut drift = profile.clone();
        drift["install"]["filePath"] = "forge-wrong-universal.jar".into();
        variants.push(drift);
        let mut drift = profile.clone();
        drift["install"]["minecraft"] = "1.7.9".into();
        variants.push(drift);
        let mut drift = profile.clone();
        drift["minecraft"] = "1.7.9".into();
        variants.push(drift);
        let mut drift = profile.clone();
        drift["install"]["target"] = "1.7.10-ForgeWrong".into();
        variants.push(drift);
        let mut drift = profile.clone();
        drift["versionInfo"]["inheritsFrom"] = "1.7.9".into();
        variants.push(drift);
        let mut drift = profile.clone();
        drift["versionInfo"]["assets"] = "unrelated-assets".into();
        variants.push(drift);

        for profile in variants {
            assert!(bind_legacy_fixture(&record, &profile).is_err());
        }
    }

    #[test]
    fn pure_plan_retains_legacy_root_forge_library() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.6.4-Forge9.11.1.1345",
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:9.11.1.1345" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:9.11.1.1345",
                "filePath": "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                "target": "1.6.4-Forge9.11.1.1345",
                "minecraft": "1.6.4"
            }
        }"#;
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                b"forge universal",
            ),
        ]);

        let plan = plan(&jar);
        let artifact = embedded_artifact(
            &plan,
            "net/minecraftforge/forge/1.6.4-9.11.1.1345/forge-1.6.4-9.11.1.1345-universal.jar",
        );
        assert_eq!(artifact, b"forge universal");
    }

    #[test]
    fn pure_plan_strips_legacy_root_forge_library_meta_in_memory() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.5.2-Forge7.8.1.738",
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:7.8.1.738" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:7.8.1.738",
                "filePath": "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                "target": "1.5.2-Forge7.8.1.738",
                "minecraft": "1.5.2",
                "stripMeta": true
            }
        }"#;
        let forge_jar = zip_with_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest".as_slice()),
            ("META-INF/FORGE.SF", b"signature".as_slice()),
            ("META-INF/FORGE.DSA", b"signature".as_slice()),
            ("net/minecraft/client/Minecraft.class", b"class".as_slice()),
        ]);
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                forge_jar.as_slice(),
            ),
        ]);
        let plan = plan(&jar);
        let installed_jar = embedded_artifact(
            &plan,
            "net/minecraftforge/forge/1.5.2-7.8.1.738/forge-1.5.2-7.8.1.738-universal.jar",
        );
        assert!(zip_contains(
            installed_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(installed_jar, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(installed_jar, "META-INF/FORGE.SF"));
        assert!(!zip_contains(installed_jar, "META-INF/FORGE.DSA"));
    }

    #[test]
    fn pure_plan_retains_modern_embedded_maven_entry() {
        let version_json = br#"{
            "id": "1.21.1-forge-52.1.0",
            "libraries": []
        }"#;
        let install_profile = br#"{
            "spec": 1,
            "profile": "forge",
            "version": "1.21.1-52.1.0",
            "libraries": [{"name":"net.minecraftforge:forge:1.21.1-52.1.0:shim"}],
            "processors": []
        }"#;
        let jar = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("install_profile.json", install_profile.as_slice()),
            (
                "maven/net/minecraftforge/forge/1.21.1-52.1.0/forge-1.21.1-52.1.0-shim.jar",
                b"shim",
            ),
        ]);

        let plan = plan(&jar);
        assert_eq!(
            embedded_artifact(
                &plan,
                "net/minecraftforge/forge/1.21.1-52.1.0/forge-1.21.1-52.1.0-shim.jar",
            ),
            b"shim"
        );
    }

    #[test]
    fn pure_plan_retains_exact_profiles_and_source_bytes() {
        let version_json = br#"{
            "id": "1.21.1-forge-52.1.0",
            "libraries": [{"name":"example:version-lib:1.0"}]
        }"#;
        let install_profile = br#"{
            "libraries": [
                {"name":"example:installer-lib:1.0"},
                {"name":"example:embedded:1.0"}
            ],
            "processors": [{"args":["{ROOT}/output.jar","{INPUT}"]}],
            "data": {
                "INPUT": {"client":"/data/input.bin"},
                "LIB": {"client":"[example:installer-lib:1.0]"}
            }
        }"#;
        let jar = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("install_profile.json", install_profile.as_slice()),
            ("maven/example/embedded/1.0/embedded-1.0.jar", b"jar"),
        ]);

        let plan = plan(&jar);
        assert_eq!(plan.source_bytes(), jar);
        assert_eq!(plan.libraries().len(), 3);
    }

    #[test]
    fn pure_plan_rejects_duplicate_and_unsafe_maven_paths() {
        let version_json = br#"{"id":"forge","libraries":[]}"#;
        let duplicate = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/mod.jar", b"first"),
            (r"maven/example\mod.jar", b"second"),
        ]);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(duplicate)),
            Err(ForgeInstallerError::DuplicateEntry { .. })
        ));

        let unsafe_path = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/../outside.jar", b"outside"),
        ]);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(unsafe_path)),
            Err(ForgeInstallerError::InvalidEntryPath)
        ));
    }

    #[test]
    fn pure_plan_rejects_undeclared_and_portable_alias_maven_paths() {
        let version_json = br#"{
            "id":"forge",
            "libraries":[{"name":"example:mod:1.0"}]
        }"#;
        let undeclared = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/other/1.0/other-1.0.jar", b"undeclared"),
        ]);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(undeclared)),
            Err(ForgeInstallerError::UndeclaredEmbeddedArtifact { .. })
        ));

        let alias = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/mod/1.0/mod-1.0.jar", b"first"),
            ("maven/Example/mod/1.0/mod-1.0.jar", b"second"),
        ]);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(alias)),
            Err(ForgeInstallerError::PortablePathAlias)
        ));
    }

    #[test]
    fn pure_plan_rejects_conflicting_legacy_and_maven_artifacts() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.6.4-Forge9.11.1.1345",
                "libraries": [{"name":"net.minecraftforge:minecraftforge:9.11.1.1345"}]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:9.11.1.1345",
                "filePath": "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                "target": "1.6.4-Forge9.11.1.1345",
                "minecraft": "1.6.4"
            }
        }"#;
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.6.4-9.11.1.1345.jar",
                b"legacy root",
            ),
            (
                "maven/net/minecraftforge/forge/1.6.4-9.11.1.1345/forge-1.6.4-9.11.1.1345-universal.jar",
                b"maven copy",
            ),
        ]);

        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar)),
            Err(ForgeInstallerError::ConflictingEmbeddedArtifact)
        ));
    }

    #[test]
    fn pure_plan_has_no_filesystem_effects_and_enforces_aggregate_bounds() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let nonexistent = std::env::temp_dir().join(format!("axial-pure-installer-{nanos:x}"));
        assert!(!nonexistent.exists());

        let version_json = br#"{
            "id":"forge",
            "libraries":[{"name":"example:mod:1.0"}]
        }"#;
        let valid = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/mod/1.0/mod-1.0.jar", b"mod"),
        ]);
        plan(&valid);
        assert!(!nonexistent.exists());

        let aggregate = zip_with_generated_maven_entries(5, 900);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(aggregate)),
            Err(ForgeInstallerError::EmbeddedEntriesTooLarge)
        ));
        let too_many = zip_with_generated_maven_entries(super::MAX_INSTALLER_ENTRY_COUNT, 0);
        assert!(matches!(
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(too_many)),
            Err(ForgeInstallerError::TooManyEntries)
        ));
    }

    #[test]
    fn pure_plan_reports_legacy_strip_meta() {
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.5.2-Forge7.8.1.738",
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "minecraftArguments": "${auth_player_name} ${auth_session}",
                "assetIndex": { "id": "legacy" },
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:7.8.1.738" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:7.8.1.738",
                "filePath": "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                "target": "1.5.2-Forge7.8.1.738",
                "minecraft": "1.5.2",
                "stripMeta": true
            }
        }"#;
        let forge_jar = zip_with_entries(&[("example/Class.class", b"class".as_slice())]);
        let jar = zip_with_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                forge_jar.as_slice(),
            ),
        ]);

        let extracted = plan(&jar);

        assert!(extracted.strip_client_meta());
    }

    #[test]
    fn verified_installer_source_derives_deterministic_stripped_client_bytes() {
        let base = zip_with_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/CLIENT.SF", b"signature"),
            ("META-INF/CLIENT.RSA", b"signature"),
            ("client/payload.bin", b"payload"),
        ]);
        let source = super::VerifiedInstallerReceiptSource::from_test(Vec::new(), true);
        let first = source
            .derive_child_client_bytes(&base)
            .expect("stripped client");
        let second = source
            .derive_child_client_bytes(&base)
            .expect("deterministic stripped client");
        assert_eq!(first.bytes(), second.bytes());
        assert!(!zip_contains(first.bytes(), "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(first.bytes(), "META-INF/CLIENT.SF"));
        assert!(!zip_contains(first.bytes(), "META-INF/CLIENT.RSA"));
        let mut archive = zip::ZipArchive::new(Cursor::new(first.bytes())).expect("client zip");
        let mut payload = Vec::new();
        archive
            .by_name("client/payload.bin")
            .expect("retained payload")
            .read_to_end(&mut payload)
            .expect("read retained payload");
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn merge_libraries_by_name_keeps_distinct_versions() {
        let merged = merge_libraries_by_name(
            &[Library {
                name: "net.sf.jopt-simple:jopt-simple:5.0.4".to_string(),
                ..Library::default()
            }],
            &[Library {
                name: "net.sf.jopt-simple:jopt-simple:6.0-alpha-3".to_string(),
                ..Library::default()
            }],
        )
        .expect("distinct library declarations");

        assert_eq!(
            merged
                .into_iter()
                .map(|library| library.name)
                .collect::<Vec<_>>(),
            vec![
                "net.sf.jopt-simple:jopt-simple:5.0.4".to_string(),
                "net.sf.jopt-simple:jopt-simple:6.0-alpha-3".to_string()
            ]
        );
    }

    #[test]
    fn merge_libraries_rejects_same_coordinate_declaration_drift() {
        let primary = Library {
            name: "example:library:1.0".to_string(),
            sha1: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            ..Library::default()
        };
        let mut conflicting = primary.clone();
        conflicting.sha1 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();

        assert!(matches!(
            merge_libraries_by_name(&[primary], &[conflicting]),
            Err(ForgeInstallerError::ConflictingLibraryDeclaration { .. })
        ));
    }

    #[test]
    fn pure_plan_rejects_oversized_profile_entry() {
        let jar = zip_with_entry(
            "install_profile.json",
            vec![b' '; (MAX_INSTALLER_PROFILE_ENTRY_BYTES + 1) as usize],
        );

        let error = plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar))
            .expect_err("oversized install profile should fail");

        assert!(
            matches!(error, ForgeInstallerError::EntryTooLarge { name } if name == "install_profile.json")
        );
    }

    #[test]
    fn pure_plan_rejects_oversized_maven_entry_without_effects() {
        let version_json = br#"{
            "id":"forge",
            "libraries":[{"name":"example:mod:1.0"}]
        }"#;
        let oversized = vec![b'j'; (MAX_INSTALLER_EMBEDDED_ENTRY_BYTES + 1) as usize];
        let jar = zip_with_entries(&[
            ("version.json", version_json.as_slice()),
            ("maven/example/mod/1.0/mod-1.0.jar", oversized.as_slice()),
        ]);

        let error = plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar))
            .expect_err("oversized maven entry should fail");

        assert!(
            matches!(error, ForgeInstallerError::EntryTooLarge { name } if name == "example/mod/1.0/mod-1.0.jar")
        );
    }

    fn binding_record(
        component_id: LoaderComponentId,
        strategy: LoaderInstallStrategy,
        minecraft_version: &str,
        loader_version: &str,
    ) -> LoaderBuildRecord {
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            version_id: installed_version_id_for(component_id, minecraft_version, loader_version)
                .expect("canonical installed version id"),
            build_meta: LoaderBuildMetadata::default(),
            strategy,
            artifact_kind: LoaderArtifactKind::InstallerJar,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::InstallerJar {
                url: "https://example.test/installer.jar".to_string(),
            },
        }
    }

    fn modern_binding_profiles(
        record: &LoaderBuildRecord,
    ) -> (serde_json::Value, serde_json::Value) {
        let (id, version_libraries, profile, path, install_libraries) = match record.component_id {
            LoaderComponentId::Forge => (
                format!(
                    "{}-forge-{}",
                    record.minecraft_version, record.loader_version
                ),
                serde_json::json!([
                    {"name": format!(
                        "net.minecraftforge:forge:{}-{}:universal",
                        record.minecraft_version, record.loader_version
                    )},
                    {"name": format!(
                        "net.minecraftforge:forge:{}-{}:client",
                        record.minecraft_version, record.loader_version
                    )}
                ]),
                "forge",
                Some(format!(
                    "net.minecraftforge:forge:{}-{}:shim",
                    record.minecraft_version, record.loader_version
                )),
                serde_json::json!([
                    {"name": format!(
                        "net.minecraftforge:forge:{}-{}:universal",
                        record.minecraft_version, record.loader_version
                    )},
                    {"name": format!(
                        "net.minecraftforge:forge:{}-{}:shim",
                        record.minecraft_version, record.loader_version
                    )}
                ]),
            ),
            LoaderComponentId::NeoForge => (
                format!("neoforge-{}", record.loader_version),
                serde_json::json!([
                    {"name": format!(
                        "net.neoforged:neoforge:{}:universal",
                        record.loader_version
                    )}
                ]),
                "NeoForge",
                None,
                serde_json::json!([
                    {"name": format!(
                        "net.neoforged:neoforge:{}:universal",
                        record.loader_version
                    )}
                ]),
            ),
            LoaderComponentId::Fabric | LoaderComponentId::Quilt => {
                unreachable!("installer binding fixture component")
            }
        };
        let version = serde_json::json!({
            "id": id.clone(),
            "inheritsFrom": record.minecraft_version,
            "type": "release",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "logging": {},
            "libraries": version_libraries
        });
        let mut install = serde_json::json!({
            "spec": 1,
            "profile": profile,
            "version": id,
            "minecraft": record.minecraft_version,
            "libraries": install_libraries,
            "processors": []
        });
        if let Some(path) = path {
            install["path"] = path.into();
        }
        (version, install)
    }

    fn forge_processor_fixture() -> (LoaderBuildRecord, serde_json::Value, serde_json::Value) {
        let record = binding_record(
            LoaderComponentId::Forge,
            LoaderInstallStrategy::ForgeModern,
            "1.21.5",
            "55.0.0",
        );
        let (version, mut install) = modern_binding_profiles(&record);
        install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .push(serde_json::json!({
                "name": "example:processor:1.0",
                "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }));
        install["data"] = serde_json::json!({
            "PATCHED": {"client":"[net.minecraftforge:forge:1.21.5-55.0.0:client]"},
            "PATCHED_SHA": {"client":"'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'"}
        });
        install["processors"] = serde_json::json!([{
            "jar": "example:processor:1.0",
            "args": ["{PATCHED}"],
            "sides": ["client"],
            "outputs": {"{PATCHED}":"{PATCHED_SHA}"}
        }]);
        (record, version, install)
    }

    fn cyclic_processor_fixture(base: &serde_json::Value, close_cycle: bool) -> serde_json::Value {
        let mut install = base.clone();
        install["libraries"]
            .as_array_mut()
            .expect("installer libraries")
            .push(serde_json::json!({
                "name":"example:generated-b:1.0",
                "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }));
        install["data"] = serde_json::json!({
            "A": {"client":"[example:generated-a:1.0]"},
            "B": {"client":"[example:generated-b:1.0]"},
            "SHA_A": {"client":"'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'"},
            "SHA_B": {"client":"'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'"}
        });
        install["processors"] = serde_json::json!([
            {
                "jar":"example:processor:1.0",
                "args":["{B}","{A}"],
                "outputs":{"{A}":"{SHA_A}"}
            },
            {
                "jar":"example:processor:1.0",
                "args": if close_cycle {serde_json::json!(["{A}","{B}"])} else {serde_json::json!(["{B}"])},
                "outputs":{"{B}":"{SHA_B}"}
            }
        ]);
        install
    }

    fn legacy_binding_profile(record: &LoaderBuildRecord) -> serde_json::Value {
        let version = format!("{}-{}", record.minecraft_version, record.loader_version);
        let assets = if record.minecraft_version == "1.8.9" {
            "1.8"
        } else {
            record.minecraft_version.as_str()
        };
        serde_json::json!({
            "versionInfo": {
                "id": format!("{}-Forge{}", record.minecraft_version, record.loader_version),
                "inheritsFrom": record.minecraft_version,
                "type": "release",
                "assets": assets,
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "libraries": [
                    {"name": format!("net.minecraftforge:forge:{version}")}
                ]
            },
            "install": {
                "path": format!("net.minecraftforge:forge:{version}"),
                "filePath": format!("forge-{version}-universal.jar"),
                "target": format!("{}-Forge{}", record.minecraft_version, record.loader_version),
                "minecraft": record.minecraft_version
            }
        })
    }

    fn later_legacy_binding_profiles(
        record: &LoaderBuildRecord,
    ) -> (serde_json::Value, serde_json::Value) {
        let id = format!(
            "{}-forge-{}",
            record.minecraft_version, record.loader_version
        );
        let root = format!(
            "net.minecraftforge:forge:{}-{}",
            record.minecraft_version, record.loader_version
        );
        (
            serde_json::json!({
                "id": id.clone(),
                "inheritsFrom": record.minecraft_version,
                "type": "release",
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "logging": {},
                "libraries": [{"name": root.clone()}]
            }),
            serde_json::json!({
                "spec": 0,
                "profile": "forge",
                "version": id,
                "path": root.clone(),
                "minecraft": record.minecraft_version,
                "libraries": [{"name": root}],
                "processors": []
            }),
        )
    }

    fn bind_modern_fixture(
        record: &LoaderBuildRecord,
        version: &serde_json::Value,
        install: &serde_json::Value,
    ) -> Result<(), ForgeInstallerError> {
        bind_modern_fixture_with_entries(record, version, install, &[]).map(|_| ())
    }

    fn bind_modern_fixture_with_entries(
        record: &LoaderBuildRecord,
        version: &serde_json::Value,
        install: &serde_json::Value,
        extra_entries: &[(&str, &[u8])],
    ) -> Result<super::BoundForgeInstallerPlan, ForgeInstallerError> {
        let version = serde_json::to_vec(version).expect("serialize version profile");
        let install = serde_json::to_vec(install).expect("serialize install profile");
        let mut entries = vec![
            ("version.json", version.as_slice()),
            ("install_profile.json", install.as_slice()),
        ];
        entries.extend_from_slice(extra_entries);
        let jar = zip_with_entries(&entries);
        let authenticated =
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar))?;
        bind_authenticated_installer_plan(authenticated, record)
    }

    fn bind_legacy_fixture(
        record: &LoaderBuildRecord,
        install: &serde_json::Value,
    ) -> Result<(), ForgeInstallerError> {
        bind_legacy_fixture_plan(record, install).map(|_| ())
    }

    fn bind_legacy_fixture_plan(
        record: &LoaderBuildRecord,
        install: &serde_json::Value,
    ) -> Result<super::BoundForgeInstallerPlan, ForgeInstallerError> {
        let install_bytes = serde_json::to_vec(install).expect("serialize legacy install profile");
        let file_path = install["install"]["filePath"]
            .as_str()
            .unwrap_or("forge-invalid-universal.jar");
        let jar = zip_with_entries(&[
            ("install_profile.json", install_bytes.as_slice()),
            (file_path, b"legacy root"),
        ]);
        let authenticated =
            plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(jar))?;
        bind_authenticated_installer_plan(authenticated, record)
    }

    fn zip_with_entry(name: &str, bytes: Vec<u8>) -> Vec<u8> {
        zip_with_entries(&[(name, bytes.as_slice())])
    }

    fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            for (name, bytes) in entries {
                writer
                    .start_file(*name, SimpleFileOptions::default())
                    .expect("start zip file");
                writer.write_all(bytes).expect("write zip file");
            }
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn zip_with_generated_maven_entries(count: usize, bytes_per_entry: usize) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            writer
                .start_file("version.json", SimpleFileOptions::default())
                .expect("start version json");
            let libraries = (0..count)
                .map(|index| serde_json::json!({"name": format!("example:artifact-{index}:1.0")}))
                .collect::<Vec<_>>();
            writer
                .write_all(
                    serde_json::to_string(&serde_json::json!({
                        "id": "forge",
                        "libraries": libraries
                    }))
                    .expect("serialize version json")
                    .as_bytes(),
                )
                .expect("write version json");
            for index in 0..count {
                writer
                    .start_file(
                        format!("maven/example/artifact-{index}/1.0/artifact-{index}-1.0.jar"),
                        SimpleFileOptions::default(),
                    )
                    .expect("start Maven entry");
                writer
                    .write_all(&vec![b'x'; bytes_per_entry])
                    .expect("write Maven entry");
            }
            writer.finish().expect("finish generated installer");
        }
        cursor.into_inner()
    }

    fn zip_contains(bytes: &[u8], name: &str) -> bool {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("zip archive");
        archive.by_name(name).is_ok()
    }

    fn plan(bytes: &[u8]) -> AuthenticatedForgeInstallerPlan {
        plan_authenticated_installer(VerifiedLoaderSource::from_test_bytes(bytes.to_vec()))
            .expect("authenticated installer plan")
    }

    fn embedded_artifact<'a>(plan: &'a AuthenticatedForgeInstallerPlan, path: &str) -> &'a [u8] {
        plan.embedded_maven_artifacts()
            .iter()
            .find(|artifact| artifact.relative_path().as_str() == path)
            .expect("embedded Maven artifact")
            .bytes()
    }
}
