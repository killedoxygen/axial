use super::path_safety::filesystem_path;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::fs as async_fs;

pub(super) async fn sweep_stale_promotion_backups(destination: &Path) -> io::Result<()> {
    let Some(prefix) = promotion_backup_file_name_prefix(destination) else {
        return Ok(());
    };
    sweep_stale_owned_slots(destination, &prefix).await
}

pub(super) fn promotion_backup_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("artifact"))
        .to_os_string();
    name.push(".axial-backup-");
    name.push(std::process::id().to_string());
    destination.with_file_name(name)
}

async fn sweep_stale_owned_slots(destination: &Path, prefix: &str) -> io::Result<()> {
    let Some(parent) = destination.parent() else {
        return Ok(());
    };
    let current_pid = std::process::id();
    let mut system = System::new();
    let mut entries = match async_fs::read_dir(filesystem_path(parent).as_ref()).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy();
        let Some(pid) = name.strip_prefix(prefix).and_then(promotion_slot_owner_pid) else {
            continue;
        };
        if pid == current_pid || promotion_slot_owner_is_live(&mut system, pid) {
            continue;
        }
        let metadata = match async_fs::symlink_metadata(filesystem_path(&path).as_ref()).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_file() || metadata.file_type().is_symlink() {
            match async_fs::remove_file(filesystem_path(&path).as_ref()).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn promotion_slot_owner_pid(pid: &str) -> Option<u32> {
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

fn promotion_slot_owner_is_live(system: &mut System, pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    system.process(pid).is_some()
}

fn promotion_backup_file_name_prefix(destination: &Path) -> Option<String> {
    let file_name = destination.file_name()?.to_string_lossy();
    Some(format!("{file_name}.axial-backup-"))
}
