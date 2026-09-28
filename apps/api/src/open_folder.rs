//! Host file-manager adapter shared by browser and desktop API composition.

use axial_app::resources::folders::{AdmittedFolder, FolderError, FolderOpener, FolderProcess};
use std::{
    io,
    process::{Child, Command, Stdio},
};

pub struct PlatformFolderOpener;

impl FolderOpener for PlatformFolderOpener {
    fn spawn(&self, folder: &AdmittedFolder) -> Result<Box<dyn FolderProcess>, FolderError> {
        let path = folder.checked_path()?;
        let mut command = Command::new(if cfg!(target_os = "windows") {
            "explorer.exe"
        } else if cfg!(target_os = "macos") {
            "/usr/bin/open"
        } else {
            "xdg-open"
        });
        // The absolute projection is one argument. No shell or user-authored
        // executable, switches, URL, or path participates in this command.
        command
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .spawn()
            .map(|child| Box::new(PlatformFolderProcess(child)) as Box<dyn FolderProcess>)
            .map_err(|_| FolderError::Spawn)
    }
}

struct PlatformFolderProcess(Child);

impl FolderProcess for PlatformFolderProcess {
    fn try_wait(&mut self) -> io::Result<bool> {
        self.0.try_wait().map(|status| status.is_some())
    }

    fn terminate(&mut self) -> io::Result<()> {
        if self.0.try_wait()?.is_some() {
            return Ok(());
        }
        self.0.kill()
    }
}
