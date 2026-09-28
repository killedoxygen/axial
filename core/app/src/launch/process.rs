//! The process capability is retained until the leader is reaped and its tree settles.
//!
//! This module accepts commands only from the private launch session owner. Unix
//! sessions own a fresh process group; Windows sessions own a kill-on-close Job.
//! A successful direct-child wait is deliberately not proof of tree settlement.

use std::io;
use std::process::{ExitStatus, Stdio};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

pub(crate) struct OwnedProcess {
    child: Child,
    tree: ProcessTree,
    status: Option<ExitStatus>,
}

pub(crate) struct ProcessOutput {
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// A spawn error may still own a suspended process whose reap needs retrying.
/// The caller must retain both this process and its admission leases in that case.
pub(crate) enum SpawnError {
    BeforeSpawn(io::Error),
    Unsettled(OwnedProcess, ProcessOutput, io::Error),
}

impl OwnedProcess {
    pub(crate) fn spawn(mut command: Command) -> Result<(Self, ProcessOutput), SpawnError> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.kill_on_drop(true);
        let mut tree = ProcessTree::prepare(&mut command).map_err(SpawnError::BeforeSpawn)?;
        let child = command.spawn().map_err(SpawnError::BeforeSpawn)?;
        let attach = tree.attach(&child);
        let mut owned = Self {
            child,
            tree,
            status: None,
        };
        // These pipes are established above before spawn and cannot be missing.
        let stdout = owned.child.stdout.take().expect("spawned stdout pipe");
        let stderr = owned.child.stderr.take().expect("spawned stderr pipe");
        let output = ProcessOutput { stdout, stderr };
        if let Err(error) = attach {
            let _ = owned.terminate();
            return Err(SpawnError::Unsettled(owned, output, error));
        }
        Ok((owned, output))
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }

    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        // Darwin can return EPERM for a zombie-only process group. Reaping our
        // leader first avoids mistaking that transient state for a live tree.
        self.try_wait()?;
        // An unattached Windows child is still suspended and has no descendants.
        // Kill that child too if assignment failed; never resume it uncontained.
        let tree = self.tree.terminate();
        if self.status.is_none() {
            if let Err(error) = self.child.start_kill() {
                if error.kind() != io::ErrorKind::InvalidInput && self.try_wait()?.is_none() {
                    return Err(error);
                }
            }
        }
        match tree {
            Err(error) => {
                // The process may have exited between try_wait and signaling.
                // Only an exact subsequent disappearance resolves the error;
                // EPERM itself is never proof that descendants are gone.
                if self.try_wait()?.is_some() && self.tree.settled().unwrap_or(false) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Ok(()) => Ok(()),
        }
    }

    pub(crate) fn tree_settled(&mut self) -> io::Result<bool> {
        if self.try_wait()?.is_none() {
            return Ok(false);
        }
        self.tree.settled()
    }
}

#[cfg(unix)]
struct ProcessTree {
    group: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        command.process_group(0);
        Ok(Self { group: None })
    }

    fn attach(&mut self, child: &Child) -> io::Result<()> {
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| io::Error::other("process identity unavailable"))?;
        self.group = Some(group);
        Ok(())
    }

    fn terminate(&self) -> io::Result<()> {
        let Some(group) = self.group else {
            return Ok(());
        };
        // SAFETY: the positive group ID came from our process_group(0) child.
        if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn settled(&mut self) -> io::Result<bool> {
        let Some(group) = self.group else {
            return Ok(true);
        };
        // Signal zero checks the exact retained group without changing it.
        if unsafe { libc::kill(-group, 0) } == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            // Never signal a recycled group ID after observing its disappearance.
            self.group = None;
            Ok(true)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: std::os::windows::io::OwnedHandle,
    attached: bool,
    settled: bool,
}

#[cfg(windows)]
impl ProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
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
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("process identity unavailable"))?;
        if unsafe { AssignProcessToJobObject(self.job.as_raw_handle().cast(), process.cast()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.attached = true;
        // Assignment precedes the first instruction, so no child can escape the job.
        if unsafe { ntapi::ntpsapi::NtResumeProcess(process.cast()) } < 0 {
            return Err(io::Error::other("contained process could not resume"));
        }
        Ok(())
    }

    fn terminate(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
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
        use std::os::windows::io::AsRawHandle;
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

impl Drop for ProcessTree {
    fn drop(&mut self) {
        // Normal completion disarms this capability. Unexpected owner destruction
        // still kills the known group/job, but is never reported as settled.
        let _ = self.terminate();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    async fn settle(process: &mut OwnedProcess) {
        let mut last_error = None;
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                last_error = process.terminate().err();
                // Like the session owner, retry transient kernel failures and
                // require positive tree settlement before releasing anything.
                if process.tree_settled().unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "tree must settle; last termination error: {last_error:?}"
        );
    }

    #[tokio::test]
    async fn process_group_termination_drains_descendant_owned_output() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & printf 'ready\\n'; wait"]);
        let (mut process, mut output) = match OwnedProcess::spawn(command) {
            Ok(value) => value,
            Err(_) => panic!("fixture spawn"),
        };
        let mut ready = [0; 6];
        tokio::time::timeout(Duration::from_secs(5), output.stdout.read_exact(&mut ready))
            .await
            .expect("ready timeout")
            .expect("ready line");
        assert_eq!(&ready, b"ready\n");
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
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & printf 'ready\\n'"]);
        let (mut process, mut output) = match OwnedProcess::spawn(command) {
            Ok(value) => value,
            Err(_) => panic!("fixture spawn"),
        };
        let mut ready = [0; 6];
        tokio::time::timeout(Duration::from_secs(5), output.stdout.read_exact(&mut ready))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while process.try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!process.tree_settled().unwrap());
        settle(&mut process).await;
    }
}
