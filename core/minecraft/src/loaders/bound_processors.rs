use super::forge_installer::{
    BoundForgeInstallExecution, BoundForgeInstallerContinuation, BoundForgeProcessorExecution,
    BoundProcessorAction, BoundProcessorArgument, BoundProcessorArgumentPart,
    BoundProcessorArtifact, BoundProcessorData, BoundProcessorOutputExpectation,
    BoundProcessorOutputRole, BoundProcessorPlan, BoundProcessorStep, ProcessorBuiltinToken,
};
use super::workspace::cleanup::{ProcessorWorkspace, ProcessorWorkspaceOwner};
use crate::download::{AuthenticatedSelectedArtifactSource, ExpectedIntegrity};
use crate::launch::VersionJson;
use crate::managed_fs::ManagedTreeSnapshot;
use crate::portable_path::PortableRelativePath;
use crate::runtime::{ProcessorRuntime, RuntimeSourceReceipt};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use axial_resource::{PhysicalIoClass, PhysicalWorkRequest, process_physical_work};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{Cursor, Read};
use std::path::Path;
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
const MAX_MCP_CONFIG_BYTES: u64 = 1 << 20;
const MAX_MAPPING_BYTES: u64 = 64 << 20;
const MAX_MAIN_CLASS_BYTES: usize = 256;
const MAX_PROCESS_OUTPUT_BYTES: usize = 1 << 20;
const MAX_PROCESS_OUTPUT_TOTAL_BYTES: usize = 2 << 20;
#[cfg(target_os = "linux")]
const MAX_LINUX_PROCESS_STAT_BYTES: u64 = 4096;
#[cfg(any(target_os = "macos", test))]
const MAX_MACOS_PROCESS_GROUP_MEMBERS: usize = 4096;
// XNU searches zombproc for PROC_PIDTBSDINFO only when arg is nonzero.
#[cfg(any(target_os = "macos", test))]
const MACOS_PROC_PIDINFO_FIND_ZOMBIES_ARG: u64 = 1;
const PROCESSOR_TIMEOUT: Duration = Duration::from_secs(120);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const PROCESS_REAP_POLL_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(not(target_os = "linux"))]
const PROCESS_REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const STAGE_WATCH_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(unix)]
struct PendingProcessContainment;

#[cfg(unix)]
struct ProcessContainment {
    group: rustix::process::Pid,
}

#[cfg(unix)]
fn prepare_process_containment(
    command: &mut Command,
) -> Result<PendingProcessContainment, BoundProcessorError> {
    command.process_group(0);
    Ok(PendingProcessContainment)
}

#[cfg(unix)]
impl PendingProcessContainment {
    fn attach(self, child: &Child) -> Result<ProcessContainment, BoundProcessorError> {
        let raw = i32::try_from(child.id().ok_or(BoundProcessorError::Containment)?)
            .map_err(|_| BoundProcessorError::Containment)?;
        let group = rustix::process::Pid::from_raw(raw).ok_or(BoundProcessorError::Containment)?;
        Ok(ProcessContainment { group })
    }
}

#[cfg(unix)]
impl ProcessContainment {
    fn terminate(&self) -> Result<(), BoundProcessorError> {
        terminate_process_group(self.group)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn is_empty(&self) -> Result<bool, BoundProcessorError> {
        match rustix::process::test_kill_process_group(self.group) {
            Ok(()) => Ok(false),
            Err(rustix::io::Errno::SRCH) => Ok(true),
            Err(_) => Err(BoundProcessorError::Unreaped),
        }
    }
}

#[cfg(unix)]
fn terminate_process_group(group: rustix::process::Pid) -> Result<(), BoundProcessorError> {
    match rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(_) => Err(BoundProcessorError::Unreaped),
    }
}

#[cfg(target_os = "macos")]
fn inspect_macos_group_after_owned_kill(
    group: rustix::process::Pid,
) -> Result<MacosGroupSettlement, BoundProcessorError> {
    settle_macos_group_probe(
        rustix::process::test_kill_process_group(group),
        || terminate_process_group(group),
        || macos_process_group_has_only_zombies(group),
    )
}

#[cfg(any(target_os = "macos", all(test, unix)))]
#[derive(Debug, Eq, PartialEq)]
enum MacosGroupSettlement {
    Empty,
    OnlyZombies,
    LiveMembers,
}

#[cfg(any(target_os = "macos", all(test, unix)))]
impl MacosGroupSettlement {
    fn from_zombie_proof(only_zombies: bool) -> Self {
        if only_zombies {
            Self::OnlyZombies
        } else {
            Self::LiveMembers
        }
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn settle_macos_group_probe<Terminate, ProveZombies>(
    probe: Result<(), rustix::io::Errno>,
    terminate: Terminate,
    prove_only_zombies: ProveZombies,
) -> Result<MacosGroupSettlement, BoundProcessorError>
where
    Terminate: FnOnce() -> Result<(), BoundProcessorError>,
    ProveZombies: FnOnce() -> Result<bool, BoundProcessorError>,
{
    match probe {
        Err(rustix::io::Errno::SRCH) => Ok(MacosGroupSettlement::Empty),
        Ok(()) => {
            terminate()?;
            Ok(MacosGroupSettlement::from_zombie_proof(
                prove_only_zombies()?
            ))
        }
        // After our SIGKILL succeeds, Darwin can report EPERM while only
        // unsignalable zombies remain. Accept only the bounded exact-group proof.
        Err(rustix::io::Errno::PERM) => Ok(MacosGroupSettlement::from_zombie_proof(
            prove_only_zombies()?,
        )),
        Err(_) => Err(BoundProcessorError::Unreaped),
    }
}

#[cfg(target_os = "macos")]
fn macos_process_group_is_empty(group: rustix::process::Pid) -> Result<bool, BoundProcessorError> {
    match inspect_macos_group_after_owned_kill(group)? {
        MacosGroupSettlement::Empty => Ok(true),
        MacosGroupSettlement::LiveMembers => Ok(false),
        MacosGroupSettlement::OnlyZombies => Ok(!matches!(
            inspect_macos_group_after_owned_kill(group)?,
            MacosGroupSettlement::LiveMembers
        )),
    }
}

#[cfg(target_os = "macos")]
fn macos_process_group_has_only_zombies(
    group: rustix::process::Pid,
) -> Result<bool, BoundProcessorError> {
    let group = group.as_raw_nonzero().get();
    // Darwin's null-buffer sizing call reports the system-wide process count,
    // not the selected group's size. Bound the group inventory directly.
    let mut pids = vec![0 as libc::pid_t; MAX_MACOS_PROCESS_GROUP_MEMBERS + 1];
    let buffer_bytes = pids
        .len()
        .checked_mul(std::mem::size_of::<libc::pid_t>())
        .and_then(|bytes| i32::try_from(bytes).ok())
        .ok_or(BoundProcessorError::Unreaped)?;
    let listed = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), buffer_bytes) };
    let listed = checked_macos_process_group_listing_len(listed, pids.len())?;
    pids.truncate(listed);

    let mut observed_member = false;
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let info_bytes = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
            .map_err(|_| BoundProcessorError::Unreaped)?;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                MACOS_PROC_PIDINFO_FIND_ZOMBIES_ARG,
                info.as_mut_ptr().cast(),
                info_bytes,
            )
        };
        if read == 0 {
            let Some(pid) = rustix::process::Pid::from_raw(pid) else {
                continue;
            };
            match rustix::process::test_kill_process(pid) {
                Err(rustix::io::Errno::SRCH) => continue,
                Ok(()) | Err(rustix::io::Errno::PERM) => return Ok(false),
                Err(_) => return Err(BoundProcessorError::Unreaped),
            }
        }
        if read != info_bytes {
            return Err(BoundProcessorError::Unreaped);
        }
        let info = unsafe { info.assume_init() };
        if !record_zombie_group_member(
            &mut observed_member,
            group as u32,
            info.pbi_pgid,
            info.pbi_status == libc::SZOMB,
        ) {
            return Ok(false);
        }
    }
    Ok(observed_member)
}

#[cfg(any(target_os = "macos", test))]
fn checked_macos_process_group_listing_len(
    listed: i32,
    capacity: usize,
) -> Result<usize, BoundProcessorError> {
    let listed = usize::try_from(listed).map_err(|_| BoundProcessorError::Unreaped)?;
    if listed >= capacity {
        return Err(BoundProcessorError::Unreaped);
    }
    Ok(listed)
}

#[cfg(any(target_os = "macos", test))]
fn record_zombie_group_member(
    observed_member: &mut bool,
    expected_group: u32,
    member_group: u32,
    is_zombie: bool,
) -> bool {
    if member_group != expected_group {
        return true;
    }
    *observed_member = true;
    is_zombie
}

#[cfg(target_os = "linux")]
fn linux_process_group_is_empty(group: rustix::process::Pid) -> Result<bool, BoundProcessorError> {
    match rustix::process::test_kill_process_group(group) {
        Err(rustix::io::Errno::SRCH) => Ok(true),
        Ok(()) => {
            terminate_process_group(group)?;
            if !linux_process_group_has_only_zombies(group)? {
                return Ok(false);
            }
            match rustix::process::test_kill_process_group(group) {
                Err(rustix::io::Errno::SRCH) => Ok(true),
                Ok(()) => {
                    terminate_process_group(group)?;
                    linux_process_group_has_only_zombies(group)
                }
                Err(_) => Err(BoundProcessorError::Unreaped),
            }
        }
        Err(_) => Err(BoundProcessorError::Unreaped),
    }
}

#[cfg(target_os = "linux")]
fn linux_process_group_has_only_zombies(
    group: rustix::process::Pid,
) -> Result<bool, BoundProcessorError> {
    use std::os::unix::ffi::OsStrExt as _;

    let group = group.as_raw_nonzero().get();
    let proc_dir = std::fs::File::open("/proc").map_err(|_| BoundProcessorError::Unreaped)?;
    let proc_stat = rustix::fs::fstatfs(&proc_dir).map_err(|_| BoundProcessorError::Unreaped)?;
    if proc_stat.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(BoundProcessorError::Unreaped);
    }
    let processes = std::fs::read_dir("/proc").map_err(|_| BoundProcessorError::Unreaped)?;
    let mut observed_member = false;
    for process in processes {
        let process = process.map_err(|_| BoundProcessorError::Unreaped)?;
        let file_name = process.file_name();
        let Some(pid) = parse_linux_decimal_i32(file_name.as_bytes()).filter(|pid| *pid > 0) else {
            continue;
        };
        let file = match std::fs::File::open(process.path().join("stat")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(BoundProcessorError::Unreaped),
        };
        let mut stat = Vec::with_capacity(MAX_LINUX_PROCESS_STAT_BYTES as usize + 1);
        match file
            .take(MAX_LINUX_PROCESS_STAT_BYTES + 1)
            .read_to_end(&mut stat)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(BoundProcessorError::Unreaped),
        }
        if stat.len() as u64 > MAX_LINUX_PROCESS_STAT_BYTES {
            return Err(BoundProcessorError::Unreaped);
        }
        let (state, process_group, threads) =
            parse_linux_process_stat(&stat, pid).ok_or(BoundProcessorError::Unreaped)?;
        if process_group == group {
            observed_member = true;
            if state != b'Z' || threads != 1 {
                return Ok(false);
            }
        }
    }
    Ok(observed_member)
}

#[cfg(target_os = "linux")]
fn parse_linux_process_stat(stat: &[u8], expected_pid: i32) -> Option<(u8, i32, u64)> {
    let pid_end = stat.iter().position(|byte| *byte == b' ')?;
    if stat.get(pid_end + 1) != Some(&b'(')
        || parse_linux_decimal_i32(&stat[..pid_end]) != Some(expected_pid)
    {
        return None;
    }
    let fields_start = stat.windows(2).rposition(|window| window == b") ")? + 2;
    if fields_start <= pid_end + 2 {
        return None;
    }
    let mut fields = stat[fields_start..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let state = fields.next()?;
    if state.len() != 1 {
        return None;
    }
    fields.next()?;
    let process_group = parse_linux_decimal_i32(fields.next()?)?;
    for _ in 0..14 {
        fields.next()?;
    }
    let threads = parse_linux_decimal_u64(fields.next()?)?;
    Some((state[0], process_group, threads))
}

#[cfg(target_os = "linux")]
fn parse_linux_decimal_i32(bytes: &[u8]) -> Option<i32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

#[cfg(target_os = "linux")]
fn parse_linux_decimal_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

#[cfg(unix)]
impl Drop for ProcessContainment {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process_group(self.group, rustix::process::Signal::KILL);
    }
}

struct ContainedChild {
    child: Child,
    containment: ProcessContainment,
}

async fn spawn_contained_child(
    command: &mut Command,
    runtime: Option<&ProcessorRuntime>,
) -> Result<ContainedChild, BoundProcessorError> {
    let pending = prepare_process_containment(command)?;
    if let Some(runtime) = runtime {
        runtime
            .validate_program(Path::new(command.as_std().get_program()))
            .map_err(|_| BoundProcessorError::Runtime)?;
    }
    let mut child = command.spawn().map_err(|_| BoundProcessorError::Spawn)?;
    match pending.attach(&child) {
        Ok(containment) => Ok(ContainedChild { child, containment }),
        Err(error) => {
            let _ = child.start_kill();
            match tokio::time::timeout(PROCESS_REAP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => Err(error),
                _ => Err(BoundProcessorError::Unreaped),
            }
        }
    }
}

#[cfg(windows)]
struct PendingProcessContainment {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
struct ProcessContainment {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
fn prepare_process_containment(
    command: &mut Command,
) -> Result<PendingProcessContainment, BoundProcessorError> {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(BoundProcessorError::Containment);
    }
    let job = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(BoundProcessorError::Containment);
    }
    command.creation_flags(CREATE_SUSPENDED);
    Ok(PendingProcessContainment { job })
}

