//! The one owner of contained child process trees.
//!
//! Unix children lead a fresh process group; Windows children start suspended
//! inside a kill-on-close Job. A successful leader wait is never proof that the
//! tree settled, and a group ID is never signalled after it has been seen gone.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

const SETTLE_POLL: Duration = Duration::from_millis(10);

pub struct OwnedProcess {
    child: Child,
    tree: Tree,
    status: Option<ExitStatus>,
}

pub struct ProcessOutput {
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// A spawn error may still own a contained process whose settlement must be
/// retried. The caller retains it, with its admission leases, until it settles.
pub enum SpawnError {
    BeforeSpawn(io::Error),
    Unsettled(OwnedProcess, ProcessOutput, io::Error),
}

impl OwnedProcess {
    pub fn spawn(mut command: Command) -> Result<(Self, ProcessOutput), SpawnError> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut tree = Tree::prepare(&mut command).map_err(SpawnError::BeforeSpawn)?;
        let mut child = command.spawn().map_err(SpawnError::BeforeSpawn)?;
        let attached = tree.attach(&child);
        // Both pipes were configured above and cannot be missing.
        let output = ProcessOutput {
            stdout: child.stdout.take().expect("spawned stdout pipe"),
            stderr: child.stderr.take().expect("spawned stderr pipe"),
        };
        let mut process = Self {
            child,
            tree,
            status: None,
        };
        if let Err(error) = attached {
            let _ = process.terminate();
            return Err(SpawnError::Unsettled(process, output, error));
        }
        Ok((process, output))
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }

    /// Observes the leader's exit. Cancel-safe.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = self.child.wait().await?;
        self.status = Some(status);
        Ok(status)
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        // Reaping the leader first keeps Darwin's EPERM for a zombie-only group
        // from being mistaken for a live tree.
        self.try_wait()?;
        let tree = self.tree.terminate();
        // An unattached Windows child is still suspended and has no descendants;
        // kill it directly and never resume it uncontained.
        if self.status.is_none()
            && let Err(error) = self.child.start_kill()
            && error.kind() != io::ErrorKind::InvalidInput
            && self.try_wait()?.is_none()
        {
            return Err(error);
        }
        // The tree may have exited between the leader wait and the signal; only
        // a subsequent exact settlement resolves the failure.
        match tree {
            Err(error) if self.try_wait()?.is_none() || !self.tree.settled().unwrap_or(false) => {
                Err(error)
            }
            _ => Ok(()),
        }
    }

    pub fn tree_settled(&mut self) -> io::Result<bool> {
        if self.try_wait()?.is_none() {
            return Ok(false);
        }
        self.tree.settled()
    }

    /// Kills the whole tree and reaps it, retrying transient failures without
    /// releasing containment. Callers bound this with their own deadline.
    pub async fn settle(&mut self) {
        loop {
            let _ = self.terminate();
            if matches!(self.tree_settled(), Ok(true)) {
                return;
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }
}

#[cfg(unix)]
struct Tree {
    /// Cleared once the group is seen gone so a recycled ID is never signalled.
    group: Option<rustix::process::Pid>,
}

#[cfg(unix)]
impl Tree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        command.process_group(0);
        Ok(Self { group: None })
    }

    fn attach(&mut self, child: &Child) -> io::Result<()> {
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| io::Error::other("process identity unavailable"))?;
        self.group = Some(group);
        Ok(())
    }

    fn terminate(&mut self) -> io::Result<()> {
        let Some(group) = self.group else {
            return Ok(());
        };
        match rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::SRCH) => {
                self.group = None;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn settled(&mut self) -> io::Result<bool> {
        let Some(group) = self.group else {
            return Ok(true);
        };
        // A member alive at the first proof could fork before dying, so a
        // zombie-only group must be proven twice around a fresh SIGKILL.
        for _ in 0..2 {
            match rustix::process::test_kill_process_group(group) {
                Err(rustix::io::Errno::SRCH) => break,
                Ok(()) => self.terminate()?,
                // Darwin reports EPERM for a group holding only zombies.
                Err(rustix::io::Errno::PERM) => {}
                Err(error) => return Err(error.into()),
            }
            if self.group.is_none() {
                break;
            }
            if !group_has_only_zombies(group)? {
                return Ok(false);
            }
        }
        self.group = None;
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
fn group_has_only_zombies(group: rustix::process::Pid) -> io::Result<bool> {
    const MAX_MEMBERS: usize = 4096;
    // XNU searches zombies for PROC_PIDTBSDINFO only when its argument is nonzero.
    const FIND_ZOMBIES: u64 = 1;
    let group = group.as_raw_nonzero().get();
    // Darwin's null-buffer sizing call reports the system-wide process count,
    // not the group's size, so bound the listing directly.
    let mut pids = vec![0 as libc::pid_t; MAX_MEMBERS + 1];
    let buffer_bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as i32;
    let listed = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), buffer_bytes) };
    let listed = usize::try_from(listed)
        .ok()
        .filter(|listed| *listed < pids.len())
        .ok_or_else(|| io::Error::other("process group listing is unbounded"))?;
    let info_bytes = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let mut observed = false;
    for pid in pids[..listed].iter().copied().filter(|pid| *pid > 0) {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                FIND_ZOMBIES,
                info.as_mut_ptr().cast(),
                info_bytes,
            )
        };
        if read == 0 {
            match rustix::process::Pid::from_raw(pid).map(rustix::process::test_kill_process) {
                None | Some(Err(rustix::io::Errno::SRCH)) => continue,
                Some(Ok(()) | Err(rustix::io::Errno::PERM)) => return Ok(false),
                Some(Err(error)) => return Err(error.into()),
            }
        }
        if read != info_bytes {
            return Err(io::Error::other("process inspection was truncated"));
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_pgid == group as u32 {
            if info.pbi_status != libc::SZOMB {
                return Ok(false);
            }
            observed = true;
        }
    }
    Ok(observed)
}

