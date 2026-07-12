use super::forge_installer::{
    AuthenticatedEmbeddedMavenArtifact, BoundForgeInstallerContinuation,
    BoundForgeProcessorExecution, BoundProcessorArgument, BoundProcessorArgumentPart,
    BoundProcessorArtifact, BoundProcessorData, BoundProcessorOutputRole, BoundProcessorPlan,
    BoundProcessorStep, ProcessorBuiltinToken,
};
use super::managed_fs::ManagedTreeSnapshot;
use super::workspace::cleanup::{LoaderWorkspace, ProcessorWorkspace};
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::KnownGoodInstallReceipt;
use crate::runtime::{ProcessorRuntime, ensure_axial_managed_processor_runtime};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{Cursor, Read};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use zip::ZipArchive;

const MAX_MANIFEST_BYTES: u64 = 64 << 10;
const MAX_PROCESSOR_JAR_ENTRIES: usize = 4096;
const MAX_MAIN_CLASS_BYTES: usize = 256;
const MAX_PROCESS_OUTPUT_BYTES: usize = 1 << 20;
const MAX_PROCESS_OUTPUT_TOTAL_BYTES: usize = 2 << 20;
const PROCESSOR_TIMEOUT: Duration = Duration::from_secs(120);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub(crate) enum BoundProcessorError {
    #[error("processor authority is invalid")]
    Authority,
    #[error("processor staging failed")]
    Stage,
    #[error("managed processor runtime is unavailable")]
    Runtime,
    #[error("processor entry point is invalid")]
    Manifest,
    #[error("processor could not be started")]
    Spawn,
    #[error("processor exceeded its execution time limit")]
    Timeout,
    #[error("processor output exceeded its capture limit")]
    OutputLimit,
    #[error("processor exited unsuccessfully")]
    Unsuccessful,
    #[error("processor execution was cancelled")]
    Cancelled,
    #[error("processor workspace cleanup failed")]
    Cleanup,
    #[error("processor owner task stopped unexpectedly")]
    OwnerStopped,
}

pub(crate) struct VerifiedProcessorOutputs {
    entries: BTreeMap<ArtifactRelativePath, VerifiedProcessorOutput>,
}

pub(crate) struct VerifiedProcessorOutput {
    bytes: Arc<[u8]>,
    size: u64,
    sha1: [u8; 20],
}

struct VerifiedStepOutput {
    bytes: Option<Vec<u8>>,
    size: u64,
    sha1: [u8; 20],
    terminal: bool,
}

pub(crate) struct BoundProcessorExecutionResult {
    pub(crate) base_receipt: KnownGoodInstallReceipt,
    pub(crate) continuation: BoundForgeInstallerContinuation,
    pub(crate) outputs: VerifiedProcessorOutputs,
}

#[derive(Clone, Copy)]
pub(crate) struct BoundProcessorProgress {
    pub(crate) current: usize,
    pub(crate) total: usize,
}

pub(crate) struct BoundProcessorExecutionHandle {
    cancel: Option<oneshot::Sender<()>>,
    progress: mpsc::UnboundedReceiver<BoundProcessorProgress>,
    task: Option<JoinHandle<Result<BoundProcessorExecutionResult, BoundProcessorError>>>,
}

impl Drop for BoundProcessorExecutionHandle {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the receipt-producing Forge install cutover consumes this capability next"
    )
)]
impl BoundProcessorExecutionHandle {
    pub(crate) async fn finish(
        mut self,
        mut progress: impl FnMut(BoundProcessorProgress),
    ) -> Result<BoundProcessorExecutionResult, BoundProcessorError> {
        let mut task = self.task.take().ok_or(BoundProcessorError::OwnerStopped)?;
        let mut progress_open = true;
        loop {
            tokio::select! {
                result = &mut task => {
                    self.cancel.take();
                    return result.map_err(|_| BoundProcessorError::OwnerStopped)?;
                }
                update = self.progress.recv(), if progress_open => {
                    match update {
                        Some(update) => progress(update),
                        None => progress_open = false,
                    }
                }
            }
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the receipt-producing Forge install cutover consumes this capability next"
    )
)]
pub(crate) fn spawn_bound_processor_execution(
    base_receipt: KnownGoodInstallReceipt,
    execution: BoundForgeProcessorExecution,
    workspace: LoaderWorkspace,
) -> BoundProcessorExecutionHandle {
    let (continuation, plan) = execution.into_parts();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_owned_execution(
        base_receipt,
        continuation,
        plan,
        workspace,
        cancel_rx,
        progress_tx,
    ));
    BoundProcessorExecutionHandle {
        cancel: Some(cancel_tx),
        progress: progress_rx,
        task: Some(task),
    }
}

async fn run_owned_execution(
    base_receipt: KnownGoodInstallReceipt,
    continuation: BoundForgeInstallerContinuation,
    plan: BoundProcessorPlan,
    loader_workspace: LoaderWorkspace,
    mut cancel: oneshot::Receiver<()>,
    progress: mpsc::UnboundedSender<BoundProcessorProgress>,
) -> Result<BoundProcessorExecutionResult, BoundProcessorError> {
    if continuation.version().inherits_from != base_receipt.version_id()
        || base_receipt.effective_version().id != base_receipt.version_id()
        || continuation.version().id != loader_workspace.target_version_id()
    {
        return match loader_workspace.cleanup() {
            Ok(()) => Err(BoundProcessorError::Authority),
            Err(_) => Err(BoundProcessorError::Cleanup),
        };
    }
    let minecraft_version = base_receipt.version_id().to_string();
    let processor_workspace = loader_workspace
        .prepare_processor_stage(&minecraft_version)
        .map_err(|_| BoundProcessorError::Stage);
    let mut processor_workspace = match processor_workspace {
        Ok(workspace) => Some(workspace),
        Err(error) => {
            return match loader_workspace.cleanup() {
                Ok(()) => Err(error),
                Err(_) => Err(BoundProcessorError::Cleanup),
            };
        }
    };

    let execution = execute_in_workspace(
        &loader_workspace,
        &base_receipt,
        &continuation,
        &plan,
        processor_workspace
            .as_ref()
            .ok_or(BoundProcessorError::Stage)?,
        &mut cancel,
        &progress,
    )
    .await;
    let processor_cleanup = processor_workspace
        .take()
        .ok_or(BoundProcessorError::Cleanup)
        .and_then(|workspace| {
            workspace
                .cleanup()
                .map_err(|_| BoundProcessorError::Cleanup)
        });
    let loader_cleanup = loader_workspace
        .cleanup()
        .map_err(|_| BoundProcessorError::Cleanup);
    processor_cleanup?;
    loader_cleanup?;
    execution.map(|outputs| BoundProcessorExecutionResult {
        base_receipt,
        continuation,
        outputs,
    })
}