#[cfg(windows)]
impl PendingProcessContainment {
    fn attach(self, child: &Child) -> Result<ProcessContainment, BoundProcessorError> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let process = child.raw_handle().ok_or(BoundProcessorError::Containment)?;
        let job = self.job.as_raw_handle();
        if unsafe { AssignProcessToJobObject(job.cast(), process.cast()) } == 0 {
            return Err(BoundProcessorError::Containment);
        }
        if unsafe { ntapi::ntpsapi::NtResumeProcess(process.cast()) } < 0 {
            return Err(BoundProcessorError::Containment);
        }
        Ok(ProcessContainment { job: self.job })
    }
}

#[cfg(windows)]
impl ProcessContainment {
    fn terminate(&self) -> Result<(), BoundProcessorError> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        if unsafe { TerminateJobObject(self.job.as_raw_handle().cast(), 1) } == 0 {
            return Err(BoundProcessorError::Unreaped);
        }
        Ok(())
    }

    fn is_empty(&self) -> Result<bool, BoundProcessorError> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };

        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let queried = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle().cast(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(BoundProcessorError::Unreaped);
        }
        Ok(accounting.ActiveProcesses == 0)
    }
}

#[cfg(windows)]
impl Drop for ProcessContainment {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[derive(Debug, Error)]
pub(crate) enum BoundProcessorError {
    #[error("processor authority is invalid")]
    Authority,
    #[error("processor source acquisition failed")]
    Source,
    #[error("processor staging failed")]
    Stage,
    #[error("managed processor runtime is unavailable")]
    Runtime,
    #[error("processor entry point is invalid")]
    Manifest,
    #[error("processor could not be started")]
    Spawn,
    #[error("processor containment could not be established")]
    Containment,
    #[error("processor exceeded its execution time limit")]
    Timeout,
    #[error("processor output exceeded its capture limit")]
    OutputLimit,
    #[error("processor exited unsuccessfully")]
    Unsuccessful,
    #[error("processor execution was cancelled")]
    Cancelled,
    #[error("processor descendants could not be proven stopped")]
    Unreaped,
    #[error("processor workspace cleanup failed")]
    Cleanup,
    #[error("processor owner task stopped unexpectedly")]
    OwnerStopped,
}

pub(crate) struct VerifiedProcessorOutputs {
    entries: BTreeMap<PortableRelativePath, VerifiedProcessorOutput>,
}

pub(crate) struct VerifiedProcessorOutput {
    bytes: Vec<u8>,
    size: u64,
    sha1: [u8; 20],
    expectation: BoundProcessorOutputExpectation,
}

struct VerifiedStepOutput {
    bytes: Option<Vec<u8>>,
    size: u64,
    sha1: [u8; 20],
    terminal: bool,
    expectation: BoundProcessorOutputExpectation,
}

pub(crate) struct BoundProcessorExecutionResult {
    pub(crate) sources: AuthenticatedProcessorSources,
    pub(crate) continuation: BoundForgeInstallerContinuation,
    pub(crate) outputs: VerifiedProcessorOutputs,
    pub(crate) reconstruction_library_sources:
        crate::download::library_source::RetainedLibrarySourceSet,
}

pub(crate) struct AuthenticatedProcessorSources {
    base_version: VersionJson,
    client: ProcessorClientSource,
    runtime_source: Option<RuntimeSourceReceipt>,
    #[cfg(test)]
    mappings_transport: Option<crate::download::TestProcessorMappingsTransport>,
}

enum ProcessorClientSource {
    Installed(Vec<u8>),
    Reconstructed(AuthenticatedSelectedArtifactSource),
}

impl AuthenticatedProcessorSources {
    pub(crate) fn from_installed(
        base_version: VersionJson,
        client_bytes: Vec<u8>,
        runtime_source: RuntimeSourceReceipt,
    ) -> Result<Self, BoundProcessorError> {
        validate_client_source(&base_version, &client_bytes)?;
        Ok(Self {
            base_version,
            client: ProcessorClientSource::Installed(client_bytes),
            runtime_source: Some(runtime_source),
            #[cfg(test)]
            mappings_transport: None,
        })
    }

    pub(crate) fn from_reconstructed(
        base_version: VersionJson,
        client_source: AuthenticatedSelectedArtifactSource,
        runtime_source: RuntimeSourceReceipt,
    ) -> Result<Self, BoundProcessorError> {
        let client = base_version
            .downloads
            .client
            .as_ref()
            .ok_or(BoundProcessorError::Authority)?;
        let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
        if client_source.provider_url() != client.url || client_source.expected() != &expected {
            return Err(BoundProcessorError::Authority);
        }
        validate_client_source(&base_version, client_source.bytes())?;
        Ok(Self {
            base_version,
            client: ProcessorClientSource::Reconstructed(client_source),
            runtime_source: Some(runtime_source),
            #[cfg(test)]
            mappings_transport: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_mappings_transport(
        mut self,
        transport: Option<crate::download::TestProcessorMappingsTransport>,
    ) -> Self {
        self.mappings_transport = transport;
        self
    }

    fn client_bytes(&self) -> &[u8] {
        match &self.client {
            ProcessorClientSource::Installed(bytes) => bytes,
            ProcessorClientSource::Reconstructed(source) => source.bytes(),
        }
    }

    pub(crate) fn into_installed_parts(
        mut self,
    ) -> Result<(Vec<u8>, RuntimeSourceReceipt), BoundProcessorError> {
        let ProcessorClientSource::Installed(client) = self.client else {
            return Err(BoundProcessorError::Authority);
        };
        Ok((
            client,
            self.runtime_source
                .take()
                .ok_or(BoundProcessorError::Runtime)?,
        ))
    }

    pub(crate) fn into_reconstructed_parts(
        mut self,
    ) -> Result<(AuthenticatedSelectedArtifactSource, RuntimeSourceReceipt), BoundProcessorError>
    {
        let ProcessorClientSource::Reconstructed(client) = self.client else {
            return Err(BoundProcessorError::Authority);
        };
        Ok((
            client,
            self.runtime_source
                .take()
                .ok_or(BoundProcessorError::Runtime)?,
        ))
    }
}

fn validate_client_source(
    base_version: &VersionJson,
    bytes: &[u8],
) -> Result<(), BoundProcessorError> {
    let client = base_version
        .downloads
        .client
        .as_ref()
        .ok_or(BoundProcessorError::Authority)?;
    if u64::try_from(client.size).ok() != Some(bytes.len() as u64)
        || !client
            .sha1
            .eq_ignore_ascii_case(&format!("{:x}", Sha1::digest(bytes)))
    {
        return Err(BoundProcessorError::Authority);
    }
    Ok(())
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

pub(crate) fn spawn_bound_processor_execution(
    execution: BoundForgeProcessorExecution,
    target_version_id: String,
    minecraft_version: String,
    sources: AuthenticatedProcessorSources,
) -> BoundProcessorExecutionHandle {
    let (continuation, plan) = execution.into_parts();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let workspace = super::workspace::cleanup::prepare_ephemeral_processor_workspace(
            &target_version_id,
            &minecraft_version,
        )
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "workspace_prepare", %error, "Forge processor execution failed");
        })?;
        run_owned_execution(
            continuation,
            plan,
            workspace,
            sources,
            crate::download::library_source::RetainedLibrarySourceSet::new(),
            cancel_rx,
            progress_tx,
        )
        .await
    });
    BoundProcessorExecutionHandle {
        cancel: Some(cancel_tx),
        progress: progress_rx,
        task: Some(task),
    }
}

pub(crate) fn spawn_reconstruction_processor_execution(
    pending: super::forge_installer::PendingForgeReconstructionSources,
    target_version_id: String,
    minecraft_version: String,
    sources: AuthenticatedProcessorSources,
    context: crate::download::ManagedReconstructionContext,
) -> BoundProcessorExecutionHandle {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let workspace = super::workspace::cleanup::prepare_ephemeral_processor_workspace(
            &target_version_id,
            &minecraft_version,
        )
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "reconstruction_workspace_prepare", %error, "Forge processor execution failed");
        })?;
        let execution = if let Err(error) = check_cancel(&mut cancel_rx) {
            Err(error)
        } else {
            crate::download::reconstruct_installer_processor_sources(
                pending,
                workspace.workspace(),
                &context,
            )
            .await
            .map_err(|_| BoundProcessorError::Source)
            .and_then(|execution| {
                check_cancel(&mut cancel_rx)?;
                Ok(execution)
            })
        };
        let (execution, reconstruction_library_sources) = match execution {
            Ok((BoundForgeInstallExecution::Run(execution), library_sources)) => {
                (*execution, library_sources)
            }
            Ok(_) => {
                return match workspace.cleanup() {
                    Ok(()) => Err(BoundProcessorError::Authority),
                    Err(_) => Err(BoundProcessorError::Cleanup),
                }
                .inspect_err(|error| {
                    tracing::warn!(stage = "reconstruction_execution_admission", %error, "Forge processor execution failed");
                });
            }
            Err(error) => {
                tracing::warn!(stage = "reconstruction_sources", %error, "Forge processor execution failed");
                return match workspace.cleanup() {
                    Ok(()) => Err(error),
                    Err(_) => {
                        tracing::warn!(stage = "reconstruction_source_settlement", error = %BoundProcessorError::Cleanup, "Forge processor execution failed");
                        Err(BoundProcessorError::Cleanup)
                    }
                };
            }
        };
        let (continuation, plan) = execution.into_parts();
        run_owned_execution(
            continuation,
            plan,
            workspace,
            sources,
            reconstruction_library_sources,
            cancel_rx,
            progress_tx,
        )
        .await
    });
    BoundProcessorExecutionHandle {
        cancel: Some(cancel_tx),
        progress: progress_rx,
        task: Some(task),
    }
}

async fn run_owned_execution(
    continuation: BoundForgeInstallerContinuation,
    plan: BoundProcessorPlan,
    workspace_owner: ProcessorWorkspaceOwner,
    mut sources: AuthenticatedProcessorSources,
    reconstruction_library_sources: crate::download::library_source::RetainedLibrarySourceSet,
    mut cancel: oneshot::Receiver<()>,
    progress: mpsc::UnboundedSender<BoundProcessorProgress>,
) -> Result<BoundProcessorExecutionResult, BoundProcessorError> {
    if !continuation.matches_execution_identity(
        workspace_owner.target_version_id(),
        &sources.base_version.id,
    ) {
        return match workspace_owner.cleanup() {
            Ok(()) => Err(BoundProcessorError::Authority),
            Err(_) => Err(BoundProcessorError::Cleanup),
        }
        .inspect_err(|error| {
            tracing::warn!(stage = "execution_identity", %error, "Forge processor execution failed");
        });
    }
    let execution = execute_in_workspace(
        &continuation,
        &plan,
        workspace_owner.workspace(),
        &mut sources,
        &workspace_owner,
        &mut cancel,
        &progress,
    )
    .await
    .inspect_err(|error| {
        tracing::warn!(stage = "workspace_execution", %error, "Forge processor execution failed");
    });
    if matches!(execution, Err(BoundProcessorError::Unreaped)) {
        workspace_owner.quarantine();
        return execution.map(|outputs| BoundProcessorExecutionResult {
            sources,
            continuation,
            outputs,
            reconstruction_library_sources,
        });
    }
    workspace_owner
        .cleanup()
        .map_err(|_| BoundProcessorError::Cleanup)
        .inspect_err(|error| {
            tracing::warn!(stage = "workspace_cleanup", %error, "Forge processor execution failed");
        })?;
    execution.map(|outputs| BoundProcessorExecutionResult {
        sources,
        continuation,
        outputs,
        reconstruction_library_sources,
    })
}

async fn execute_in_workspace(
    continuation: &BoundForgeInstallerContinuation,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    sources: &mut AuthenticatedProcessorSources,
    workspace_owner: &ProcessorWorkspaceOwner,
    cancel: &mut oneshot::Receiver<()>,
    progress: &mpsc::UnboundedSender<BoundProcessorProgress>,
) -> Result<VerifiedProcessorOutputs, BoundProcessorError> {
    check_cancel(cancel)?;
    let mut authority = stage_inputs(continuation, plan, workspace, sources, cancel)
        .await
        .inspect_err(|error| {
            tracing::warn!(stage = "stage_inputs", %error, "Forge processor execution failed");
        })?;
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)?;
    workspace
        .revalidate()
        .map_err(|_| BoundProcessorError::Stage)?;
    let initial_stage = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;

    let runtime_source = sources
        .runtime_source
        .take()
        .ok_or(BoundProcessorError::Runtime)?;
    let runtime = workspace_owner
        .materialize_runtime(&sources.base_version.java_version, runtime_source)
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
            &sources.base_version,
            #[cfg(test)]
            sources.mappings_transport.as_ref(),
            &mut authority,
            cancel,
        )
        .await
        .inspect_err(|error| {
            tracing::warn!(stage = "processor_step", step = index + 1, %error, "Forge processor execution failed");
        })?;
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
                        bytes,
                        size: output.size,
                        sha1: output.sha1,
                        expectation: output.expectation,
                    },
                );
            }
        }
    }
    final_rescan(
        workspace,
        plan,
        &sources.base_version.id,
        &authority,
        &initial_stage,
    )
    .inspect_err(|error| {
        tracing::warn!(stage = "final_rescan", %error, "Forge processor execution failed");
    })?;
    sources.runtime_source = Some(runtime.into_source_receipt());
    Ok(VerifiedProcessorOutputs { entries: verified })
}

struct AuthenticatedBytes {
    size: u64,
    sha1: [u8; 20],
}