#[cfg(target_os = "linux")]
fn group_has_only_zombies(group: rustix::process::Pid) -> io::Result<bool> {
    use std::io::Read as _;
    const MAX_STAT_BYTES: u64 = 4096;
    let group = group.as_raw_nonzero().get();
    let proc = std::fs::File::open("/proc")?;
    if rustix::fs::fstatfs(&proc)?.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(io::Error::other("/proc is not procfs"));
    }
    let mut observed = false;
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let mut stat = Vec::new();
        match std::fs::File::open(entry.path().join("stat"))
            .and_then(|file| file.take(MAX_STAT_BYTES + 1).read_to_end(&mut stat))
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let (state, member_group, threads) = (stat.len() as u64 <= MAX_STAT_BYTES)
            .then(|| parse_linux_stat(&stat, pid))
            .flatten()
            .ok_or_else(|| io::Error::other("process status is malformed"))?;
        if member_group == group {
            if state != b'Z' || threads != 1 {
                return Ok(false);
            }
            observed = true;
        }
    }
    Ok(observed)
}

/// Returns (state, process group, thread count) from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn parse_linux_stat(stat: &[u8], expected_pid: i32) -> Option<(u8, i32, u64)> {
    fn decimal<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
        std::str::from_utf8(bytes).ok()?.parse().ok()
    }
    let pid_end = stat.iter().position(|byte| *byte == b' ')?;
    if stat.get(pid_end + 1) != Some(&b'(') || decimal(&stat[..pid_end]) != Some(expected_pid) {
        return None;
    }
    // The command name may contain anything, including ") ".
    let fields_start = stat.windows(2).rposition(|window| window == b") ")? + 2;
    if fields_start <= pid_end + 2 {
        return None;
    }
    let mut fields = stat[fields_start..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let [state] = fields.next()? else {
        return None;
    };
    let process_group = decimal(fields.nth(1)?)?;
    let threads = decimal(fields.nth(14)?)?;
    Some((*state, process_group, threads))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn group_has_only_zombies(_: rustix::process::Pid) -> io::Result<bool> {
    Ok(false)
}

#[cfg(windows)]
struct Tree {
    job: std::os::windows::io::OwnedHandle,
    attached: bool,
    settled: bool,
}

#[cfg(windows)]
impl Tree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
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
            return Err(io::Error::last_os_error());
        }
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        Ok(Self {
            job,
            attached: false,
            settled: false,
        })
    }

    fn attach(&mut self, child: &Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("process identity unavailable"))?;
        if unsafe { AssignProcessToJobObject(self.job.as_raw_handle().cast(), process.cast()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.attached = true;
        // Assignment precedes the first instruction, so no child escapes the job.
        if unsafe { ntapi::ntpsapi::NtResumeProcess(process.cast()) } < 0 {
            return Err(io::Error::other("contained process could not resume"));
        }
        Ok(())
    }

    fn terminate(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if !self.attached || self.settled {
            return Ok(());
        }
        if unsafe { TerminateJobObject(self.job.as_raw_handle().cast(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn settled(&mut self) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };
        if !self.attached || self.settled {
            return Ok(true);
        }
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let read = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle().cast(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        self.settled = accounting.ActiveProcesses == 0;
        Ok(self.settled)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // Unexpected owner destruction still kills the known group or job, but
        // is never reported as settlement.
        let _ = self.terminate();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn spawn_ready(script: &str) -> (OwnedProcess, ProcessOutput) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        let Ok((process, mut output)) = OwnedProcess::spawn(command) else {
            panic!("fixture spawn");
        };
        let mut ready = [0; 6];
        tokio::time::timeout(Duration::from_secs(5), output.stdout.read_exact(&mut ready))
            .await
            .expect("ready timeout")
            .expect("ready line");
        assert_eq!(&ready, b"ready\n");
        (process, output)
    }

    async fn settle(process: &mut OwnedProcess) {
        tokio::time::timeout(Duration::from_secs(10), process.settle())
            .await
            .expect("tree must settle");
    }

    #[tokio::test]
    async fn group_termination_drains_descendant_owned_output() {
        let (mut process, mut output) = spawn_ready("sleep 60 & printf 'ready\\n'; wait").await;
        settle(&mut process).await;
        let mut remainder = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            output.stdout.read_to_end(&mut remainder),
        )
        .await
        .expect("stdout drained")
        .expect("stdout EOF");
        assert!(process.try_wait().unwrap().is_some());
        assert!(process.tree_settled().unwrap());
    }

    #[tokio::test]
    async fn leader_exit_does_not_settle_a_live_descendant() {
        let (mut process, _output) = spawn_ready("sleep 60 & printf 'ready\\n'").await;
        tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!process.tree_settled().unwrap());
        settle(&mut process).await;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parsing_survives_adversarial_process_names() {
        let zombie = b"42 (processor) worker\n\xff) Z 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0";
        assert_eq!(parse_linux_stat(zombie, 42), Some((b'Z', 42, 1)));
        assert_eq!(
            parse_linux_stat(
                b"43 (processor) R 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 2 0",
                43
            ),
            Some((b'R', 42, 2))
        );
        assert_eq!(parse_linux_stat(zombie, 43), None);
        assert_eq!(parse_linux_stat(b"malformed", 42), None);
    }
}