async fn execute_in_workspace(
    loader_workspace: &LoaderWorkspace,
    base_receipt: &KnownGoodInstallReceipt,
    continuation: &BoundForgeInstallerContinuation,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    cancel: &mut oneshot::Receiver<()>,
    progress: &mpsc::UnboundedSender<BoundProcessorProgress>,
) -> Result<VerifiedProcessorOutputs, BoundProcessorError> {
    check_cancel(cancel)?;
    let mut authority = stage_inputs(
        loader_workspace,
        base_receipt,
        continuation,
        plan,
        workspace,
        cancel,
    )
    .await?;
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)?;
    workspace
        .revalidate()
        .map_err(|_| BoundProcessorError::Stage)?;
    let initial_stage = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;

    let java_version = base_receipt.effective_version().java_version.clone();
    let runtime = ensure_axial_managed_processor_runtime(&java_version)
        .await
        .map_err(|_| BoundProcessorError::Runtime)?;
    check_cancel(cancel)?;

    let mut verified = BTreeMap::new();
    for (index, step) in plan.steps.iter().enumerate() {
        progress
            .send(BoundProcessorProgress {
                current: index + 1,
                total: plan.steps.len(),
            })
            .ok();
        let outputs = run_step(
            step,
            plan,
            workspace,
            &runtime,
            base_receipt.version_id(),
            &mut authority,
            cancel,
        )
        .await?;
        for (path, output) in outputs {
            authority.libraries.insert(
                path.clone(),
                AuthenticatedBytes {
                    size: output.size,
                    sha1: output.sha1,
                },
            );
            if output.terminal {
                let bytes = output.bytes.ok_or(BoundProcessorError::Authority)?;
                verified.insert(
                    path,
                    VerifiedProcessorOutput {
                        bytes: Arc::from(bytes),
                        size: output.size,
                        sha1: output.sha1,
                    },
                );
            }
        }
    }
    final_rescan(
        workspace,
        plan,
        base_receipt.version_id(),
        &authority,
        &initial_stage,
    )?;
    Ok(VerifiedProcessorOutputs { entries: verified })
}

struct AuthenticatedBytes {
    size: u64,
    sha1: [u8; 20],
}

struct StagedAuthority {
    libraries: BTreeMap<ArtifactRelativePath, AuthenticatedBytes>,
    version: AuthenticatedBytes,
    processor_data: BTreeMap<ArtifactRelativePath, AuthenticatedBytes>,
    installer: Option<AuthenticatedBytes>,
}

async fn stage_inputs(
    loader_workspace: &LoaderWorkspace,
    base_receipt: &KnownGoodInstallReceipt,
    continuation: &BoundForgeInstallerContinuation,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<StagedAuthority, BoundProcessorError> {
    let mut libraries = BTreeMap::new();
    for (path, contract) in &plan.input_artifacts {
        check_cancel(cancel)?;
        let bytes = match contract.source {
            super::forge_installer::BoundProcessorInputSource::Download => loader_workspace
                .read_live_library_authenticated(path, contract.size, &contract.sha1)
                .map_err(|_| BoundProcessorError::Authority)?,
            super::forge_installer::BoundProcessorInputSource::Embedded => {
                let embedded = exact_embedded(continuation.embedded_maven_artifacts(), path)
                    .ok_or(BoundProcessorError::Authority)?;
                authenticate_bytes(embedded.bytes(), contract.size, &contract.sha1)?
            }
        };
        workspace
            .write_library_exact(path, &bytes)
            .await
            .map_err(|_| BoundProcessorError::Stage)?;
        libraries.insert(
            path.clone(),
            AuthenticatedBytes {
                size: bytes.len() as u64,
                sha1: contract.sha1,
            },
        );
    }

    let client_integrity = base_receipt
        .authenticated_client_integrity()
        .map_err(|_| BoundProcessorError::Authority)?;
    let client_name = format!("{}.jar", base_receipt.version_id());
    let client_bytes = loader_workspace
        .read_base_client_authenticated(
            base_receipt.version_id(),
            client_integrity.size,
            client_integrity.sha1.as_deref(),
        )
        .map_err(|_| BoundProcessorError::Authority)?;
    base_receipt
        .authenticate_client_bytes(&client_bytes)
        .map_err(|_| BoundProcessorError::Authority)?;
    let staged_client =
        ArtifactRelativePath::new(&client_name).map_err(|_| BoundProcessorError::Authority)?;
    workspace
        .write_version_exact(&staged_client, &client_bytes)
        .await
        .map_err(|_| BoundProcessorError::Stage)?;

    let version = AuthenticatedBytes {
        size: client_bytes.len() as u64,
        sha1: Sha1::digest(&client_bytes).into(),
    };
    let mut processor_data = BTreeMap::new();
    for (path, bytes) in &plan.installer_data {
        check_cancel(cancel)?;
        workspace
            .write_processor_data_exact(path, bytes)
            .await
            .map_err(|_| BoundProcessorError::Stage)?;
        processor_data.insert(
            path.clone(),
            AuthenticatedBytes {
                size: bytes.len() as u64,
                sha1: Sha1::digest(bytes).into(),
            },
        );
    }
    let installer = if plan_requires_installer(plan) {
        workspace
            .write_installer_exact(continuation.source_bytes())
            .await
            .map_err(|_| BoundProcessorError::Stage)?;
        Some(AuthenticatedBytes {
            size: continuation.source_bytes().len() as u64,
            sha1: Sha1::digest(continuation.source_bytes()).into(),
        })
    } else {
        None
    };
    loader_workspace
        .revalidate()
        .map_err(|_| BoundProcessorError::Stage)?;
    Ok(StagedAuthority {
        libraries,
        version,
        processor_data,
        installer,
    })
}

fn exact_embedded<'a>(
    embedded: &'a [AuthenticatedEmbeddedMavenArtifact],
    path: &ArtifactRelativePath,
) -> Option<&'a AuthenticatedEmbeddedMavenArtifact> {
    let mut matches = embedded
        .iter()
        .filter(|artifact| artifact.relative_path() == path);
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