struct StagedAuthority {
    libraries: BTreeMap<PortableRelativePath, AuthenticatedBytes>,
    version: AuthenticatedBytes,
    processor_data: BTreeMap<PortableRelativePath, AuthenticatedBytes>,
    installer: Option<AuthenticatedBytes>,
}

async fn stage_inputs(
    continuation: &BoundForgeInstallerContinuation,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    sources: &AuthenticatedProcessorSources,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<StagedAuthority, BoundProcessorError> {
    let mut libraries = BTreeMap::new();
    for (index, (path, contract)) in plan.input_artifacts.iter().enumerate() {
        check_cancel(cancel)?;
        let authenticated = match contract.source {
            super::forge_installer::BoundProcessorInputSource::Download => {
                match continuation
                    .network_input_source(path)
                    .map_err(|_| BoundProcessorError::Authority)
                    .inspect_err(|error| {
                        tracing::warn!(stage = "input_source_admission", input = index + 1, %error, "Forge processor execution failed");
                    })?
                {
                    super::forge_installer::BoundProcessorNetworkInput::Retained(source) => {
                        let (reader, size, sha1) = source.into_parts();
                        if sha1 != contract.sha1
                            || contract.size.is_some_and(|expected| expected != size)
                        {
                            return Err(BoundProcessorError::Authority);
                        }
                        workspace
                            .import_library_authenticated(path, reader, size, sha1)
                            .await
                            .map_err(|_| BoundProcessorError::Stage)
                            .inspect_err(|error| {
                                tracing::warn!(stage = "input_library_import", input = index + 1, %error, "Forge processor execution failed");
                            })?;
                        AuthenticatedBytes { size, sha1 }
                    }
                    super::forge_installer::BoundProcessorNetworkInput::ReconstructionWorkspace => {
                        let bytes = workspace
                            .read_library_authenticated(path, contract.size, &contract.sha1)
                            .map_err(|_| BoundProcessorError::Authority)
                            .inspect_err(|error| {
                                tracing::warn!(stage = "input_library_reconstruction", input = index + 1, %error, "Forge processor execution failed");
                            })?;
                        AuthenticatedBytes {
                            size: bytes.len() as u64,
                            sha1: contract.sha1,
                        }
                    }
                }
            }
            super::forge_installer::BoundProcessorInputSource::Embedded => {
                let embedded = continuation
                    .embedded_maven_artifact(path)
                    .ok_or(BoundProcessorError::Authority)?;
                let bytes = authenticate_bytes(embedded.bytes(), contract.size, &contract.sha1)?;
                workspace
                    .write_library_exact(path, &bytes)
                    .await
                    .map_err(|_| BoundProcessorError::Stage)
                    .inspect_err(|error| {
                        tracing::warn!(stage = "input_embedded_stage", input = index + 1, %error, "Forge processor execution failed");
                    })?;
                AuthenticatedBytes {
                    size: bytes.len() as u64,
                    sha1: contract.sha1,
                }
            }
        };
        libraries.insert(path.clone(), authenticated);
    }

    let client_name = format!("{}.jar", sources.base_version.id);
    let client_bytes = sources.client_bytes();
    validate_client_source(&sources.base_version, client_bytes)?;
    let staged_client =
        PortableRelativePath::new(&client_name).map_err(|_| BoundProcessorError::Authority)?;
    workspace
        .write_version_exact(&staged_client, client_bytes)
        .await
        .map_err(|_| BoundProcessorError::Stage)?;

    let version = AuthenticatedBytes {
        size: client_bytes.len() as u64,
        sha1: Sha1::digest(client_bytes).into(),
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
    Ok(StagedAuthority {
        libraries,
        version,
        processor_data,
        installer,
    })
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
    base_version: &VersionJson,
    #[cfg(test)] mappings_transport: Option<&crate::download::TestProcessorMappingsTransport>,
    authority: &mut StagedAuthority,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<BTreeMap<PortableRelativePath, VerifiedStepOutput>, BoundProcessorError> {
    check_cancel(cancel)?;
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_scratch_prepare", %error, "Forge processor execution failed");
        })?;
    for output in &step.outputs {
        workspace
            .ensure_library_parent(&output.artifact.relative_path)
            .map_err(|_| BoundProcessorError::Stage)
            .inspect_err(|error| {
                tracing::warn!(stage = "step_output_parent", %error, "Forge processor execution failed");
            })?;
    }
    let before_root = workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_root_before", %error, "Forge processor execution failed");
        })?;
    let before_stage = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_stage_before", %error, "Forge processor execution failed");
        })?;
    if !matches!(
        &step.action,
        BoundProcessorAction::Java | BoundProcessorAction::SplitJar
    ) {
        reauthenticate_step_dependencies(step, plan, workspace, authority, &base_version.id)?;
    }
    match &step.action {
        BoundProcessorAction::Java | BoundProcessorAction::SplitJar => {
            for output in &step.outputs {
                workspace
                    .ensure_temp_parent(&output.artifact.relative_path)
                    .map_err(|_| BoundProcessorError::Stage)?;
            }
            run_java_step(
                step,
                plan,
                workspace,
                runtime,
                &base_version.id,
                authority,
                cancel,
            )
            .await?;
            promote_java_outputs(step, workspace, &before_root, cancel).await?;
        }
        BoundProcessorAction::ExtractMcpMappings { input } => {
            let archive = staged_artifact_bytes(workspace, input, &authority.libraries)?;
            let bytes = extract_mcp_mappings(&archive, &base_version.id)?;
            write_native_output(step, workspace, &bytes, cancel).await?;
        }
        BoundProcessorAction::DownloadMojmaps => {
            let acquire = async {
                #[cfg(test)]
                if let Some(transport) = mappings_transport {
                    return crate::download::acquire_test_processor_mappings(
                        base_version,
                        transport,
                    )
                    .await;
                }
                crate::download::acquire_processor_mappings(base_version).await
            };
            let source = tokio::select! {
                result = acquire => {
                    result.map_err(|_| BoundProcessorError::Source)?
                }
                _ = &mut *cancel => return Err(BoundProcessorError::Cancelled),
            };
            validate_mapping_source(base_version, &source)?;
            write_native_output(step, workspace, source.bytes(), cancel).await?;
        }
    }
    check_cancel(cancel)?;
    settle_step_outputs(step, workspace, &before_root, &before_stage)
}

fn settle_step_outputs(
    step: &BoundProcessorStep,
    workspace: &ProcessorWorkspace,
    before_root: &ManagedTreeSnapshot,
    before_stage: &ManagedTreeSnapshot,
) -> Result<BTreeMap<PortableRelativePath, VerifiedStepOutput>, BoundProcessorError> {
    workspace
        .revalidate()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_settle_revalidate", %error, "Forge processor execution failed");
        })?;
    let after_stage = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_stage_after", %error, "Forge processor execution failed");
        })?;
    let after_root = workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_root_after", %error, "Forge processor execution failed");
        })?;
    verify_step_diff(step, before_root, &after_root, before_stage, &after_stage)?;

    let mut outputs = BTreeMap::new();
    for output in &step.outputs {
        let fact = after_root
            .files()
            .get(&library_root_path(&output.artifact.relative_path)?)
            .ok_or(BoundProcessorError::Stage)?;
        if fact.size() == 0
            || matches!(&output.expectation, BoundProcessorOutputExpectation::ProviderSha1(sha1) if fact.sha1() != sha1)
        {
            return Err(BoundProcessorError::Authority);
        }
        let bytes = workspace
            .read_library_authenticated(
                &output.artifact.relative_path,
                Some(fact.size()),
                fact.sha1(),
            )
            .map_err(|_| BoundProcessorError::Authority)?;
        if matches!(
            &output.expectation,
            BoundProcessorOutputExpectation::Derived(_)
        ) && output.artifact.relative_path.as_str().ends_with(".txt")
        {
            validate_mapping_text(&bytes)?;
        }
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
                sha1: *fact.sha1(),
                bytes: terminal.then_some(bytes),
                terminal,
                expectation: output.expectation.clone(),
            },
        );
    }
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_scratch_cleanup", %error, "Forge processor execution failed");
        })?;
    let settled = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_stage_settled", %error, "Forge processor execution failed");
        })?;
    verify_clean_stage_diff(step, before_stage, &settled)?;
    Ok(outputs)
}

async fn promote_java_outputs(
    step: &BoundProcessorStep,
    workspace: &ProcessorWorkspace,
    before_root: &ManagedTreeSnapshot,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), BoundProcessorError> {
    check_cancel(cancel)?;
    workspace
        .validate_live_bounds()
        .map_err(|_| BoundProcessorError::Stage)?;
    let after_java = workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)?;
    if &after_java != before_root {
        return Err(BoundProcessorError::Stage);
    }
    let settled = workspace
        .snapshot_stage()
        .map_err(|_| BoundProcessorError::Stage)?;
    let mut remaining = MAX_JAVA_RECONSTRUCTION_WORK;
    reserve_java_work(
        &mut remaining,
        step.outputs.len() as u64 * std::mem::size_of::<(&PortableRelativePath, Vec<u8>)>() as u64,
    )?;
    let mut verified = Vec::with_capacity(step.outputs.len());
    for output in &step.outputs {
        check_cancel(cancel)?;
        validate_fresh_output_target(step, &output.artifact, before_root)?;
        let temporary_path = PortableRelativePath::new_exact(&format!(
            "tmp/{}",
            output.artifact.relative_path.as_str()
        ))
        .map_err(|_| BoundProcessorError::Authority)?;
        let fact = settled
            .files()
            .get(&temporary_path)
            .ok_or(BoundProcessorError::Authority)?;
        let expected_size = match output.role {
            BoundProcessorOutputRole::Intermediate => None,
            BoundProcessorOutputRole::Terminal { expected_size } => expected_size,
        };
        if fact.size() == 0 {
            return Err(BoundProcessorError::Authority);
        }
        reserve_java_work(&mut remaining, fact.size() * 3 + 1)?;
        // Observed hashes bind the scratch reread, not a provider expectation.
        let mut bytes = workspace
            .read_temp_authenticated(
                &output.artifact.relative_path,
                Some(fact.size()),
                fact.sha1(),
            )
            .map_err(|_| BoundProcessorError::Authority)?;
        if matches!(&output.expectation, BoundProcessorOutputExpectation::ProviderSha1(sha1) if fact.sha1() != sha1)
        {
            bytes = recompress_java_archive(&bytes, &mut remaining, cancel).await?;
        }
        reserve_java_work(&mut remaining, bytes.len() as u64 * 3 + 1)?;
        if expected_size.is_some_and(|size| size != bytes.len() as u64)
            || matches!(&output.expectation, BoundProcessorOutputExpectation::ProviderSha1(sha1) if <[u8; 20]>::from(Sha1::digest(&bytes)) != *sha1)
        {
            return Err(BoundProcessorError::Authority);
        }
        verified.push((&output.artifact.relative_path, bytes));
    }
    check_cancel(cancel)?;
    if &workspace
        .snapshot_root()
        .map_err(|_| BoundProcessorError::Stage)?
        != before_root
    {
        return Err(BoundProcessorError::Stage);
    }
    workspace
        .clear_scratch()
        .map_err(|_| BoundProcessorError::Stage)?;
    for (path, bytes) in verified {
        check_cancel(cancel)?;
        workspace
            .write_library_exact(path, &bytes)
            .await
            .map_err(|_| BoundProcessorError::Stage)?;
    }
    Ok(())
}

const MAX_JAVA_ARCHIVE_BYTES: usize = 128 << 20;
const MAX_JAVA_ARCHIVE_ENTRIES: usize = 32_768;
const MAX_JAVA_ENTRY_BYTES: u64 = 64 << 20;
const MAX_JAVA_RECONSTRUCTION_WORK: u64 = 512 << 20;

fn reserve_java_work(remaining: &mut u64, bytes: u64) -> Result<(), BoundProcessorError> {
    *remaining = remaining
        .checked_sub(bytes)
        .ok_or(BoundProcessorError::Authority)?;
    Ok(())
}

fn zip_part(bytes: &[u8], start: usize, length: usize) -> Result<&[u8], BoundProcessorError> {
    bytes
        .get(
            start
                ..start
                    .checked_add(length)
                    .ok_or(BoundProcessorError::Authority)?,
        )
        .ok_or(BoundProcessorError::Authority)
}

fn zip_u16(bytes: &[u8], start: usize) -> Result<usize, BoundProcessorError> {
    Ok(u16::from_le_bytes(zip_part(bytes, start, 2)?.try_into().unwrap()) as usize)
}

fn zip_u32(bytes: &[u8], start: usize) -> Result<usize, BoundProcessorError> {
    Ok(u32::from_le_bytes(zip_part(bytes, start, 4)?.try_into().unwrap()) as usize)
}