fn authenticate_bytes(
    bytes: &[u8],
    size: Option<u64>,
    sha1: &[u8; 20],
) -> Result<Vec<u8>, BoundProcessorError> {
    let actual: [u8; 20] = Sha1::digest(bytes).into();
    if size.is_some_and(|size| size != bytes.len() as u64) || &actual != sha1 {
        return Err(BoundProcessorError::Authority);
    }
    Ok(bytes.to_vec())
}

fn plan_requires_installer(plan: &BoundProcessorPlan) -> bool {
    plan.steps
        .iter()
        .flat_map(|step| &step.args)
        .any(|argument| {
            matches!(
                argument,
                BoundProcessorArgument::Template(parts)
                    if parts.iter().any(|part| matches!(
                        part,
                        BoundProcessorArgumentPart::BuiltinToken(ProcessorBuiltinToken::Installer)
                    ))
            )
        })
}

async fn run_step(
    step: &BoundProcessorStep,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    runtime: &ProcessorRuntime,
    minecraft_version: &str,
    authority: &mut StagedAuthority,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<BTreeMap<ArtifactRelativePath, VerifiedStepOutput>, BoundProcessorError> {
    check_cancel(cancel)?;
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)?;
    for output in &step.outputs {
        workspace
            .ensure_library_parent(&output.artifact.relative_path)
            .map_err(|_| BoundProcessorError::Stage)?;
    }
    let before_root = workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)?;
    let before_stage = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;
    let jar_bytes = staged_artifact_bytes(workspace, &step.jar, &authority.libraries)?;
    let main_class = processor_main_class(&jar_bytes)?;
    let classpath = render_classpath(step, workspace)?;
    let arguments = step
        .args
        .iter()
        .map(|argument| render_argument(argument, plan, workspace, minecraft_version))
        .collect::<Result<Vec<_>, _>>()?;
    let bootstrap_environment = processor_bootstrap_environment()?;
    reauthenticate_step_dependencies(step, plan, workspace, authority, minecraft_version)?;
    let java = runtime
        .revalidate_cli_executable()
        .map_err(|_| BoundProcessorError::Runtime)?;
    let mut command = Command::new(java);
    command
        .env_clear()
        .current_dir(workspace.root_path())
        .arg("-cp")
        .arg(classpath)
        .arg(main_class)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    set_processor_environment(&mut command, workspace, &bootstrap_environment);
    let mut child = command.spawn().map_err(|_| BoundProcessorError::Spawn)?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_and_wait(&mut child).await?;
            return Err(BoundProcessorError::Spawn);
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_and_wait(&mut child).await?;
            return Err(BoundProcessorError::Spawn);
        }
    };
    let total = Arc::new(AtomicUsize::new(0));
    let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(drain_pipe(stdout, total.clone(), pipe_tx.clone()));
    let stderr_task = tokio::spawn(drain_pipe(stderr, total, pipe_tx));
    let process_result = wait_for_direct_child(&mut child, cancel, &mut pipe_rx).await;
    stdout_task.abort();
    stderr_task.abort();
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let runtime_result = runtime
        .revalidate_cli_executable()
        .map_err(|_| BoundProcessorError::Runtime);
    process_result?;
    runtime_result?;
    workspace
        .revalidate()
        .map_err(|_| BoundProcessorError::Stage)?;
    let after_stage = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;
    let after_root = workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)?;
    verify_step_diff(step, &before_root, &after_root, &before_stage, &after_stage)?;

    let mut outputs = BTreeMap::new();
    for output in &step.outputs {
        let fact = after_root
            .files()
            .get(&library_root_path(&output.artifact.relative_path)?)
            .ok_or(BoundProcessorError::Stage)?;
        if fact.sha1() != &output.sha1 {
            return Err(BoundProcessorError::Authority);
        }
        let bytes = workspace
            .read_library_authenticated(
                &output.artifact.relative_path,
                Some(fact.size()),
                &output.sha1,
            )
            .map_err(|_| BoundProcessorError::Authority)?;
        let expected_size = match output.role {
            BoundProcessorOutputRole::Intermediate => None,
            BoundProcessorOutputRole::Terminal { expected_size } => expected_size,
        };
        if expected_size.is_some_and(|size| size != fact.size()) {
            return Err(BoundProcessorError::Authority);
        }
        let terminal = matches!(output.role, BoundProcessorOutputRole::Terminal { .. });
        outputs.insert(
            output.artifact.relative_path.clone(),
            VerifiedStepOutput {
                size: fact.size(),
                sha1: output.sha1,
                bytes: terminal.then_some(bytes),
                terminal,
            },
        );
    }
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)?;
    let settled = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;
    verify_clean_stage_diff(step, &before_stage, &settled)?;
    Ok(outputs)
}

fn staged_artifact_bytes(
    workspace: &ProcessorWorkspace,
    artifact: &BoundProcessorArtifact,
    authority: &BTreeMap<ArtifactRelativePath, AuthenticatedBytes>,
) -> Result<Vec<u8>, BoundProcessorError> {
    let authority = authority
        .get(&artifact.relative_path)
        .ok_or(BoundProcessorError::Authority)?;
    let bytes = workspace
        .read_library_authenticated(
            &artifact.relative_path,
            Some(authority.size),
            &authority.sha1,
        )
        .map_err(|_| BoundProcessorError::Authority)?;
    Ok(bytes)
}