async fn recompress_java_archive(
    bytes: &[u8],
    remaining: &mut u64,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<Vec<u8>, BoundProcessorError> {
    check_cancel(cancel)?;
    let end = bytes
        .len()
        .checked_sub(22)
        .ok_or(BoundProcessorError::Authority)?;
    let tail = zip_part(bytes, end, 22)?;
    let entries = zip_u16(tail, 10)?;
    let central_size = zip_u32(tail, 12)?;
    let central_start = zip_u32(tail, 16)?;
    if bytes.len() > MAX_JAVA_ARCHIVE_BYTES
        || zip_u32(tail, 0)? != 0x06054b50
        || zip_u32(tail, 4)? != 0
        || zip_u16(tail, 8)? != entries
        || zip_u16(tail, 20)? != 0
        || entries == 0
        || entries > MAX_JAVA_ARCHIVE_ENTRIES
        || central_size > 16 << 20
        || central_start.checked_add(central_size) != Some(end)
    {
        return Err(BoundProcessorError::Authority);
    }
    reserve_java_work(remaining, bytes.len() as u64 + entries as u64 * 512)?;
    let mut central = central_start;
    let mut names = 0_usize;
    let mut expanded = 0_u64;
    let mut compressed_capacity = 0_usize;
    for _ in 0..entries {
        let header = zip_part(bytes, central, 46)?;
        let size = zip_u32(header, 24)? as u64;
        let name = zip_u16(header, 28)?;
        names = names
            .checked_add(name)
            .ok_or(BoundProcessorError::Authority)?;
        expanded = expanded
            .checked_add(size)
            .ok_or(BoundProcessorError::Authority)?;
        // deflateBound's conservative raw-stream bound, including its wrapper allowance.
        let capacity = size + (size >> 12) + (size >> 14) + (size >> 25) + 13;
        compressed_capacity = compressed_capacity
            .checked_add(capacity as usize)
            .ok_or(BoundProcessorError::Authority)?;
        if zip_u32(header, 0)? != 0x02014b50
            || zip_u16(header, 8)? != 0x808
            || zip_u16(header, 10)? != 8
            || zip_u16(header, 34)? != 0
            || name == 0
            || name > 1024
            || names > 8 << 20
            || std::str::from_utf8(zip_part(bytes, central + 46, name)?).is_err()
            || size > MAX_JAVA_ENTRY_BYTES
            || expanded > MAX_JAVA_ARCHIVE_BYTES as u64
        {
            return Err(BoundProcessorError::Authority);
        }
        central = central
            .checked_add(46 + name + zip_u16(header, 30)? + zip_u16(header, 32)?)
            .ok_or(BoundProcessorError::Authority)?;
        if central > end {
            return Err(BoundProcessorError::Authority);
        }
    }
    if central != end {
        return Err(BoundProcessorError::Authority);
    }
    let capacity = bytes
        .len()
        .checked_add(compressed_capacity)
        .ok_or(BoundProcessorError::Authority)?;
    reserve_java_work(
        remaining,
        names as u64 * 3
            + expanded * 2
            + compressed_capacity as u64 * 2
            + capacity as u64
            + (512 << 10),
    )?;
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| BoundProcessorError::Authority)?;
    if archive.len() != entries {
        return Err(BoundProcessorError::Authority);
    }
    let mut rebuilt = Vec::with_capacity(capacity);
    let mut revisions = Vec::with_capacity(entries);
    let mut previous_end = 0;
    for index in 0..entries {
        check_cancel(cancel)?;
        let (payload, header_start, data_start, compressed_size, crc) = {
            let mut file = archive
                .by_index(index)
                .map_err(|_| BoundProcessorError::Authority)?;
            let size = usize::try_from(file.size()).map_err(|_| BoundProcessorError::Authority)?;
            if file.encrypted() || file.size() > MAX_JAVA_ENTRY_BYTES {
                return Err(BoundProcessorError::Authority);
            }
            let mut payload = vec![0; size];
            file.read_exact(&mut payload)
                .map_err(|_| BoundProcessorError::Authority)?;
            if file
                .read(&mut [0])
                .map_err(|_| BoundProcessorError::Authority)?
                != 0
            {
                return Err(BoundProcessorError::Authority);
            }
            (
                payload,
                file.header_start() as usize,
                file.data_start() as usize,
                file.compressed_size() as usize,
                file.crc32(),
            )
        };
        let header = zip_part(bytes, header_start, 30)?;
        let descriptor_start = data_start
            .checked_add(compressed_size)
            .ok_or(BoundProcessorError::Authority)?;
        let descriptor = zip_part(bytes, descriptor_start, 16)?;
        if header_start != previous_end
            || zip_u32(header, 0)? != 0x04034b50
            || zip_u16(header, 6)? != 0x808
            || zip_u16(header, 8)? != 8
            || zip_u32(header, 14)? != 0
            || zip_u32(header, 18)? != 0
            || zip_u32(header, 22)? != 0
            || header_start.checked_add(30 + zip_u16(header, 26)? + zip_u16(header, 28)?)
                != Some(data_start)
            || zip_u32(descriptor, 0)? != 0x08074b50
            || zip_u32(descriptor, 4)? != crc as usize
            || zip_u32(descriptor, 8)? != compressed_size
            || zip_u32(descriptor, 12)? != payload.len()
        {
            return Err(BoundProcessorError::Authority);
        }
        let compressed = classic_deflate(&payload)?;
        let offset = rebuilt.len();
        rebuilt.extend_from_slice(zip_part(bytes, header_start, data_start - header_start)?);
        rebuilt.extend_from_slice(&compressed);
        rebuilt.extend_from_slice(&descriptor[..8]);
        rebuilt.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        rebuilt.extend_from_slice(&descriptor[12..]);
        previous_end = descriptor_start + 16;
        revisions.push((compressed.len() as u32, offset as u32));
        if rebuilt.len() > MAX_JAVA_ARCHIVE_BYTES {
            return Err(BoundProcessorError::Authority);
        }
        if index % 128 == 127 {
            tokio::task::yield_now().await;
        }
    }
    if previous_end != central_start {
        return Err(BoundProcessorError::Authority);
    }
    let new_central_start = rebuilt.len();
    central = central_start;
    for (size, offset) in revisions {
        let header = zip_part(bytes, central, 46)?;
        let length = 46 + zip_u16(header, 28)? + zip_u16(header, 30)? + zip_u16(header, 32)?;
        let start = rebuilt.len();
        rebuilt.extend_from_slice(zip_part(bytes, central, length)?);
        rebuilt[start + 20..start + 24].copy_from_slice(&size.to_le_bytes());
        rebuilt[start + 42..start + 46].copy_from_slice(&offset.to_le_bytes());
        central += length;
    }
    let new_central_size = rebuilt.len() - new_central_start;
    rebuilt.extend_from_slice(&tail[..12]);
    rebuilt.extend_from_slice(&(new_central_size as u32).to_le_bytes());
    rebuilt.extend_from_slice(&(new_central_start as u32).to_le_bytes());
    rebuilt.extend_from_slice(&tail[20..]);
    if rebuilt.len() > MAX_JAVA_ARCHIVE_BYTES || rebuilt.len() > capacity {
        return Err(BoundProcessorError::Authority);
    }
    Ok(rebuilt)
}

fn classic_deflate(bytes: &[u8]) -> Result<Vec<u8>, BoundProcessorError> {
    let mut stream = Box::<libz_sys::z_stream>::new_uninit();
    // C initializes the null callbacks before Rust observes the stream; its address stays fixed.
    let initialized = unsafe {
        stream.as_mut_ptr().write_bytes(0, 1);
        libz_sys::deflateInit2_(
            stream.as_mut_ptr(),
            libz_sys::Z_DEFAULT_COMPRESSION,
            libz_sys::Z_DEFLATED,
            -15,
            8,
            libz_sys::Z_DEFAULT_STRATEGY,
            libz_sys::zlibVersion(),
            std::mem::size_of::<libz_sys::z_stream>() as i32,
        )
    };
    if initialized != libz_sys::Z_OK {
        return Err(BoundProcessorError::Authority);
    }
    // Successful deflateInit2 fills every field, including valid allocator function pointers.
    let mut stream = unsafe { stream.assume_init() };
    let result = (|| {
        // The initialized stream is owned here and deflateBound does not retain the input.
        let capacity =
            unsafe { libz_sys::deflateBound(&mut *stream, bytes.len() as libz_sys::uLong) };
        let capacity = usize::try_from(capacity).map_err(|_| BoundProcessorError::Authority)?;
        let reserved =
            bytes.len() + (bytes.len() >> 12) + (bytes.len() >> 14) + (bytes.len() >> 25) + 13;
        if capacity > reserved || bytes.len() > MAX_JAVA_ENTRY_BYTES as usize {
            return Err(BoundProcessorError::Authority);
        }
        let mut compressed = vec![0; capacity];
        stream.next_in = bytes.as_ptr().cast_mut();
        stream.avail_in = bytes.len() as libz_sys::uInt;
        stream.next_out = compressed.as_mut_ptr();
        stream.avail_out = compressed.len() as libz_sys::uInt;
        // Both buffers stay live and bounded until deflate finishes using their pointers.
        let status = unsafe { libz_sys::deflate(&mut *stream, libz_sys::Z_FINISH) };
        if status != libz_sys::Z_STREAM_END
            || stream.avail_in != 0
            || stream.total_in as usize != bytes.len()
            || stream.total_out as usize > compressed.len()
        {
            return Err(BoundProcessorError::Authority);
        }
        compressed.truncate(stream.total_out as usize);
        Ok(compressed)
    })();
    // Every initialized stream is released, including failed compression attempts.
    let ended = unsafe { libz_sys::deflateEnd(&mut *stream) };
    if ended != libz_sys::Z_OK {
        return Err(BoundProcessorError::Authority);
    }
    result
}

async fn run_java_step(
    step: &BoundProcessorStep,
    plan: &BoundProcessorPlan,
    workspace: &ProcessorWorkspace,
    runtime: &ProcessorRuntime,
    minecraft_version: &str,
    authority: &StagedAuthority,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), BoundProcessorError> {
    let jar_bytes = staged_artifact_bytes(workspace, &step.jar, &authority.libraries)?;
    let main_class = processor_main_class(&jar_bytes)?;
    let classpath = render_classpath(step, workspace)?;
    let arguments = step
        .args
        .iter()
        .map(|argument| render_argument(argument, plan, workspace, minecraft_version))
        .collect::<Result<Vec<_>, _>>()?;
    let bootstrap_environment = processor_bootstrap_environment()?;
    reauthenticate_step_dependencies(step, plan, workspace, authority, minecraft_version)
        .inspect_err(|error| {
            tracing::warn!(stage = "step_java_dependencies", %error, "Forge processor execution failed");
        })?;
    let mut command = Command::new(runtime.cli_executable_path());
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
    check_cancel(cancel)?;
    let mut child = spawn_contained_child(&mut command, Some(runtime)).await?;
    let stdout = match child.child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_and_reap(&mut child).await?;
            return Err(BoundProcessorError::Spawn);
        }
    };
    let stderr = match child.child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_and_reap(&mut child).await?;
            return Err(BoundProcessorError::Spawn);
        }
    };
    let total = Arc::new(AtomicUsize::new(0));
    let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(drain_pipe(stdout, total.clone(), pipe_tx.clone()));
    let stderr_task = tokio::spawn(drain_pipe(stderr, total, pipe_tx));
    let process_result = wait_for_contained_child(
        &mut child,
        cancel,
        &mut pipe_rx,
        Some(workspace),
        PROCESSOR_TIMEOUT,
        PIPE_DRAIN_TIMEOUT,
    )
    .await;
    stdout_task.abort();
    stderr_task.abort();
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    process_result?;
    runtime
        .validate_program(runtime.cli_executable_path())
        .map_err(|_| BoundProcessorError::Runtime)?;
    Ok(())
}

async fn write_native_output(
    step: &BoundProcessorStep,
    workspace: &ProcessorWorkspace,
    bytes: &[u8],
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), BoundProcessorError> {
    check_cancel(cancel)?;
    let [output] = step.outputs.as_slice() else {
        return Err(BoundProcessorError::Authority);
    };
    validate_mapping_text(bytes)?;
    validate_fresh_output_target(
        step,
        &output.artifact,
        &workspace
            .snapshot_root()
            .map_err(|_| BoundProcessorError::Stage)?,
    )?;
    workspace
        .write_library_exact(&output.artifact.relative_path, bytes)
        .await
        .map_err(|_| BoundProcessorError::Stage)?;
    check_cancel(cancel)
}

fn validate_mapping_source(
    base_version: &VersionJson,
    source: &AuthenticatedSelectedArtifactSource,
) -> Result<(), BoundProcessorError> {
    let mappings = base_version
        .downloads
        .client_mappings
        .as_ref()
        .ok_or(BoundProcessorError::Authority)?;
    let expected = ExpectedIntegrity::from_mojang(mappings.size, &mappings.sha1);
    if source.kind() != crate::download::SelectedDownloadArtifactKind::ClientMappings
        || source.logical_identity() != base_version.id
        || source.provider_url() != mappings.url
        || source.expected() != &expected
    {
        return Err(BoundProcessorError::Authority);
    }
    validate_mapping_bytes(base_version, source.bytes())
}

fn validate_mapping_bytes(
    base_version: &VersionJson,
    bytes: &[u8],
) -> Result<(), BoundProcessorError> {
    let mappings = base_version
        .downloads
        .client_mappings
        .as_ref()
        .ok_or(BoundProcessorError::Authority)?;
    if u64::try_from(mappings.size).ok() != Some(bytes.len() as u64)
        || !mappings
            .sha1
            .eq_ignore_ascii_case(&format!("{:x}", Sha1::digest(bytes)))
    {
        return Err(BoundProcessorError::Authority);
    }
    validate_mapping_text(bytes)
}

fn validate_mapping_text(bytes: &[u8]) -> Result<(), BoundProcessorError> {
    if bytes.is_empty()
        || bytes.len() as u64 > MAX_MAPPING_BYTES
        || bytes.contains(&0)
        || std::str::from_utf8(bytes).is_err()
    {
        return Err(BoundProcessorError::Authority);
    }
    Ok(())
}

fn extract_mcp_mappings(
    bytes: &[u8],
    minecraft_version: &str,
) -> Result<Vec<u8>, BoundProcessorError> {
    #[derive(serde::Deserialize)]
    struct McpConfig {
        spec: u32,
        version: String,
        data: McpData,
    }
    #[derive(serde::Deserialize)]
    struct McpData {
        mappings: String,
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| BoundProcessorError::Authority)?;
    if archive.len() > MAX_PROCESSOR_JAR_ENTRIES {
        return Err(BoundProcessorError::Authority);
    }
    let mut files = BTreeMap::new();
    let mut spellings = BTreeMap::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| BoundProcessorError::Authority)?;
        let name = entry.name().strip_suffix('/').unwrap_or(entry.name());
        let path =
            PortableRelativePath::new_exact(name).map_err(|_| BoundProcessorError::Authority)?;
        let kind = entry.unix_mode().unwrap_or_default() & 0o170000;
        if (entry.is_dir() && !matches!(kind, 0 | 0o040000))
            || (!entry.is_dir() && !matches!(kind, 0 | 0o100000))
        {
            return Err(BoundProcessorError::Authority);
        }
        let mut prefix = String::new();
        for component in path.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let ancestor = PortableRelativePath::new_exact(&prefix)
                .map_err(|_| BoundProcessorError::Authority)?;
            if spellings
                .insert(ancestor.key(), ancestor.clone())
                .is_some_and(|prior| prior != ancestor)
            {
                return Err(BoundProcessorError::Authority);
            }
        }
        if files.insert(path, (index, entry.is_dir())).is_some() {
            return Err(BoundProcessorError::Authority);
        }
    }
    for path in files.keys() {
        let mut parent = path.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            let prefix_path = PortableRelativePath::new_exact(prefix)
                .map_err(|_| BoundProcessorError::Authority)?;
            if matches!(files.get(&prefix_path), Some((_, false))) {
                return Err(BoundProcessorError::Authority);
            }
            parent = prefix;
        }
    }
    let config_path = PortableRelativePath::new_exact("config.json")
        .map_err(|_| BoundProcessorError::Authority)?;
    let config_bytes = read_mcp_entry(&mut archive, &files, &config_path, MAX_MCP_CONFIG_BYTES)?;
    let config: McpConfig =
        serde_json::from_slice(&config_bytes).map_err(|_| BoundProcessorError::Authority)?;
    if config.spec != 4 || config.version != minecraft_version {
        return Err(BoundProcessorError::Authority);
    }
    let mappings_path = PortableRelativePath::new_exact(&config.data.mappings)
        .map_err(|_| BoundProcessorError::Authority)?;
    let mappings = read_mcp_entry(&mut archive, &files, &mappings_path, MAX_MAPPING_BYTES)?;
    validate_mapping_text(&mappings)?;
    Ok(mappings)
}

fn read_mcp_entry(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    files: &BTreeMap<PortableRelativePath, (usize, bool)>,
    path: &PortableRelativePath,
    limit: u64,
) -> Result<Vec<u8>, BoundProcessorError> {
    let &(index, directory) = files.get(path).ok_or(BoundProcessorError::Authority)?;
    let mut entry = archive
        .by_index(index)
        .map_err(|_| BoundProcessorError::Authority)?;
    if directory || entry.size() == 0 || entry.size() > limit {
        return Err(BoundProcessorError::Authority);
    }
    let mut bytes = Vec::new();
    entry
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| BoundProcessorError::Authority)?;
    if bytes.len() as u64 != entry.size() || bytes.len() as u64 > limit {
        return Err(BoundProcessorError::Authority);
    }
    Ok(bytes)
}