fn reauthenticate_step_dependencies(
    step: &BoundProcessorStep,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    authority: &StagedAuthority,
    minecraft_version: &str,
) -> Result<(), BoundProcessorError> {
    let pre_spawn = workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)?;
    for artifact in std::iter::once(&step.jar).chain(&step.classpath) {
        staged_artifact_bytes(workspace, artifact, &authority.libraries)?;
    }
    for argument in &step.args {
        match argument {
            BoundProcessorArgument::Artifact(artifact) => {
                staged_artifact_bytes(workspace, artifact, &authority.libraries)?;
            }
            BoundProcessorArgument::OutputArtifact(artifact) => {
                validate_fresh_output_target(step, artifact, &pre_spawn)?;
            }
            BoundProcessorArgument::Template(parts) => {
                for token in parts.iter().filter_map(|part| match part {
                    BoundProcessorArgumentPart::DataToken(token) => Some(token),
                    _ => None,
                }) {
                    if let Some(BoundProcessorData::Artifact(artifact)) = plan.data.get(token) {
                        staged_artifact_bytes(workspace, artifact, &authority.libraries)?;
                    } else if let Some(BoundProcessorData::InstallerData(path)) =
                        plan.data.get(token)
                    {
                        let facts = authority
                            .processor_data
                            .get(path)
                            .ok_or(BoundProcessorError::Authority)?;
                        workspace
                            .read_processor_data_authenticated(path, Some(facts.size), &facts.sha1)
                            .map_err(|_| BoundProcessorError::Authority)?;
                    }
                }
                for token in parts.iter().filter_map(|part| match part {
                    BoundProcessorArgumentPart::OutputToken(token) => Some(token),
                    _ => None,
                }) {
                    let BoundProcessorData::Artifact(artifact) =
                        plan.data.get(token).ok_or(BoundProcessorError::Authority)?
                    else {
                        return Err(BoundProcessorError::Authority);
                    };
                    validate_fresh_output_target(step, artifact, &pre_spawn)?;
                }
                for builtin in parts.iter().filter_map(|part| match part {
                    BoundProcessorArgumentPart::BuiltinToken(token) => Some(*token),
                    _ => None,
                }) {
                    match builtin {
                        ProcessorBuiltinToken::MinecraftJar => {
                            let path =
                                ArtifactRelativePath::new(&format!("{minecraft_version}.jar"))
                                    .map_err(|_| BoundProcessorError::Authority)?;
                            workspace
                                .read_version_authenticated(
                                    &path,
                                    Some(authority.version.size),
                                    &authority.version.sha1,
                                )
                                .map_err(|_| BoundProcessorError::Authority)?;
                        }
                        ProcessorBuiltinToken::Installer => {
                            let facts = authority
                                .installer
                                .as_ref()
                                .ok_or(BoundProcessorError::Authority)?;
                            workspace
                                .read_installer_authenticated(Some(facts.size), &facts.sha1)
                                .map_err(|_| BoundProcessorError::Authority)?;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_fresh_output_target(
    step: &BoundProcessorStep,
    artifact: &BoundProcessorArtifact,
    snapshot: &ManagedTreeSnapshot,
) -> Result<(), BoundProcessorError> {
    if !step
        .outputs
        .iter()
        .any(|output| output.artifact.relative_path == artifact.relative_path)
        || snapshot
            .files()
            .contains_key(&library_root_path(&artifact.relative_path)?)
    {
        return Err(BoundProcessorError::Authority);
    }
    Ok(())
}

fn render_classpath(
    step: &BoundProcessorStep,
    workspace: &ProcessorWorkspace,
) -> Result<OsString, BoundProcessorError> {
    let paths = std::iter::once(&step.jar)
        .chain(&step.classpath)
        .map(|artifact| {
            workspace
                .libraries_path()
                .join(artifact.relative_path.as_str())
        });
    std::env::join_paths(paths).map_err(|_| BoundProcessorError::Authority)
}

fn render_argument(
    argument: &BoundProcessorArgument,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    minecraft_version: &str,
) -> Result<OsString, BoundProcessorError> {
    match argument {
        BoundProcessorArgument::Artifact(artifact)
        | BoundProcessorArgument::OutputArtifact(artifact) => Ok(workspace
            .libraries_path()
            .join(artifact.relative_path.as_str())
            .into_os_string()),
        BoundProcessorArgument::Template(parts) => {
            let mut rendered = OsString::new();
            for part in parts {
                match part {
                    BoundProcessorArgumentPart::Literal(value) => rendered.push(value),
                    BoundProcessorArgumentPart::DataToken(token)
                    | BoundProcessorArgumentPart::OutputToken(token) => {
                        push_data_value(&mut rendered, token, plan, workspace)?;
                    }
                    BoundProcessorArgumentPart::BuiltinToken(token) => {
                        push_builtin(&mut rendered, *token, workspace, minecraft_version)?;
                    }
                }
            }
            Ok(rendered)
        }
    }
}

fn push_data_value(
    rendered: &mut OsString,
    token: &str,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
) -> Result<(), BoundProcessorError> {
    match plan.data.get(token).ok_or(BoundProcessorError::Authority)? {
        BoundProcessorData::Artifact(artifact) => {
            rendered.push(
                workspace
                    .libraries_path()
                    .join(artifact.relative_path.as_str()),
            );
        }
        BoundProcessorData::InstallerData(path) => {
            rendered.push(workspace.processor_data_path().join(path.as_str()));
        }
        BoundProcessorData::Literal(value) => rendered.push(value),
    }
    Ok(())
}

fn push_builtin(
    rendered: &mut OsString,
    token: ProcessorBuiltinToken,
    workspace: &ProcessorWorkspace,
    minecraft_version: &str,
) -> Result<(), BoundProcessorError> {
    match token {
        ProcessorBuiltinToken::MinecraftJar => {
            rendered.push(
                workspace
                    .version_path()
                    .join(format!("{minecraft_version}.jar")),
            );
        }
        ProcessorBuiltinToken::Side => rendered.push("client"),
        ProcessorBuiltinToken::MinecraftVersion => rendered.push(minecraft_version),
        ProcessorBuiltinToken::Root => rendered.push(workspace.root_path()),
        ProcessorBuiltinToken::LibraryDir => rendered.push(workspace.libraries_path()),
        ProcessorBuiltinToken::Installer => rendered.push(workspace.installer_path()),
    }
    Ok(())
}

fn processor_main_class(jar: &[u8]) -> Result<String, BoundProcessorError> {
    let mut archive =
        ZipArchive::new(Cursor::new(jar)).map_err(|_| BoundProcessorError::Manifest)?;
    if archive.len() > MAX_PROCESSOR_JAR_ENTRIES {
        return Err(BoundProcessorError::Manifest);
    }
    let mut manifest = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| BoundProcessorError::Manifest)?;
        if entry.name().eq_ignore_ascii_case("META-INF/MANIFEST.MF") {
            if entry.name() != "META-INF/MANIFEST.MF" || manifest.is_some() {
                return Err(BoundProcessorError::Manifest);
            }
            let mut bytes = Vec::new();
            entry
                .by_ref()
                .take(MAX_MANIFEST_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| BoundProcessorError::Manifest)?;
            if bytes.len() as u64 > MAX_MANIFEST_BYTES {
                return Err(BoundProcessorError::Manifest);
            }
            manifest = Some(bytes);
        }
    }
    let text = std::str::from_utf8(manifest.as_deref().ok_or(BoundProcessorError::Manifest)?)
        .map_err(|_| BoundProcessorError::Manifest)?;
    let attributes = manifest_attributes(text)?;
    let mut values = attributes
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("Main-Class"));
    let main = values.next().ok_or(BoundProcessorError::Manifest)?.1.trim();
    if values.next().is_some() || !valid_main_class(main) {
        return Err(BoundProcessorError::Manifest);
    }
    Ok(main.to_string())
}

fn manifest_attributes(text: &str) -> Result<Vec<(String, String)>, BoundProcessorError> {
    let mut attributes: Vec<(String, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            break;
        }
        if let Some(continuation) = line.strip_prefix(' ') {
            attributes
                .last_mut()
                .ok_or(BoundProcessorError::Manifest)?
                .1
                .push_str(continuation);
            continue;
        }
        let (name, value) = line.split_once(": ").ok_or(BoundProcessorError::Manifest)?;
        if name.is_empty() {
            return Err(BoundProcessorError::Manifest);
        }
        attributes.push((name.to_string(), value.to_string()));
    }
    Ok(attributes)
}

fn valid_main_class(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MAIN_CLASS_BYTES
        && value.split('.').all(|segment| {
            let mut bytes = segment.bytes();
            bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$'))
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
        })
}

struct ProcessorBootstrapEnvironment {
    #[cfg(windows)]
    system_root: Option<OsString>,
    #[cfg(windows)]
    windir: Option<OsString>,
}

fn processor_bootstrap_environment() -> Result<ProcessorBootstrapEnvironment, BoundProcessorError> {
    #[cfg(windows)]
    {
        let read = |name| -> Result<Option<OsString>, BoundProcessorError> {
            let value = std::env::var_os(name);
            if value
                .as_ref()
                .is_some_and(|value| !std::path::Path::new(value).is_absolute())
            {
                return Err(BoundProcessorError::Spawn);
            }
            Ok(value)
        };
        return Ok(ProcessorBootstrapEnvironment {
            system_root: read("SystemRoot")?,
            windir: read("WINDIR")?,
        });
    }
    #[cfg(not(windows))]
    Ok(ProcessorBootstrapEnvironment {})
}

fn set_processor_environment(
    command: &mut Command,
    workspace: &ProcessorWorkspace,
    bootstrap: &ProcessorBootstrapEnvironment,
) {
    #[cfg(not(windows))]
    let _ = bootstrap;
    command
        .env("HOME", workspace.home_path())
        .env("TMPDIR", workspace.temp_path())
        .env("TMP", workspace.temp_path())
        .env("TEMP", workspace.temp_path())
        .env("TZ", "UTC");
    #[cfg(unix)]
    command.env("LC_ALL", "C").env("LANG", "C");
    #[cfg(windows)]
    {
        command.env("USERPROFILE", workspace.home_path());
        if let Some(value) = &bootstrap.system_root {
            command.env("SystemRoot", value);
        }
        if let Some(value) = &bootstrap.windir {
            command.env("WINDIR", value);
        }
    }
}

enum PipeEvent {
    Finished,
    Limit,
    Read,
}

async fn drain_pipe(
    mut pipe: impl AsyncRead + Unpin,
    aggregate: Arc<AtomicUsize>,
    events: mpsc::UnboundedSender<PipeEvent>,
) {
    let mut stream_bytes = 0_usize;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match pipe.read(&mut buffer).await {
            Ok(0) => {
                events.send(PipeEvent::Finished).ok();
                return;
            }
            Ok(read) => read,
            Err(_) => {
                events.send(PipeEvent::Read).ok();
                return;
            }
        };
        stream_bytes = stream_bytes.saturating_add(read);
        let prior = aggregate.fetch_add(read, Ordering::Relaxed);
        if stream_bytes > MAX_PROCESS_OUTPUT_BYTES
            || prior.saturating_add(read) > MAX_PROCESS_OUTPUT_TOTAL_BYTES
        {
            events.send(PipeEvent::Limit).ok();
            return;
        }
    }
}

async fn wait_for_direct_child(
    child: &mut Child,
    cancel: &mut oneshot::Receiver<()>,
    pipe: &mut mpsc::UnboundedReceiver<PipeEvent>,
) -> Result<(), BoundProcessorError> {
    wait_for_direct_child_with_limits(child, cancel, pipe, PROCESSOR_TIMEOUT, PIPE_DRAIN_TIMEOUT)
        .await
}

async fn wait_for_direct_child_with_limits(
    child: &mut Child,
    cancel: &mut oneshot::Receiver<()>,
    pipe: &mut mpsc::UnboundedReceiver<PipeEvent>,
    process_timeout: Duration,
    drain_timeout: Duration,
) -> Result<(), BoundProcessorError> {
    let deadline = tokio::time::sleep(process_timeout);
    tokio::pin!(deadline);
    let mut finished_pipes = 0_usize;
    loop {
        let outcome = tokio::select! {
            biased;
            event = pipe.recv(), if finished_pipes < 2 => match event {
                Some(PipeEvent::Finished) => {
                    finished_pipes += 1;
                    continue;
                }
                Some(PipeEvent::Limit) => Some(BoundProcessorError::OutputLimit),
                Some(PipeEvent::Read) | None => Some(BoundProcessorError::Unsuccessful),
            },
            _ = &mut *cancel => Some(BoundProcessorError::Cancelled),
            _ = &mut deadline => Some(BoundProcessorError::Timeout),
            status = child.wait() => match status {
                Ok(status) if status.success() => None,
                Ok(_) => return Err(BoundProcessorError::Unsuccessful),
                Err(_) => Some(BoundProcessorError::Unsuccessful),
            },
        };
        if let Some(error) = outcome {
            let _ = child.start_kill();
            child
                .wait()
                .await
                .map_err(|_| BoundProcessorError::Unsuccessful)?;
            return Err(error);
        }
        break;
    }

    let drains = async {
        while finished_pipes < 2 {
            match pipe.recv().await {
                Some(PipeEvent::Finished) => finished_pipes += 1,
                Some(PipeEvent::Limit) => return Err(BoundProcessorError::OutputLimit),
                Some(PipeEvent::Read) | None => return Err(BoundProcessorError::Unsuccessful),
            }
        }
        Ok(())
    };
    tokio::time::timeout(drain_timeout, drains)
        .await
        .map_err(|_| BoundProcessorError::Unsuccessful)?
}

async fn kill_and_wait(child: &mut Child) -> Result<(), BoundProcessorError> {
    let _ = child.start_kill();
    child
        .wait()
        .await
        .map(|_| ())
        .map_err(|_| BoundProcessorError::Unsuccessful)
}

fn verify_step_diff(
    step: &BoundProcessorStep,
    before_root: &ManagedTreeSnapshot,
    after_root: &ManagedTreeSnapshot,
    before_stage: &ManagedTreeSnapshot,
    after_stage: &ManagedTreeSnapshot,
) -> Result<(), BoundProcessorError> {
    let expected_root = step
        .outputs
        .iter()
        .map(|output| library_root_path(&output.artifact.relative_path))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected_stage = expected_root
        .iter()
        .map(|path| ArtifactRelativePath::new(&format!("root/{}", path.as_str())))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| BoundProcessorError::Stage)?;
    exact_added_files(before_root, after_root, &expected_root)?;
    let diff = before_stage.diff(after_stage);
    let root_additions = diff
        .added_files()
        .keys()
        .filter(|path| path.as_str().starts_with("root/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let scratch_file = |path: &ArtifactRelativePath| {
        path.as_str().starts_with("home/") || path.as_str().starts_with("tmp/")
    };
    let scratch_directory = |path: &ArtifactRelativePath| {
        path.as_str() == "home" || path.as_str() == "tmp" || scratch_file(path)
    };
    if root_additions != expected_stage
        || diff
            .added_files()
            .keys()
            .any(|path| !expected_stage.contains(path) && !scratch_file(path))
        || diff.modified_files().keys().any(|path| !scratch_file(path))
        || !diff.removed_files().is_empty()
        || diff
            .added_directories()
            .iter()
            .any(|path| !scratch_directory(path))
        || !diff.removed_directories().is_empty()
    {
        return Err(BoundProcessorError::Stage);
    }
    Ok(())
}

fn verify_clean_stage_diff(
    step: &BoundProcessorStep,
    before: &ManagedTreeSnapshot,
    settled: &ManagedTreeSnapshot,
) -> Result<(), BoundProcessorError> {
    let expected = step
        .outputs
        .iter()
        .map(|output| {
            ArtifactRelativePath::new(&format!(
                "root/libraries/{}",
                output.artifact.relative_path.as_str()
            ))
            .map_err(|_| BoundProcessorError::Authority)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    exact_added_files(before, settled, &expected)
}

fn exact_added_files(
    before: &ManagedTreeSnapshot,
    after: &ManagedTreeSnapshot,
    expected: &BTreeSet<ArtifactRelativePath>,
) -> Result<(), BoundProcessorError> {
    let diff = before.diff(after);
    let added = diff.added_files().keys().cloned().collect::<BTreeSet<_>>();
    if added != *expected
        || !diff.removed_files().is_empty()
        || !diff.modified_files().is_empty()
        || !diff.added_directories().is_empty()
        || !diff.removed_directories().is_empty()
    {
        return Err(BoundProcessorError::Stage);
    }
    Ok(())
}

fn library_root_path(
    relative: &ArtifactRelativePath,
) -> Result<ArtifactRelativePath, BoundProcessorError> {
    ArtifactRelativePath::new(&format!("libraries/{}", relative.as_str()))
        .map_err(|_| BoundProcessorError::Authority)
}

fn final_rescan(
    workspace: &ProcessorWorkspace,
    plan: &BoundProcessorPlan,
    minecraft_version: &str,
    authority: &StagedAuthority,
    initial_stage: &ManagedTreeSnapshot,
) -> Result<(), BoundProcessorError> {
    workspace
        .revalidate()
        .map_err(|_| BoundProcessorError::Stage)?;
    let before = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;
    let mut expected = authority
        .libraries
        .keys()
        .map(|path| {
            ArtifactRelativePath::new(&format!("root/libraries/{}", path.as_str()))
                .map_err(|_| BoundProcessorError::Authority)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    expected.insert(
        ArtifactRelativePath::new(&format!(
            "root/versions/{minecraft_version}/{minecraft_version}.jar"
        ))
        .map_err(|_| BoundProcessorError::Authority)?,
    );
    for path in plan.installer_data.keys() {
        expected.insert(
            ArtifactRelativePath::new(&format!("root/processor-data/{}", path.as_str()))
                .map_err(|_| BoundProcessorError::Authority)?,
        );
    }
    if plan_requires_installer(plan) {
        expected.insert(
            ArtifactRelativePath::new("root/installer.jar")
                .map_err(|_| BoundProcessorError::Authority)?,
        );
    }
    if before.files().keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err(BoundProcessorError::Stage);
    }
    let mut expected_directories = initial_stage.directories().clone();
    for output in plan.steps.iter().flat_map(|step| &step.outputs) {
        let staged = format!("root/libraries/{}", output.artifact.relative_path.as_str());
        let mut segments = staged.split('/').collect::<Vec<_>>();
        segments.pop();
        while !segments.is_empty() {
            expected_directories.insert(
                ArtifactRelativePath::new(&segments.join("/"))
                    .map_err(|_| BoundProcessorError::Authority)?,
            );
            segments.pop();
        }
    }
    if before.directories() != &expected_directories {
        return Err(BoundProcessorError::Stage);
    }
    if initial_stage.files().iter().any(|(path, fact)| {
        before
            .files()
            .get(path)
            .is_none_or(|current| current != fact)
    }) {
        return Err(BoundProcessorError::Authority);
    }
    for (path, authenticated) in &authority.libraries {
        let bytes = workspace
            .read_library_authenticated(path, Some(authenticated.size), &authenticated.sha1)
            .map_err(|_| BoundProcessorError::Authority)?;
        drop(bytes);
    }
    let after = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;
    if before != after {
        return Err(BoundProcessorError::Stage);
    }
    Ok(())
}

fn check_cancel(cancel: &mut oneshot::Receiver<()>) -> Result<(), BoundProcessorError> {
    match cancel.try_recv() {
        Ok(()) | Err(oneshot::error::TryRecvError::Closed) => Err(BoundProcessorError::Cancelled),
        Err(oneshot::error::TryRecvError::Empty) => Ok(()),
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the receipt-producing Forge install cutover consumes this carrier next"
    )
)]
impl VerifiedProcessorOutputs {
    pub(crate) fn none() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub(crate) fn into_entries(self) -> BTreeMap<ArtifactRelativePath, VerifiedProcessorOutput> {
        self.entries
    }

    #[cfg(test)]
    pub(crate) fn from_test_terminal(entries: Vec<(ArtifactRelativePath, Vec<u8>)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(path, bytes)| {
                    let bytes = Arc::<[u8]>::from(bytes);
                    let size = bytes.len() as u64;
                    let sha1 = Sha1::digest(bytes.as_ref()).into();
                    (path, VerifiedProcessorOutput { bytes, size, sha1 })
                })
                .collect(),
        }
    }
}

impl VerifiedProcessorOutput {
    pub(crate) fn into_parts(self) -> (Arc<[u8]>, u64, [u8; 20]) {
        (self.bytes, self.size, self.sha1)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthenticatedBytes, BoundProcessorError, PipeEvent, StagedAuthority, drain_pipe,
        manifest_attributes, processor_main_class, reauthenticate_step_dependencies,
        valid_main_class, wait_for_direct_child_with_limits,
    };
    use crate::artifact_path::ArtifactRelativePath;
    use crate::loaders::forge_installer::{
        BoundProcessorArgument, BoundProcessorArgumentPart, BoundProcessorArtifact,
        BoundProcessorData, BoundProcessorOutput, BoundProcessorOutputRole, BoundProcessorPlan,
        BoundProcessorStep, ProcessorBuiltinToken,
    };
    use crate::loaders::workspace::cleanup::prepare_fresh_work_dir;
    use sha1::{Digest as _, Sha1};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::sync::{Arc, atomic::AtomicUsize};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::sync::{mpsc, oneshot};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn parses_manifest_continuations_and_validates_binary_name() {
        let attributes =
            manifest_attributes("Manifest-Version: 1.0\r\nMain-Class: example.\r\n Main\r\n\r\n")
                .expect("manifest");
        assert_eq!(attributes[1].1, "example.Main");
        assert!(valid_main_class("example.Main$Nested"));
        assert!(!valid_main_class("example/Main"));
        assert!(!valid_main_class("example..Main"));
    }

    #[test]
    fn reads_exact_main_class_from_bounded_jar() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("META-INF/MANIFEST.MF", SimpleFileOptions::default())
            .expect("manifest entry");
        writer
            .write_all(b"Manifest-Version: 1.0\r\nMain-Class: example.Main\r\n\r\n")
            .expect("manifest bytes");
        let jar = writer.finish().expect("jar").into_inner();
        assert_eq!(
            processor_main_class(&jar).expect("main class"),
            "example.Main"
        );
    }

    #[test]
    fn rejects_portable_manifest_alias() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("meta-inf/manifest.mf", SimpleFileOptions::default())
            .expect("manifest entry");
        writer
            .write_all(b"Main-Class: example.Main\r\n")
            .expect("manifest bytes");
        let jar = writer.finish().expect("jar").into_inner();
        assert!(processor_main_class(&jar).is_err());
    }

    #[test]
    fn processor_errors_are_closed_static_and_redacted() {
        for error in [
            BoundProcessorError::Authority,
            BoundProcessorError::Stage,
            BoundProcessorError::Runtime,
            BoundProcessorError::Manifest,
            BoundProcessorError::Spawn,
            BoundProcessorError::Timeout,
            BoundProcessorError::OutputLimit,
            BoundProcessorError::Unsuccessful,
            BoundProcessorError::Cancelled,
            BoundProcessorError::Cleanup,
            BoundProcessorError::OwnerStopped,
        ] {
            let rendered = error.to_string();
            assert!(!rendered.is_empty());
            assert!(!rendered.contains("PRIVATE"));
            assert!(!rendered.contains('/'));
            assert!(!rendered.contains('\\'));
            assert!(!rendered.contains("java"));
            assert!(!rendered.contains("Main-Class"));
        }
    }

    #[tokio::test]
    async fn typed_non_library_dependencies_reject_staged_tampering() {
        let root = temp_dir("typed-dependency-tamper");
        fs::create_dir_all(&root).expect("Minecraft root");
        let loader = prepare_fresh_work_dir(&root, "forge-target").expect("loader workspace");
        let workspace = loader
            .prepare_processor_stage("1.21.5")
            .expect("processor workspace");
        let jar = ArtifactRelativePath::new("example/processor.jar").expect("jar path");
        let data = ArtifactRelativePath::new("patch/client.bin").expect("data path");
        let version = ArtifactRelativePath::new("1.21.5.jar").expect("version path");
        workspace
            .write_library_exact(&jar, b"jar")
            .await
            .expect("jar stage");
        workspace
            .write_processor_data_exact(&data, b"patch")
            .await
            .expect("data stage");
        workspace
            .write_version_exact(&version, b"client")
            .await
            .expect("version stage");
        workspace
            .write_installer_exact(b"installer")
            .await
            .expect("installer stage");

        let facts = |bytes: &[u8]| AuthenticatedBytes {
            size: bytes.len() as u64,
            sha1: Sha1::digest(bytes).into(),
        };
        let authority = StagedAuthority {
            libraries: BTreeMap::from([(jar.clone(), facts(b"jar"))]),
            version: facts(b"client"),
            processor_data: BTreeMap::from([(data.clone(), facts(b"patch"))]),
            installer: Some(facts(b"installer")),
        };
        let artifact = BoundProcessorArtifact {
            coordinate: "example:processor:1".to_string(),
            relative_path: jar,
        };
        let step = BoundProcessorStep {
            jar: artifact,
            classpath: Vec::new(),
            args: vec![BoundProcessorArgument::Template(vec![
                BoundProcessorArgumentPart::DataToken("PATCH".to_string()),
                BoundProcessorArgumentPart::BuiltinToken(ProcessorBuiltinToken::MinecraftJar),
                BoundProcessorArgumentPart::BuiltinToken(ProcessorBuiltinToken::Installer),
            ])],
            outputs: Vec::new(),
        };
        let plan = BoundProcessorPlan {
            steps: Vec::new(),
            data: BTreeMap::from([("PATCH".to_string(), BoundProcessorData::InstallerData(data))]),
            installer_data: BTreeMap::new(),
            input_artifacts: BTreeMap::new(),
        };
        reauthenticate_step_dependencies(&step, &plan, &workspace, &authority, "1.21.5")
            .expect("authenticated dependencies");

        fs::write(
            workspace.libraries_path().join("example/processor.jar"),
            b"changed",
        )
        .expect("tamper staged jar");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, &workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_library_exact(
                &ArtifactRelativePath::new("example/processor.jar").expect("jar path"),
                b"jar",
            )
            .await
            .expect("restore jar");

        fs::write(
            workspace.processor_data_path().join("patch/client.bin"),
            b"changed",
        )
        .expect("tamper staged data");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, &workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_processor_data_exact(
                &ArtifactRelativePath::new("patch/client.bin").expect("data path"),
                b"patch",
            )
            .await
            .expect("restore data");

        fs::write(workspace.version_path().join("1.21.5.jar"), b"changed")
            .expect("tamper staged client");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, &workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_version_exact(
                &ArtifactRelativePath::new("1.21.5.jar").expect("version path"),
                b"client",
            )
            .await
            .expect("restore client");

        fs::write(workspace.installer_path(), b"changed").expect("tamper installer");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, &workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_installer_exact(b"installer")
            .await
            .expect("restore installer");

        let output_path = ArtifactRelativePath::new("example/generated.jar").expect("output path");
        let output_artifact = BoundProcessorArtifact {
            coordinate: "example:generated:1".to_string(),
            relative_path: output_path.clone(),
        };
        let output_step = BoundProcessorStep {
            jar: BoundProcessorArtifact {
                coordinate: "example:processor:1".to_string(),
                relative_path: ArtifactRelativePath::new("example/processor.jar")
                    .expect("jar path"),
            },
            classpath: Vec::new(),
            args: vec![
                BoundProcessorArgument::OutputArtifact(output_artifact.clone()),
                BoundProcessorArgument::Template(vec![BoundProcessorArgumentPart::OutputToken(
                    "OUT".to_string(),
                )]),
            ],
            outputs: vec![BoundProcessorOutput {
                artifact: output_artifact.clone(),
                sha1: Sha1::digest(b"generated").into(),
                role: BoundProcessorOutputRole::Terminal {
                    expected_size: Some(9),
                },
            }],
        };
        let output_plan = BoundProcessorPlan {
            steps: Vec::new(),
            data: BTreeMap::from([(
                "OUT".to_string(),
                BoundProcessorData::Artifact(output_artifact),
            )]),
            installer_data: BTreeMap::new(),
            input_artifacts: BTreeMap::new(),
        };
        reauthenticate_step_dependencies(
            &output_step,
            &output_plan,
            &workspace,
            &authority,
            "1.21.5",
        )
        .expect("fresh declared output target");
        workspace
            .write_library_exact(&output_path, b"preexisting")
            .await
            .expect("precreate output");
        assert!(matches!(
            reauthenticate_step_dependencies(
                &output_step,
                &output_plan,
                &workspace,
                &authority,
                "1.21.5",
            ),
            Err(BoundProcessorError::Authority)
        ));
        workspace.cleanup().expect("processor cleanup");
        loader.cleanup().expect("loader cleanup");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pipe_reader_reports_hard_stream_limit() {
        let (mut writer, reader) = tokio::io::duplex((1 << 20) + 8192);
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(drain_pipe(reader, Arc::new(AtomicUsize::new(0)), events_tx));
        writer
            .write_all(&vec![b'x'; (1 << 20) + 1])
            .await
            .expect("bounded pipe write");
        drop(writer);
        assert!(matches!(events_rx.recv().await, Some(PipeEvent::Limit)));
        task.await.expect("pipe owner");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_child_nonzero_cancel_and_output_limit_are_reaped() {
        let mut nonzero = Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("nonzero child");
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let (_pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        assert!(matches!(
            wait_for_direct_child_with_limits(
                &mut nonzero,
                &mut cancel_rx,
                &mut pipe_rx,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await,
            Err(BoundProcessorError::Unsuccessful)
        ));
        assert!(nonzero.try_wait().expect("nonzero wait").is_some());

        let mut cancelled = Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .expect("cancelled child");
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        cancel_tx.send(()).expect("cancel signal");
        let (_pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        assert!(matches!(
            wait_for_direct_child_with_limits(
                &mut cancelled,
                &mut cancel_rx,
                &mut pipe_rx,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await,
            Err(BoundProcessorError::Cancelled)
        ));
        assert!(cancelled.try_wait().expect("cancel wait").is_some());

        let mut flooded = Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .expect("flood child");
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        pipe_tx.send(PipeEvent::Limit).expect("limit event");
        assert!(matches!(
            wait_for_direct_child_with_limits(
                &mut flooded,
                &mut cancel_rx,
                &mut pipe_rx,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await,
            Err(BoundProcessorError::OutputLimit)
        ));
        assert!(flooded.try_wait().expect("limit wait").is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_child_timeout_is_reaped() {
        let mut child = Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .expect("timeout child");
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let (_pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        assert!(matches!(
            wait_for_direct_child_with_limits(
                &mut child,
                &mut cancel_rx,
                &mut pipe_rx,
                Duration::from_millis(20),
                Duration::from_millis(20),
            )
            .await,
            Err(BoundProcessorError::Timeout)
        ));
        assert!(child.try_wait().expect("timeout wait").is_some());
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-{label}-{nonce}"))
    }
}