fn staged_artifact_bytes(
    workspace: &ProcessorWorkspace,
    artifact: &BoundProcessorArtifact,
    authority: &BTreeMap<PortableRelativePath, AuthenticatedBytes>,
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
                                PortableRelativePath::new(&format!("{minecraft_version}.jar"))
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
    let output_root = workspace.temp_path();
    match argument {
        BoundProcessorArgument::Artifact(artifact) => Ok(workspace
            .libraries_path()
            .join(artifact.relative_path.as_str())
            .into_os_string()),
        BoundProcessorArgument::OutputArtifact(artifact) => Ok(output_root
            .join(artifact.relative_path.as_str())
            .into_os_string()),
        BoundProcessorArgument::Template(parts) => {
            let mut rendered = OsString::new();
            for part in parts {
                match part {
                    BoundProcessorArgumentPart::Literal(value) => rendered.push(value),
                    BoundProcessorArgumentPart::DataToken(token) => {
                        push_data_value(&mut rendered, token, plan, workspace)?;
                    }
                    BoundProcessorArgumentPart::OutputToken(token) => {
                        let Some(BoundProcessorData::Artifact(artifact)) = plan.data.get(token)
                        else {
                            return Err(BoundProcessorError::Authority);
                        };
                        rendered.push(output_root.join(artifact.relative_path.as_str()));
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

async fn wait_for_contained_child(
    child: &mut ContainedChild,
    cancel: &mut oneshot::Receiver<()>,
    pipe: &mut mpsc::UnboundedReceiver<PipeEvent>,
    workspace: Option<&ProcessorWorkspace>,
    process_timeout: Duration,
    drain_timeout: Duration,
) -> Result<(), BoundProcessorError> {
    let deadline = tokio::time::sleep(process_timeout);
    tokio::pin!(deadline);
    let mut stage_watch = tokio::time::interval(STAGE_WATCH_INTERVAL);
    stage_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut finished_pipes = 0_usize;
    let outcome = loop {
        let outcome = tokio::select! {
            biased;
            event = pipe.recv(), if finished_pipes < 2 => match event {
                Some(PipeEvent::Finished) => {
                    finished_pipes += 1;
                    continue;
                }
                Some(PipeEvent::Limit) => {
                    finished_pipes += 1;
                    Err(BoundProcessorError::OutputLimit)
                }
                Some(PipeEvent::Read) => {
                    finished_pipes += 1;
                    Err(BoundProcessorError::Unsuccessful)
                }
                None => {
                    finished_pipes = 2;
                    Err(BoundProcessorError::Unsuccessful)
                }
            },
            _ = &mut *cancel => Err(BoundProcessorError::Cancelled),
            _ = &mut deadline => Err(BoundProcessorError::Timeout),
            _ = stage_watch.tick(), if workspace.is_some() => match workspace
                .expect("guarded processor workspace")
                .validate_live_bounds()
            {
                Ok(()) => continue,
                Err(source) => {
                    let io_kind = match source {
                        super::types::LoaderError::Io(error) => Some(error.kind()),
                        _ => None,
                    };
                    tracing::warn!(stage = "step_live_bounds", ?io_kind, error = %BoundProcessorError::Stage, "Forge processor execution failed");
                    Err(BoundProcessorError::Stage)
                },
            },
            status = child.child.wait() => match status {
                Ok(status) if status.success() => Ok(()),
                Ok(_) | Err(_) => Err(BoundProcessorError::Unsuccessful),
            },
        };
        break outcome;
    };

    terminate_and_reap(child).await?;

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
    let drain_result = tokio::time::timeout(drain_timeout, drains)
        .await
        .map_err(|_| BoundProcessorError::Unsuccessful)
        .and_then(|result| result);
    match outcome {
        Ok(()) => drain_result,
        Err(error) => Err(error),
    }
}

async fn terminate_and_reap(child: &mut ContainedChild) -> Result<(), BoundProcessorError> {
    child.containment.terminate()?;
    tokio::time::timeout(PROCESS_REAP_TIMEOUT, child.child.wait())
        .await
        .map_err(|_| BoundProcessorError::Unreaped)?
        .map_err(|_| BoundProcessorError::Unreaped)?;
    let deadline = tokio::time::Instant::now() + PROCESS_REAP_TIMEOUT;
    loop {
        let empty = process_containment_is_empty(&child.containment).await?;
        if empty {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BoundProcessorError::Unreaped);
        }
        tokio::time::sleep(PROCESS_REAP_POLL_INTERVAL).await;
    }
}

async fn process_containment_is_empty(
    containment: &ProcessContainment,
) -> Result<bool, BoundProcessorError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let group = containment.group;
        let admission = process_physical_work()
            .admit(PhysicalWorkRequest::foreground(
                PhysicalIoClass::Metadata,
                0,
            ))
            .await
            .map_err(|_| BoundProcessorError::Unreaped)?;
        admission
            .run(move |_| {
                #[cfg(target_os = "linux")]
                {
                    linux_process_group_is_empty(group)
                }
                #[cfg(target_os = "macos")]
                {
                    macos_process_group_is_empty(group)
                }
            })
            .await
            .map_err(|_| BoundProcessorError::Unreaped)?
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        containment.is_empty()
    }
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
        .map(|path| PortableRelativePath::new(&format!("root/{}", path.as_str())))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| BoundProcessorError::Stage)?;
    exact_added_files(
        before_root,
        after_root,
        &expected_root,
        "step_output_root_diff",
    )?;
    let diff = before_stage.diff(after_stage);
    let root_additions = diff
        .added_files()
        .keys()
        .filter(|path| path.as_str().starts_with("root/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let scratch_file = |path: &PortableRelativePath| {
        path.as_str().starts_with("home/") || path.as_str().starts_with("tmp/")
    };
    let scratch_directory = |path: &PortableRelativePath| {
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
        tracing::warn!(
            stage = "step_output_stage_diff",
            expected_outputs = expected_stage.len(),
            root_additions = root_additions.len(),
            added_files = diff.added_files().len(),
            modified_files = diff.modified_files().len(),
            removed_files = diff.removed_files().len(),
            added_directories = diff.added_directories().len(),
            removed_directories = diff.removed_directories().len(),
            error = %BoundProcessorError::Stage,
            "Forge processor execution failed"
        );
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
            PortableRelativePath::new(&format!(
                "root/libraries/{}",
                output.artifact.relative_path.as_str()
            ))
            .map_err(|_| BoundProcessorError::Authority)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    exact_added_files(before, settled, &expected, "step_clean_stage_diff")
}

fn exact_added_files(
    before: &ManagedTreeSnapshot,
    after: &ManagedTreeSnapshot,
    expected: &BTreeSet<PortableRelativePath>,
    stage: &'static str,
) -> Result<(), BoundProcessorError> {
    let diff = before.diff(after);
    let added = diff.added_files().keys().cloned().collect::<BTreeSet<_>>();
    if added != *expected
        || !diff.removed_files().is_empty()
        || !diff.modified_files().is_empty()
        || !diff.added_directories().is_empty()
        || !diff.removed_directories().is_empty()
    {
        tracing::warn!(
            stage,
            expected_files = expected.len(),
            added_files = added.len(),
            missing_expected = expected.difference(&added).count(),
            unexpected_added = added.difference(expected).count(),
            modified_files = diff.modified_files().len(),
            removed_files = diff.removed_files().len(),
            added_directories = diff.added_directories().len(),
            removed_directories = diff.removed_directories().len(),
            error = %BoundProcessorError::Stage,
            "Forge processor execution failed"
        );
        return Err(BoundProcessorError::Stage);
    }
    Ok(())
}

fn library_root_path(
    relative: &PortableRelativePath,
) -> Result<PortableRelativePath, BoundProcessorError> {
    PortableRelativePath::new(&format!("libraries/{}", relative.as_str()))
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
            PortableRelativePath::new(&format!("root/libraries/{}", path.as_str()))
                .map_err(|_| BoundProcessorError::Authority)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    expected.insert(
        PortableRelativePath::new(&format!(
            "root/versions/{minecraft_version}/{minecraft_version}.jar"
        ))
        .map_err(|_| BoundProcessorError::Authority)?,
    );
    for path in plan.installer_data.keys() {
        expected.insert(
            PortableRelativePath::new(&format!("root/processor-data/{}", path.as_str()))
                .map_err(|_| BoundProcessorError::Authority)?,
        );
    }
    if plan_requires_installer(plan) {
        expected.insert(
            PortableRelativePath::new("root/installer.jar")
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
                PortableRelativePath::new(&segments.join("/"))
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

impl VerifiedProcessorOutputs {
    pub(crate) fn into_entries(self) -> BTreeMap<PortableRelativePath, VerifiedProcessorOutput> {
        self.entries
    }

    #[cfg(test)]
    pub(crate) fn from_test_terminal(entries: Vec<(PortableRelativePath, Vec<u8>)>) -> Self {
        Self::from_test_terminal_with_expectations(
            entries
                .into_iter()
                .map(|(path, bytes)| {
                    let expectation =
                        BoundProcessorOutputExpectation::ProviderSha1(Sha1::digest(&bytes).into());
                    (path, bytes, expectation)
                })
                .collect(),
        )
    }

    #[cfg(test)]
    pub(crate) fn from_test_terminal_with_expectations(
        entries: Vec<(
            PortableRelativePath,
            Vec<u8>,
            BoundProcessorOutputExpectation,
        )>,
    ) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(path, bytes, expectation)| {
                    let size = bytes.len() as u64;
                    let sha1 = Sha1::digest(&bytes).into();
                    (
                        path,
                        VerifiedProcessorOutput {
                            bytes,
                            size,
                            sha1,
                            expectation,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl VerifiedProcessorOutput {
    pub(crate) fn into_parts_for_expectation(
        self,
        expected: &BoundProcessorOutputExpectation,
    ) -> Result<(Vec<u8>, u64, [u8; 20]), BoundProcessorError> {
        if &self.expectation != expected
            || matches!(expected, BoundProcessorOutputExpectation::ProviderSha1(sha1) if sha1 != &self.sha1)
        {
            return Err(BoundProcessorError::Authority);
        }
        Ok((self.bytes, self.size, self.sha1))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthenticatedBytes, BoundProcessorError, PipeEvent, StagedAuthority, drain_pipe,
        manifest_attributes, processor_main_class, reauthenticate_step_dependencies,
        valid_main_class,
    };
    use super::{spawn_contained_child, wait_for_contained_child};
    use crate::loaders::forge_installer::{
        BoundProcessorAction, BoundProcessorArgument, BoundProcessorArgumentPart,
        BoundProcessorArtifact, BoundProcessorData, BoundProcessorOutput,
        BoundProcessorOutputExpectation, BoundProcessorOutputRole, BoundProcessorPlan,
        BoundProcessorStep, ProcessorBuiltinToken, ProcessorDerivation,
    };
    use crate::loaders::workspace::cleanup::prepare_ephemeral_processor_workspace;
    use crate::portable_path::PortableRelativePath;
    use sha1::{Digest as _, Sha1};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::sync::{Arc, atomic::AtomicUsize};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_process_stat_with_adversarial_process_names() {
        let zombie = b"42 (processor) worker\n\xff) Z 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0";
        assert_eq!(
            super::parse_linux_process_stat(zombie, 42),
            Some((b'Z', 42, 1))
        );
        assert_eq!(
            super::parse_linux_process_stat(
                b"43 (processor) R 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 2 0",
                43
            ),
            Some((b'R', 42, 2))
        );
        assert_eq!(super::parse_linux_process_stat(zombie, 43), None);
        assert_eq!(super::parse_linux_process_stat(b"malformed", 42), None);
    }

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

    fn mcp_archive(config: &[u8], entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in std::iter::once(("config.json", config)).chain(entries.iter().copied())
        {
            writer
                .start_file(name, SimpleFileOptions::default())
                .expect("entry");
            writer.write_all(bytes).expect("entry bytes");
        }
        writer.finish().expect("archive").into_inner()
    }

    const MCP_CONFIG: &[u8] =
        br#"{"spec":4,"version":"1.20.1","data":{"mappings":"config/joined.tsrg"}}"#;
    const MCP_MAPPINGS: &[u8] = b"tsrg2 obf srg id\na example/Class 1\n";

    #[test]
    fn extracts_mcp_exact_configured_entry_with_matching_schema_and_version() {
        let archive = mcp_archive(MCP_CONFIG, &[("config/joined.tsrg", MCP_MAPPINGS)]);
        assert_eq!(
            super::extract_mcp_mappings(&archive, "1.20.1").expect("mappings"),
            MCP_MAPPINGS
        );
        assert!(super::extract_mcp_mappings(&archive, "1.20.2").is_err());
        for config in [
            br#"{"spec":3,"version":"1.20.1","data":{"mappings":"config/joined.tsrg"}}"#.as_slice(),
            br#"{"spec":4,"version":"1.20.1","data":{"mappings":"../joined.tsrg"}}"#.as_slice(),
            br#"{"spec":4,"version":"1.20.1","data":{"mappings":"CONFIG/joined.tsrg"}}"#.as_slice(),
            br#"{"spec":4,"version":"1.20.1","data":{"mappings":"config/missing.tsrg"}}"#.as_slice(),
            br#"{"spec":4,"version":"1.20.1","data":{"mappings":"config/joined.tsrg","mappings":"config/other.tsrg"}}"#.as_slice(),
        ] {
            let archive = mcp_archive(config, &[("config/joined.tsrg", MCP_MAPPINGS)]);
            assert!(super::extract_mcp_mappings(&archive, "1.20.1").is_err());
        }
    }

    #[test]
    fn rejects_mcp_unsafe_alias_empty_nontext_and_oversized_entries() {
        for entries in [
            vec![
                ("config/joined.tsrg", MCP_MAPPINGS),
                ("Config/unrelated", b"other".as_slice()),
            ],
            vec![
                ("config/joined.tsrg", MCP_MAPPINGS),
                ("CONFIG.JSON", b"{}".as_slice()),
            ],
            vec![
                ("config/joined.tsrg", MCP_MAPPINGS),
                ("config", b"not a directory".as_slice()),
            ],
            vec![("../config/joined.tsrg", MCP_MAPPINGS)],
            vec![("config/joined.tsrg", b"".as_slice())],
            vec![("config/joined.tsrg", b"\xff".as_slice())],
            vec![("config/joined.tsrg", b"a\0b".as_slice())],
        ] {
            assert!(
                super::extract_mcp_mappings(&mcp_archive(MCP_CONFIG, &entries), "1.20.1").is_err()
            );
        }
        let oversized = vec![b' '; (super::MAX_MCP_CONFIG_BYTES + 1) as usize];
        assert!(super::extract_mcp_mappings(&mcp_archive(&oversized, &[]), "1.20.1").is_err());
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("config.json", SimpleFileOptions::default())
            .expect("config");
        writer.write_all(MCP_CONFIG).expect("config bytes");
        writer
            .add_symlink(
                "config/joined.tsrg",
                "elsewhere",
                SimpleFileOptions::default(),
            )
            .expect("symlink");
        let archive = writer.finish().expect("archive").into_inner();
        assert!(super::extract_mcp_mappings(&archive, "1.20.1").is_err());
    }

    #[test]
    fn mapping_bytes_require_retained_base_size_hash_and_text() {
        let mut base: crate::launch::VersionJson =
            serde_json::from_value(serde_json::json!({"id": "1.20.1"})).expect("base version");
        base.downloads.client_mappings = Some(crate::launch::DownloadEntry {
            url: "https://piston-data.mojang.com/client.txt".to_string(),
            sha1: format!("{:x}", Sha1::digest(MCP_MAPPINGS)),
            size: MCP_MAPPINGS.len() as i64,
            ..Default::default()
        });
        super::validate_mapping_bytes(&base, MCP_MAPPINGS).expect("authenticated mapping bytes");
        assert!(super::validate_mapping_bytes(&base, b"changed").is_err());
        base.downloads
            .client_mappings
            .as_mut()
            .expect("mapping")
            .size += 1;
        assert!(super::validate_mapping_bytes(&base, MCP_MAPPINGS).is_err());
        base.downloads
            .client_mappings
            .as_mut()
            .expect("mapping")
            .size -= 1;
        base.downloads
            .client_mappings
            .as_mut()
            .expect("mapping")
            .sha1 = "0".repeat(40);
        assert!(super::validate_mapping_bytes(&base, MCP_MAPPINGS).is_err());
        base.downloads.client_mappings = None;
        assert!(super::validate_mapping_bytes(&base, MCP_MAPPINGS).is_err());
    }

    fn native_mapping_step() -> BoundProcessorStep {
        let artifact = |coordinate: &str, path: &str| BoundProcessorArtifact {
            coordinate: coordinate.to_string(),
            relative_path: PortableRelativePath::new_exact(path).expect("path"),
        };
        let input = artifact("de.oceanlabs.mcp:mcp_config:1@zip", "mcp/config.zip");
        let output = artifact(
            "de.oceanlabs.mcp:mcp_config:1:mappings@txt",
            "mcp/mappings.txt",
        );
        BoundProcessorStep {
            action: BoundProcessorAction::ExtractMcpMappings {
                input: input.clone(),
            },
            jar: artifact(
                "net.minecraftforge:installertools:1.4.1",
                "forge/installertools.jar",
            ),
            classpath: Vec::new(),
            args: vec![
                BoundProcessorArgument::Artifact(input),
                BoundProcessorArgument::OutputArtifact(output.clone()),
            ],
            outputs: vec![BoundProcessorOutput {
                artifact: output,
                expectation: BoundProcessorOutputExpectation::Derived(
                    super::super::forge_installer::ProcessorDerivation::from_test(),
                ),
                role: BoundProcessorOutputRole::Intermediate,
            }],
        }
    }

    #[tokio::test]
    async fn native_output_settlement_preserves_intermediate_provenance_and_exact_diff() {
        let owner =
            prepare_ephemeral_processor_workspace("forge-test", "1.20.1").expect("workspace");
        let workspace = owner.workspace();
        let step = native_mapping_step();
        let path = &step.outputs[0].artifact.relative_path;
        workspace
            .ensure_library_parent(path)
            .expect("output parent");
        let before_root = workspace.snapshot_root().expect("root snapshot");
        let before_stage = workspace.snapshot_stage().expect("stage snapshot");
        let (_cancel_tx, mut cancel) = oneshot::channel();
        super::write_native_output(&step, workspace, MCP_MAPPINGS, &mut cancel)
            .await
            .expect("write mappings");
        let mut settled = super::settle_step_outputs(&step, workspace, &before_root, &before_stage)
            .expect("settled mappings");
        let output = settled.remove(path).expect("mapping output");
        assert!(!output.terminal);
        assert!(output.bytes.is_none());
        assert_eq!(output.expectation, step.outputs[0].expectation);
        assert_eq!(output.sha1, <[u8; 20]>::from(Sha1::digest(MCP_MAPPINGS)));
        assert!(
            super::write_native_output(&step, workspace, MCP_MAPPINGS, &mut cancel)
                .await
                .is_err()
        );
        fs::write(workspace.root_path().join("unexpected"), b"unexpected")
            .expect("unexpected file");
        assert!(super::settle_step_outputs(&step, workspace, &before_root, &before_stage).is_err());
        owner.cleanup().expect("cleanup");
    }

    #[tokio::test]
    async fn native_output_cancellation_and_wrong_provider_hash_fail_closed() {
        let owner =
            prepare_ephemeral_processor_workspace("forge-test", "1.20.1").expect("workspace");
        let workspace = owner.workspace();
        let mut step = native_mapping_step();
        workspace
            .ensure_library_parent(&step.outputs[0].artifact.relative_path)
            .expect("output parent");
        let before_root = workspace.snapshot_root().expect("root snapshot");
        let before_stage = workspace.snapshot_stage().expect("stage snapshot");
        let (cancel_tx, mut cancel) = oneshot::channel();
        cancel_tx.send(()).expect("cancel");
        assert!(matches!(
            super::write_native_output(&step, workspace, MCP_MAPPINGS, &mut cancel).await,
            Err(BoundProcessorError::Cancelled)
        ));
        assert_eq!(
            workspace.snapshot_root().expect("root snapshot"),
            before_root
        );
        let (_cancel_tx, mut cancel) = oneshot::channel();
        super::write_native_output(&step, workspace, MCP_MAPPINGS, &mut cancel)
            .await
            .expect("write mappings");
        step.outputs[0].expectation = BoundProcessorOutputExpectation::ProviderSha1([0; 20]);
        assert!(matches!(
            super::settle_step_outputs(&step, workspace, &before_root, &before_stage),
            Err(BoundProcessorError::Authority)
        ));
        owner.cleanup().expect("cleanup");
    }

    #[tokio::test]
    async fn final_rescan_reauthenticates_derived_outputs_and_original_inputs() {
        let owner =
            prepare_ephemeral_processor_workspace("forge-test", "1.20.1").expect("workspace");
        let workspace = owner.workspace();
        let version_path = PortableRelativePath::new_exact("1.20.1.jar").expect("version path");
        workspace
            .write_version_exact(&version_path, b"client")
            .await
            .expect("client");
        let initial = workspace.snapshot_stage().expect("initial snapshot");
        let step = native_mapping_step();
        let path = step.outputs[0].artifact.relative_path.clone();
        workspace
            .ensure_library_parent(&path)
            .expect("output parent");
        let (_cancel_tx, mut cancel) = oneshot::channel();
        super::write_native_output(&step, workspace, MCP_MAPPINGS, &mut cancel)
            .await
            .expect("output");
        let facts = |bytes: &[u8]| AuthenticatedBytes {
            size: bytes.len() as u64,
            sha1: Sha1::digest(bytes).into(),
        };
        let authority = StagedAuthority {
            libraries: BTreeMap::from([(path.clone(), facts(MCP_MAPPINGS))]),
            version: facts(b"client"),
            processor_data: BTreeMap::new(),
            installer: None,
        };
        let plan = BoundProcessorPlan {
            steps: vec![step],
            data: BTreeMap::new(),
            installer_data: BTreeMap::new(),
            input_artifacts: BTreeMap::new(),
        };
        super::final_rescan(workspace, &plan, "1.20.1", &authority, &initial)
            .expect("settled tree");
        fs::write(workspace.libraries_path().join(path.as_str()), b"replaced")
            .expect("replace output");
        assert!(super::final_rescan(workspace, &plan, "1.20.1", &authority, &initial).is_err());
        workspace
            .write_library_exact(&path, MCP_MAPPINGS)
            .await
            .expect("restore output");
        fs::write(
            workspace.version_path().join(version_path.as_str()),
            b"changed",
        )
        .expect("replace client");
        assert!(super::final_rescan(workspace, &plan, "1.20.1", &authority, &initial).is_err());
        owner.cleanup().expect("cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn java_outputs_reconstruct_classic_provider_bytes_before_batch_publication() {
        // Smallest differing entry from authenticated Forge 47.4.10 Java output:
        // assets/minecraft/font/include/unifont.json (29 bytes; zlib-ng raw DEFLATE).
        let bytes = b"\x50\x4b\x03\x04\x14\x00\x08\x08\x08\x00\x00\x00\x7a\x13\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
            \x00\x00\x2a\x00\x00\x00\x61\x73\x73\x65\x74\x73\x2f\x6d\x69\x6e\x65\x63\x72\x61\x66\x74\x2f\x66\
            \x6f\x6e\x74\x2f\x69\x6e\x63\x6c\x75\x64\x65\x2f\x75\x6e\x69\x66\x6f\x6e\x74\x2e\x6a\x73\x6f\x6e\
            \xab\xe6\x52\x50\x50\x50\x50\x2a\x28\xca\x2f\xcb\x4c\x49\x2d\x2a\x56\xb2\x52\x88\x06\x0b\xc5\x72\
            \xd5\x72\x01\x00\x50\x4b\x07\x08\x1c\x6d\x4d\x8e\x1c\x00\x00\x00\x1d\x00\x00\x00\x50\x4b\x01\x02\
            \x14\x00\x14\x00\x08\x08\x08\x00\x00\x00\x7a\x13\x1c\x6d\x4d\x8e\x1c\x00\x00\x00\x1d\x00\x00\x00\
            \x2a\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x61\x73\x73\x65\x74\x73\
            \x2f\x6d\x69\x6e\x65\x63\x72\x61\x66\x74\x2f\x66\x6f\x6e\x74\x2f\x69\x6e\x63\x6c\x75\x64\x65\x2f\
            \x75\x6e\x69\x66\x6f\x6e\x74\x2e\x6a\x73\x6f\x6e\x50\x4b\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00\
            \x58\x00\x00\x00\x74\x00\x00\x00\x00\x00";
        let provider = [
            0x03, 0xa2, 0x7b, 0x3e, 0x51, 0xb1, 0xe9, 0x03, 0x40, 0xcd, 0xef, 0x35, 0x57, 0x2c,
            0x5c, 0x01, 0xc7, 0x7b, 0x88, 0x4e,
        ];
        let owner = prepare_ephemeral_processor_workspace("split-classic-test", "1.20.1")
            .expect("canonical output owner");
        let base = serde_json::from_value(serde_json::json!({
            "id": "1.20.1", "javaVersion": {"component": "java-runtime-delta", "majorVersion": 17}
        }))
        .expect("runtime fixture version");
        let runtime = split_test_runtime(&owner, &base).await;
        let workspace = owner.workspace();
        let artifact = |name: &str| BoundProcessorArtifact {
            coordinate: format!("example:{name}:1"),
            relative_path: PortableRelativePath::new_exact(&format!("example/{name}.jar")).unwrap(),
        };
        let input = artifact("input");
        workspace
            .write_library_exact(&input.relative_path, bytes)
            .await
            .expect("immutable input");
        let mut step = BoundProcessorStep {
            action: BoundProcessorAction::Java,
            jar: input,
            classpath: Vec::new(),
            args: Vec::new(),
            outputs: ["slim", "extra"]
                .into_iter()
                .map(|name| BoundProcessorOutput {
                    artifact: artifact(name),
                    expectation: BoundProcessorOutputExpectation::ProviderSha1(provider),
                    role: BoundProcessorOutputRole::Terminal {
                        expected_size: Some(224),
                    },
                })
                .collect(),
        };
        for output in &step.outputs {
            let path = &output.artifact.relative_path;
            workspace.ensure_temp_parent(path).expect("scratch parent");
            workspace
                .ensure_library_parent(path)
                .expect("library parent");
            fs::write(workspace.temp_path().join(path.as_str()), bytes).expect("Java output");
        }
        let before = workspace.snapshot_root().expect("immutable input facts");
        let input_path = workspace
            .libraries_path()
            .join(step.jar.relative_path.as_str());
        let (_cancel_tx, mut cancel) = oneshot::channel();
        step.outputs[1].expectation = BoundProcessorOutputExpectation::ProviderSha1([0; 20]);
        let refused = super::promote_java_outputs(&step, workspace, &before, &mut cancel).await;
        let refused_root = workspace.snapshot_root().expect("refused batch facts");
        step.outputs[1].expectation = BoundProcessorOutputExpectation::ProviderSha1(provider);
        let (cancel_tx, mut cancelled) = oneshot::channel();
        cancel_tx.send(()).expect("cancel promotion");
        let cancelled_result =
            super::promote_java_outputs(&step, workspace, &before, &mut cancelled).await;
        let cancelled_root = workspace.snapshot_root().expect("cancelled batch facts");
        let mut exhausted = 1;
        let budget_result =
            super::recompress_java_archive(bytes, &mut exhausted, &mut cancel).await;
        step.outputs[1].role = BoundProcessorOutputRole::Terminal {
            expected_size: Some(225),
        };
        let wrong_size = super::promote_java_outputs(&step, workspace, &before, &mut cancel).await;
        let wrong_size_root = workspace.snapshot_root().expect("wrong size batch facts");
        step.outputs[1].role = BoundProcessorOutputRole::Terminal {
            expected_size: Some(224),
        };
        fs::write(&input_path, b"changed").expect("changed authenticated input");
        let changed_input =
            super::promote_java_outputs(&step, workspace, &before, &mut cancel).await;
        let changed_root = workspace
            .snapshot_root()
            .expect("changed input batch facts");
        fs::write(&input_path, bytes).expect("restore authenticated input");
        let result = super::promote_java_outputs(&step, workspace, &before, &mut cancel).await;
        let after = workspace.snapshot_root().expect("published facts");
        let scratch_empty = fs::read_dir(workspace.temp_path())
            .expect("scratch directory")
            .next()
            .is_none();
        let mut retained = after.files().clone();
        let observed: Vec<_> = step
            .outputs
            .iter()
            .map(|output| {
                retained.remove(&super::library_root_path(&output.artifact.relative_path).unwrap())
            })
            .collect();
        drop(runtime);
        let cleanup = owner.cleanup();
        cleanup.expect("settled owner cleanup");
        assert_eq!(
            format!("{:x}", Sha1::digest(bytes)),
            "4c76f0fc157635dfdda3bce53aac5576e8c996eb"
        );
        assert!(matches!(refused, Err(BoundProcessorError::Authority)));
        assert_eq!(
            before.files(),
            refused_root.files(),
            "bad second hash cannot publish the first"
        );
        assert!(matches!(
            cancelled_result,
            Err(BoundProcessorError::Cancelled)
        ));
        assert_eq!(before.files(), cancelled_root.files());
        assert!(matches!(budget_result, Err(BoundProcessorError::Authority)));
        assert!(matches!(wrong_size, Err(BoundProcessorError::Authority)));
        assert_eq!(before.files(), wrong_size_root.files());
        assert!(matches!(changed_input, Err(BoundProcessorError::Stage)));
        assert_eq!(before.files().len(), changed_root.files().len());
        for output in &step.outputs {
            let path = super::library_root_path(&output.artifact.relative_path).unwrap();
            assert!(!changed_root.files().contains_key(&path));
        }
        result.expect("correct classic provider reconstruction");
        assert!(scratch_empty);
        assert_eq!(
            before.files(),
            &retained,
            "authenticated input remains unchanged"
        );
        for output in observed {
            let output = output.expect("published output");
            assert_eq!(output.size(), 224);
            assert_eq!(output.sha1(), &provider);
        }
    }

    #[cfg(unix)]
    async fn split_test_runtime(
        owner: &super::ProcessorWorkspaceOwner,
        base: &crate::launch::VersionJson,
    ) -> super::ProcessorRuntime {
        let program = br#"#!/bin/sh
case "$*" in
  *-version*) printf '%s\n' 'openjdk version "17.0.1"' >&2; exit 0 ;;
esac
/bin/cp "$5" "$6"
if [ "$4" != derived-missing ]; then
  /bin/cp "$5" "$7"
fi
if [ "$4" != generic ]; then
  printf 'cache' > "$6.cache"
  printf 'cache' > "$7.cache"
fi
case "$4" in
  wrong|generic-wrong) printf 'invalid' > "$7" ;;
  derived-empty) : > "$7" ;;
  unexpected|derived-unexpected|generic-unexpected) printf 'unexpected' > unexpected ;;
esac
"#
        .to_vec();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let url = format!(
            "http://{}/java",
            listener.local_addr().expect("fixture address")
        );
        let manifest = serde_json::from_value(serde_json::json!({
            "files": {
                "bin": {"type": "directory"},
                (crate::runtime::runtime_java_relative_path()): {
                    "type": "file", "executable": true,
                    "downloads": {"raw": {
                        "url": url,
                        "sha1": format!("{:x}", Sha1::digest(&program)),
                        "size": program.len()
                    }}
                }
            }
        }))
        .expect("fixture runtime manifest");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("fixture connection");
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 4096, "bounded fixture request");
                header.push(
                    tokio::io::AsyncReadExt::read_u8(&mut stream)
                        .await
                        .expect("request byte"),
                );
            }
            assert!(header.starts_with(b"GET /java HTTP/1.1\r\n"));
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        program.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("response header");
            stream.write_all(&program).await.expect("runtime bytes");
        });
        let source = crate::runtime::authenticated_runtime_source_from_manifest_for_test(
            crate::runtime::RuntimeId::from("java-runtime-delta"),
            manifest,
        )
        .expect("authenticated runtime fixture");
        let runtime = owner
            .materialize_runtime(&base.java_version, source)
            .await
            .expect("materialize fixture runtime");
        server.await.expect("runtime fixture served");
        runtime
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn split_jar_executes_with_disposable_caches_and_preserves_exact_outputs() {
        for mode in [
            "split",
            "wrong",
            "unexpected",
            "generic",
            "generic-wrong",
            "generic-size",
            "generic-unexpected",
            "derived",
            "derived-empty",
            "derived-missing",
            "derived-size",
            "derived-unexpected",
        ] {
            let owner =
                prepare_ephemeral_processor_workspace("split-test", "1.20.1").expect("workspace");
            let base: crate::launch::VersionJson = serde_json::from_value(serde_json::json!({
                "id": "1.20.1",
                "javaVersion": {"component": "java-runtime-delta", "majorVersion": 17}
            }))
            .expect("base version");
            let workspace = owner.workspace();
            let artifact = |coordinate: &str, path: &str| BoundProcessorArtifact {
                coordinate: coordinate.to_string(),
                relative_path: PortableRelativePath::new_exact(path).expect("artifact path"),
            };
            let jar = artifact("example:processor:1", "example/processor/1/processor-1.jar");
            let input = artifact("example:input:1", "example/input/1/input-1.jar");
            let slim = artifact("example:slim:1", "example/slim/output.jar");
            let extra = artifact("example:extra:1", "example/extra/output.jar");
            let bytes = mcp_archive(
                MCP_CONFIG,
                &[("META-INF/MANIFEST.MF", b"Main-Class: example.Processor\n\n")],
            );
            let sha1: [u8; 20] = Sha1::digest(&bytes).into();
            workspace
                .write_library_exact(&jar.relative_path, &bytes)
                .await
                .expect("processor jar");
            workspace
                .write_library_exact(&input.relative_path, &bytes)
                .await
                .expect("input jar");
            let mut authority = StagedAuthority {
                libraries: [jar.relative_path.clone(), input.relative_path.clone()]
                    .into_iter()
                    .map(|path| {
                        (
                            path,
                            AuthenticatedBytes {
                                size: bytes.len() as u64,
                                sha1,
                            },
                        )
                    })
                    .collect(),
                version: AuthenticatedBytes {
                    size: 0,
                    sha1: [0; 20],
                },
                processor_data: BTreeMap::new(),
                installer: None,
            };
            let plan = BoundProcessorPlan {
                steps: Vec::new(),
                data: BTreeMap::from([(
                    "EXTRA".to_string(),
                    BoundProcessorData::Artifact(extra.clone()),
                )]),
                installer_data: BTreeMap::new(),
                input_artifacts: BTreeMap::new(),
            };
            let derived = mode.starts_with("derived");
            let expectation = if derived {
                BoundProcessorOutputExpectation::Derived(ProcessorDerivation::from_test())
            } else {
                BoundProcessorOutputExpectation::ProviderSha1(sha1)
            };
            let step = BoundProcessorStep {
                action: if mode.starts_with("generic") {
                    BoundProcessorAction::Java
                } else {
                    BoundProcessorAction::SplitJar
                },
                jar,
                classpath: Vec::new(),
                args: vec![
                    BoundProcessorArgument::Template(vec![BoundProcessorArgumentPart::Literal(
                        mode.to_string(),
                    )]),
                    BoundProcessorArgument::Artifact(input),
                    BoundProcessorArgument::OutputArtifact(slim.clone()),
                    BoundProcessorArgument::Template(vec![
                        BoundProcessorArgumentPart::OutputToken("EXTRA".to_string()),
                    ]),
                ],
                outputs: [slim, extra]
                    .into_iter()
                    .enumerate()
                    .map(|(index, artifact)| BoundProcessorOutput {
                        artifact,
                        expectation: expectation.clone(),
                        role: if derived && index == 0 {
                            BoundProcessorOutputRole::Intermediate
                        } else {
                            BoundProcessorOutputRole::Terminal {
                                expected_size: if matches!(mode, "derived-size" | "generic-size") {
                                    Some(bytes.len() as u64 + 1)
                                } else {
                                    (!derived).then_some(bytes.len() as u64)
                                },
                            }
                        },
                    })
                    .collect(),
            };
            let runtime = split_test_runtime(&owner, &base).await;
            let (_cancel_tx, mut cancel) = oneshot::channel();
            let result = super::run_step(
                &step,
                &plan,
                workspace,
                &runtime,
                &base,
                None,
                &mut authority,
                &mut cancel,
            )
            .await;
            match mode {
                "wrong" | "generic-wrong" | "generic-size" | "derived-empty"
                | "derived-missing" | "derived-size" => {
                    assert!(
                        matches!(result, Err(BoundProcessorError::Authority)),
                        "{mode} must refuse before promotion"
                    );
                    let (cancel_tx, mut cancelled) = oneshot::channel();
                    cancel_tx.send(()).expect("cancel promotion");
                    assert!(matches!(
                        super::promote_java_outputs(
                            &step,
                            workspace,
                            &workspace.snapshot_root().expect("cancelled root facts"),
                            &mut cancelled,
                        )
                        .await,
                        Err(BoundProcessorError::Cancelled)
                    ));
                    for output in &step.outputs {
                        assert!(
                            !workspace
                                .libraries_path()
                                .join(output.artifact.relative_path.as_str())
                                .exists()
                        );
                    }
                }
                "unexpected" | "derived-unexpected" | "generic-unexpected" => {
                    assert!(matches!(result, Err(BoundProcessorError::Stage)))
                }
                _ => {
                    let outputs = result.expect("settled processor outputs");
                    assert_eq!(outputs.len(), 2);
                    for output in &step.outputs {
                        let observed = outputs
                            .get(&output.artifact.relative_path)
                            .expect("exact output");
                        let terminal =
                            matches!(output.role, BoundProcessorOutputRole::Terminal { .. });
                        assert_eq!(
                            observed.bytes.as_deref(),
                            terminal.then_some(bytes.as_slice())
                        );
                        assert_eq!(observed.size, bytes.len() as u64);
                        assert_eq!(observed.sha1, sha1);
                        assert_eq!(observed.expectation, output.expectation);
                        assert_eq!(observed.terminal, terminal);
                    }
                    if derived {
                        let terminal = outputs
                            .values()
                            .find(|output| output.terminal)
                            .expect("terminal output");
                        let consume = |expected: &BoundProcessorOutputExpectation| {
                            super::VerifiedProcessorOutput {
                                bytes: terminal.bytes.clone().expect("terminal bytes"),
                                size: terminal.size,
                                sha1: terminal.sha1,
                                expectation: terminal.expectation.clone(),
                            }
                            .into_parts_for_expectation(expected)
                        };
                        assert_eq!(
                            consume(&expectation).expect("original derivation"),
                            (bytes.clone(), bytes.len() as u64, sha1)
                        );
                        for foreign in [
                            BoundProcessorOutputExpectation::Derived(
                                ProcessorDerivation::from_test(),
                            ),
                            BoundProcessorOutputExpectation::ProviderSha1(sha1),
                        ] {
                            assert!(matches!(
                                consume(&foreign),
                                Err(BoundProcessorError::Authority)
                            ));
                        }
                    }
                    assert!(
                        fs::read_dir(workspace.temp_path())
                            .expect("scratch directory")
                            .next()
                            .is_none()
                    );
                    assert_eq!(
                        workspace
                            .snapshot_root()
                            .expect("root snapshot")
                            .files()
                            .len(),
                        4
                    );
                }
            }
            drop(runtime);
            owner.cleanup().expect("cleanup");
        }
    }

    #[test]
    fn processor_errors_are_closed_static_and_redacted() {
        for error in [
            BoundProcessorError::Authority,
            BoundProcessorError::Source,
            BoundProcessorError::Stage,
            BoundProcessorError::Runtime,
            BoundProcessorError::Manifest,
            BoundProcessorError::Spawn,
            BoundProcessorError::Containment,
            BoundProcessorError::Timeout,
            BoundProcessorError::OutputLimit,
            BoundProcessorError::Unsuccessful,
            BoundProcessorError::Cancelled,
            BoundProcessorError::Unreaped,
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
        let owner = prepare_ephemeral_processor_workspace("forge-target", "1.21.5")
            .expect("processor workspace");
        let workspace = owner.workspace();
        let jar = PortableRelativePath::new("example/processor.jar").expect("jar path");
        let data = PortableRelativePath::new("patch/client.bin").expect("data path");
        let version = PortableRelativePath::new("1.21.5.jar").expect("version path");
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
            action: BoundProcessorAction::Java,
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
        reauthenticate_step_dependencies(&step, &plan, workspace, &authority, "1.21.5")
            .expect("authenticated dependencies");

        fs::write(
            workspace.libraries_path().join("example/processor.jar"),
            b"changed",
        )
        .expect("tamper staged jar");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_library_exact(
                &PortableRelativePath::new("example/processor.jar").expect("jar path"),
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
            reauthenticate_step_dependencies(&step, &plan, workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_processor_data_exact(
                &PortableRelativePath::new("patch/client.bin").expect("data path"),
                b"patch",
            )
            .await
            .expect("restore data");

        fs::write(workspace.version_path().join("1.21.5.jar"), b"changed")
            .expect("tamper staged client");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_version_exact(
                &PortableRelativePath::new("1.21.5.jar").expect("version path"),
                b"client",
            )
            .await
            .expect("restore client");

        fs::write(workspace.installer_path(), b"changed").expect("tamper installer");
        assert!(matches!(
            reauthenticate_step_dependencies(&step, &plan, workspace, &authority, "1.21.5"),
            Err(BoundProcessorError::Authority)
        ));
        workspace
            .write_installer_exact(b"installer")
            .await
            .expect("restore installer");

        let output_path = PortableRelativePath::new("example/generated.jar").expect("output path");
        let output_artifact = BoundProcessorArtifact {
            coordinate: "example:generated:1".to_string(),
            relative_path: output_path.clone(),
        };
        let output_step = BoundProcessorStep {
            action: BoundProcessorAction::Java,
            jar: BoundProcessorArtifact {
                coordinate: "example:processor:1".to_string(),
                relative_path: PortableRelativePath::new("example/processor.jar")
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
                expectation: BoundProcessorOutputExpectation::ProviderSha1(
                    Sha1::digest(b"generated").into(),
                ),
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
            workspace,
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
                workspace,
                &authority,
                "1.21.5",
            ),
            Err(BoundProcessorError::Authority)
        ));
        owner.cleanup().expect("processor cleanup");
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

    const CONTAINMENT_FIXTURE_ENV: &str = "AXIAL_CONTAINMENT_FIXTURE";
    const CONTAINMENT_FIXTURE_TEST: &str =
        "loaders::bound_processors::tests::contained_child_fixture";

    #[test]
    #[allow(
        clippy::zombie_processes,
        reason = "the fixture intentionally orphans a descendant to verify containment cleanup"
    )]
    fn contained_child_fixture() {
        let Ok(mode) = std::env::var(CONTAINMENT_FIXTURE_ENV) else {
            return;
        };
        match mode.as_str() {
            "exit-7" => std::process::exit(7),
            "wait" => std::thread::sleep(Duration::from_secs(30)),
            "leader-with-descendant" => {
                let mut descendant = std::process::Command::new(
                    std::env::current_exe().expect("current test executable"),
                );
                descendant
                    .args(["--exact", CONTAINMENT_FIXTURE_TEST, "--nocapture"])
                    .env(CONTAINMENT_FIXTURE_ENV, "wait");
                descendant.spawn().expect("fixture descendant");
            }
            other => panic!("unknown containment fixture mode: {other}"),
        }
    }

    fn containment_fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args(["--exact", CONTAINMENT_FIXTURE_TEST, "--nocapture"])
            .env(CONTAINMENT_FIXTURE_ENV, mode);
        command
    }

    #[test]
    fn macos_group_listing_bounds_and_zombie_classification_fail_closed() {
        assert_ne!(super::MACOS_PROC_PIDINFO_FIND_ZOMBIES_ARG, 0);
        assert_eq!(
            super::checked_macos_process_group_listing_len(
                super::MAX_MACOS_PROCESS_GROUP_MEMBERS as i32,
                super::MAX_MACOS_PROCESS_GROUP_MEMBERS + 1,
            )
            .expect("maximum bounded group"),
            super::MAX_MACOS_PROCESS_GROUP_MEMBERS
        );
        assert!(matches!(
            super::checked_macos_process_group_listing_len(-1, 1),
            Err(BoundProcessorError::Unreaped)
        ));
        assert!(matches!(
            super::checked_macos_process_group_listing_len(
                (super::MAX_MACOS_PROCESS_GROUP_MEMBERS + 1) as i32,
                super::MAX_MACOS_PROCESS_GROUP_MEMBERS + 1,
            ),
            Err(BoundProcessorError::Unreaped)
        ));

        let mut observed_member = false;
        assert!(super::record_zombie_group_member(
            &mut observed_member,
            17,
            18,
            false,
        ));
        assert!(!observed_member);
        assert!(super::record_zombie_group_member(
            &mut observed_member,
            17,
            17,
            true,
        ));
        assert!(observed_member);
        assert!(!super::record_zombie_group_member(
            &mut observed_member,
            17,
            17,
            false,
        ));
    }

    #[test]
    #[cfg(unix)]
    fn macos_group_probe_settlement_preserves_fail_closed_branches() {
        fn settle(
            probe: Result<(), rustix::io::Errno>,
            termination: Result<(), BoundProcessorError>,
            proof: Result<bool, BoundProcessorError>,
            calls: &std::cell::Cell<(u8, u8)>,
        ) -> Result<super::MacosGroupSettlement, BoundProcessorError> {
            super::settle_macos_group_probe(
                probe,
                || {
                    let (_, proofs) = calls.get();
                    calls.set((1, proofs));
                    termination
                },
                || {
                    let (terminations, proofs) = calls.get();
                    calls.set((terminations, proofs + 1));
                    proof
                },
            )
        }

        let calls = std::cell::Cell::new((0, 0));
        assert_eq!(
            settle(Err(rustix::io::Errno::SRCH), Ok(()), Ok(true), &calls).expect("missing group"),
            super::MacosGroupSettlement::Empty
        );
        assert_eq!(calls.get(), (0, 0));

        calls.set((0, 0));
        assert_eq!(
            settle(Ok(()), Ok(()), Ok(true), &calls).expect("signalable zombie group"),
            super::MacosGroupSettlement::OnlyZombies
        );
        assert_eq!(calls.get(), (1, 1));

        calls.set((0, 0));
        assert_eq!(
            settle(Err(rustix::io::Errno::PERM), Ok(()), Ok(true), &calls).expect("zombie proof"),
            super::MacosGroupSettlement::OnlyZombies
        );
        assert_eq!(calls.get(), (0, 1));

        calls.set((0, 0));
        assert_eq!(
            settle(Err(rustix::io::Errno::PERM), Ok(()), Ok(false), &calls,)
                .expect("live member remains"),
            super::MacosGroupSettlement::LiveMembers
        );
        assert_eq!(calls.get(), (0, 1));

        calls.set((0, 0));
        assert!(matches!(
            settle(Err(rustix::io::Errno::INVAL), Ok(()), Ok(true), &calls,),
            Err(BoundProcessorError::Unreaped)
        ));
        assert_eq!(calls.get(), (0, 0));

        calls.set((0, 0));
        assert!(matches!(
            settle(
                Ok(()),
                Err(BoundProcessorError::Containment),
                Ok(true),
                &calls,
            ),
            Err(BoundProcessorError::Containment)
        ));
        assert_eq!(calls.get(), (1, 0));

        calls.set((0, 0));
        assert!(matches!(
            settle(
                Err(rustix::io::Errno::PERM),
                Ok(()),
                Err(BoundProcessorError::Cleanup),
                &calls,
            ),
            Err(BoundProcessorError::Cleanup)
        ));
        assert_eq!(calls.get(), (0, 1));
    }

    #[tokio::test]
    async fn contained_nonzero_cancel_and_output_limit_are_reaped() {
        let mut command = containment_fixture_command("exit-7");
        let mut nonzero = spawn_contained_child(&mut command, None)
            .await
            .expect("nonzero child");
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        pipe_tx.send(PipeEvent::Finished).unwrap();
        pipe_tx.send(PipeEvent::Finished).unwrap();
        assert!(matches!(
            wait_for_contained_child(
                &mut nonzero,
                &mut cancel_rx,
                &mut pipe_rx,
                None,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await,
            Err(BoundProcessorError::Unsuccessful)
        ));
        assert!(nonzero.child.try_wait().expect("nonzero wait").is_some());

        let mut command = containment_fixture_command("wait");
        let mut cancelled = spawn_contained_child(&mut command, None)
            .await
            .expect("cancelled child");
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        cancel_tx.send(()).expect("cancel signal");
        let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        pipe_tx.send(PipeEvent::Finished).unwrap();
        pipe_tx.send(PipeEvent::Finished).unwrap();
        assert!(matches!(
            wait_for_contained_child(
                &mut cancelled,
                &mut cancel_rx,
                &mut pipe_rx,
                None,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await,
            Err(BoundProcessorError::Cancelled)
        ));
        assert!(cancelled.child.try_wait().expect("cancel wait").is_some());

        let mut command = containment_fixture_command("wait");
        let mut flooded = spawn_contained_child(&mut command, None)
            .await
            .expect("flood child");
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        pipe_tx.send(PipeEvent::Limit).expect("limit event");
        assert!(matches!(
            wait_for_contained_child(
                &mut flooded,
                &mut cancel_rx,
                &mut pipe_rx,
                None,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await,
            Err(BoundProcessorError::OutputLimit)
        ));
        assert!(flooded.child.try_wait().expect("limit wait").is_some());
    }

    #[tokio::test]
    async fn contained_successful_leader_exit_terminates_surviving_descendants() {
        let attempts = if cfg!(target_os = "macos") { 4 } else { 1 };
        for attempt in 0..attempts {
            let mut command = containment_fixture_command("leader-with-descendant");
            let mut child = spawn_contained_child(&mut command, None)
                .await
                .expect("contained child");
            let (_cancel_tx, mut cancel_rx) = oneshot::channel();
            let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
            pipe_tx.send(PipeEvent::Finished).unwrap();
            pipe_tx.send(PipeEvent::Finished).unwrap();

            wait_for_contained_child(
                &mut child,
                &mut cancel_rx,
                &mut pipe_rx,
                None,
                Duration::from_secs(2),
                Duration::from_millis(50),
            )
            .await
            .unwrap_or_else(|error| panic!("contained success attempt {attempt}: {error:?}"));
            assert!(child.child.try_wait().expect("leader wait").is_some());
            assert!(
                super::process_containment_is_empty(&child.containment)
                    .await
                    .expect("empty process group")
            );
        }
    }

    #[tokio::test]
    async fn contained_tree_timeout_is_reaped() {
        let mut command = containment_fixture_command("wait");
        let mut child = spawn_contained_child(&mut command, None)
            .await
            .expect("timeout child");
        let (_cancel_tx, mut cancel_rx) = oneshot::channel();
        let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
        pipe_tx.send(PipeEvent::Finished).unwrap();
        pipe_tx.send(PipeEvent::Finished).unwrap();
        assert!(matches!(
            wait_for_contained_child(
                &mut child,
                &mut cancel_rx,
                &mut pipe_rx,
                None,
                Duration::from_millis(20),
                Duration::from_millis(20),
            )
            .await,
            Err(BoundProcessorError::Timeout)
        ));
        assert!(child.child.try_wait().expect("timeout wait").is_some());
    }
}
