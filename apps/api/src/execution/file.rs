//! Execution-owned file capabilities.
//!
//! These helpers perform bounded file effects and emit structured facts. They
//! do not decide product policy or Guardian repair behavior.

use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{OperationId, OwnershipClass, TargetDescriptor};
use crate::state::ownership::protection_for;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct FileWriteRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub destination: &'a Path,
    pub contents: &'a [u8],
}

impl<'a> FileWriteRequest<'a> {
    pub fn new(target: TargetDescriptor, destination: &'a Path, contents: &'a [u8]) -> Self {
        Self {
            operation_id: None,
            target,
            destination,
            contents,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PromoteTempFileRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub temp_path: &'a Path,
    pub destination: &'a Path,
}

impl<'a> PromoteTempFileRequest<'a> {
    pub fn new(target: TargetDescriptor, temp_path: &'a Path, destination: &'a Path) -> Self {
        Self {
            operation_id: None,
            target,
            temp_path,
            destination,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeleteFileRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub path: &'a Path,
}

impl<'a> DeleteFileRequest<'a> {
    pub fn new(target: TargetDescriptor, path: &'a Path) -> Self {
        Self {
            operation_id: None,
            target,
            path,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileCapabilityReport {
    pub target: TargetDescriptor,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Debug)]
pub struct FileCapabilityError {
    pub kind: FileCapabilityErrorKind,
    pub facts: Vec<ExecutionFact>,
    source: Option<io::Error>,
}

impl FileCapabilityError {
    fn new(kind: FileCapabilityErrorKind, facts: Vec<ExecutionFact>) -> Self {
        Self {
            kind,
            facts,
            source: None,
        }
    }

    fn with_source(
        kind: FileCapabilityErrorKind,
        facts: Vec<ExecutionFact>,
        source: io::Error,
    ) -> Self {
        Self {
            kind,
            facts,
            source: Some(source),
        }
    }

    pub fn io_kind(&self) -> io::ErrorKind {
        self.source
            .as_ref()
            .map(io::Error::kind)
            .unwrap_or(io::ErrorKind::Other)
    }
}

impl fmt::Display for FileCapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            FileCapabilityErrorKind::OwnershipUnknown => {
                formatter.write_str("file capability refused unknown ownership")
            }
            FileCapabilityErrorKind::OwnershipRefused => {
                formatter.write_str("file capability refused target ownership")
            }
            FileCapabilityErrorKind::UnsupportedSource => {
                formatter.write_str("file capability refused unsupported source type")
            }
            FileCapabilityErrorKind::CreateParentFailed => {
                formatter.write_str("file capability failed to create parent directory")
            }
            FileCapabilityErrorKind::TempWriteFailed => {
                formatter.write_str("file capability failed to write temporary file")
            }
            FileCapabilityErrorKind::PromoteFailed => {
                formatter.write_str("file capability failed to promote temporary file")
            }
            FileCapabilityErrorKind::DeleteFailed => {
                formatter.write_str("file capability failed to delete source")
            }
        }
    }
}

impl std::error::Error for FileCapabilityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

impl From<FileCapabilityError> for io::Error {
    fn from(error: FileCapabilityError) -> Self {
        io::Error::new(error.io_kind(), error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileCapabilityErrorKind {
    OwnershipUnknown,
    OwnershipRefused,
    UnsupportedSource,
    CreateParentFailed,
    TempWriteFailed,
    PromoteFailed,
    DeleteFailed,
}

pub fn write_file_atomically(
    request: FileWriteRequest<'_>,
) -> Result<FileCapabilityReport, FileCapabilityError> {
    let mut facts = Vec::new();
    validate_managed_ownership(&request.target, request.operation_id.as_ref(), &mut facts)?;

    if let Some(parent) = request.destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            let mut error_facts = facts.clone();
            error_facts.push(io_error_fact(
                error.kind(),
                request.operation_id.clone(),
                &request.target,
            ));
            FileCapabilityError::with_source(
                FileCapabilityErrorKind::CreateParentFailed,
                error_facts,
                error,
            )
        })?;
    }

    let temp_path = atomic_temp_path_for(request.destination);
    if temp_path.exists() {
        facts.push(file_fact(
            ExecutionFactKind::FileTempLeftover,
            request.operation_id.clone(),
            &request.target,
        ));
    }
    fs::write(&temp_path, request.contents).map_err(|error| {
        let mut error_facts = facts.clone();
        error_facts.push(io_error_fact(
            error.kind(),
            request.operation_id.clone(),
            &request.target,
        ));
        FileCapabilityError::with_source(
            FileCapabilityErrorKind::TempWriteFailed,
            error_facts,
            error,
        )
    })?;
    facts.push(file_fact(
        ExecutionFactKind::FileWrittenToTemp,
        request.operation_id.clone(),
        &request.target,
    ));

    let promote_report = promote_temp_file(PromoteTempFileRequest {
        operation_id: request.operation_id,
        target: request.target.clone(),
        temp_path: &temp_path,
        destination: request.destination,
    })?;
    facts.extend(promote_report.facts);

    Ok(FileCapabilityReport {
        target: request.target,
        facts,
    })
}

pub fn promote_temp_file(
    request: PromoteTempFileRequest<'_>,
) -> Result<FileCapabilityReport, FileCapabilityError> {
    let mut facts = Vec::new();
    validate_managed_ownership(&request.target, request.operation_id.as_ref(), &mut facts)?;

    match replace_file_atomically(request.temp_path, request.destination) {
        Ok(()) => {
            facts.push(file_fact(
                ExecutionFactKind::FilePromoted,
                request.operation_id.clone(),
                &request.target,
            ));
            Ok(FileCapabilityReport {
                target: request.target,
                facts,
            })
        }
        Err(first_error) if !request.temp_path.exists() => {
            facts.push(file_fact(
                ExecutionFactKind::FileMissing,
                request.operation_id.clone(),
                &request.target,
            ));
            Err(FileCapabilityError::with_source(
                FileCapabilityErrorKind::PromoteFailed,
                facts,
                first_error,
            ))
        }
        Err(error) => {
            let mut error_facts = facts;
            error_facts.push(io_error_fact(
                error.kind(),
                request.operation_id,
                &request.target,
            ));
            Err(FileCapabilityError::with_source(
                FileCapabilityErrorKind::PromoteFailed,
                error_facts,
                error,
            ))
        }
    }
}

pub fn delete_launcher_managed_file(
    request: DeleteFileRequest<'_>,
) -> Result<FileCapabilityReport, FileCapabilityError> {
    let mut facts = Vec::new();
    validate_managed_ownership(&request.target, request.operation_id.as_ref(), &mut facts)?;

    let metadata = match fs::symlink_metadata(request.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            facts.push(file_fact(
                ExecutionFactKind::FileMissing,
                request.operation_id,
                &request.target,
            ));
            return Ok(FileCapabilityReport {
                target: safe_target_descriptor(&request.target),
                facts,
            });
        }
        Err(error) => {
            facts.push(io_error_fact(
                error.kind(),
                request.operation_id.clone(),
                &request.target,
            ));
            return Err(FileCapabilityError::with_source(
                FileCapabilityErrorKind::DeleteFailed,
                facts,
                error,
            ));
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        facts.push(file_fact(
            ExecutionFactKind::PrimitiveRefused,
            request.operation_id,
            &request.target,
        ));
        return Err(FileCapabilityError::new(
            FileCapabilityErrorKind::UnsupportedSource,
            facts,
        ));
    }

    fs::remove_file(request.path).map_err(|error| {
        let mut error_facts = facts.clone();
        error_facts.push(io_error_fact(
            error.kind(),
            request.operation_id,
            &request.target,
        ));
        FileCapabilityError::with_source(FileCapabilityErrorKind::DeleteFailed, error_facts, error)
    })?;

    Ok(FileCapabilityReport {
        target: safe_target_descriptor(&request.target),
        facts,
    })
}

pub fn file_fact(
    kind: ExecutionFactKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
) -> ExecutionFact {
    let target = safe_target_descriptor(target);
    ExecutionFact {
        operation_id,
        kind,
        target: Some(target.clone()),
        fields: vec![EvidenceField::new(
            "target",
            target.id.clone(),
            EvidenceSensitivity::Public,
        )],
    }
}

fn safe_target_descriptor(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(target.system, target.kind, &target.id, target.ownership)
}

pub(crate) fn validate_managed_ownership(
    target: &TargetDescriptor,
    operation_id: Option<&OperationId>,
    facts: &mut Vec<ExecutionFact>,
) -> Result<(), FileCapabilityError> {
    if protection_for(target.ownership).allows_automatic_managed_mutation() {
        return Ok(());
    }

    if target.ownership == OwnershipClass::Unknown {
        facts.push(file_fact(
            ExecutionFactKind::FileOwnershipUnknown,
            operation_id.cloned(),
            target,
        ));
        return Err(FileCapabilityError::new(
            FileCapabilityErrorKind::OwnershipUnknown,
            facts.clone(),
        ));
    }

    Err(FileCapabilityError::new(
        FileCapabilityErrorKind::OwnershipRefused,
        facts.clone(),
    ))
}

pub(crate) fn io_error_fact(
    kind: io::ErrorKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
) -> ExecutionFact {
    let fact_kind = match kind {
        io::ErrorKind::NotFound => ExecutionFactKind::FileMissing,
        io::ErrorKind::PermissionDenied => ExecutionFactKind::FilePermissionDenied,
        io::ErrorKind::WouldBlock => ExecutionFactKind::FileLocked,
        _ => ExecutionFactKind::PrimitiveRefused,
    };
    file_fact(fact_kind, operation_id, target)
}

pub(crate) fn atomic_temp_path_for(destination: &Path) -> PathBuf {
    destination.with_extension(
        match destination.extension().and_then(|value| value.to_str()) {
            Some(extension) if !extension.is_empty() => format!("{extension}.tmp"),
            _ => "tmp".to_string(),
        },
    )
}

#[cfg(not(windows))]
fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeleteFileRequest, FileCapabilityErrorKind, FileWriteRequest, PromoteTempFileRequest,
        delete_launcher_managed_file, file_fact, promote_temp_file, write_file_atomically,
    };
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn atomic_write_promotes_temp_and_replaces_existing_managed_file() {
        let root = test_root("atomic-write-promotes");
        let destination = root.join("status.json");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"stale").expect("write stale");

        let report = write_file_atomically(FileWriteRequest::new(
            launcher_target("operation_status"),
            &destination,
            b"fresh",
        ))
        .expect("write file");

        assert_eq!(fs::read(&destination).expect("read destination"), b"fresh");
        assert!(!destination.with_extension("json.tmp").exists());
        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FileWrittenToTemp)
        );
        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FilePromoted)
        );

        cleanup(&root);
    }

    #[test]
    fn unknown_ownership_blocks_destructive_write() {
        let root = test_root("unknown-ownership-blocks");
        let destination = root.join("status.json");
        let error = write_file_atomically(FileWriteRequest::new(
            TargetDescriptor::new(
                StabilizationSystem::State,
                TargetKind::FilesystemPath,
                "unknown_status",
                OwnershipClass::Unknown,
            ),
            &destination,
            b"fresh",
        ))
        .expect_err("unknown ownership should block");

        assert_eq!(error.kind, FileCapabilityErrorKind::OwnershipUnknown);
        assert!(!destination.exists());
        assert!(
            error
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FileOwnershipUnknown)
        );

        cleanup(&root);
    }

    #[test]
    fn managed_file_delete_is_idempotent_and_refuses_directories() {
        let root = test_root("managed-delete");
        let path = root.join("status.json");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&path, b"terminal").expect("write status");

        delete_launcher_managed_file(DeleteFileRequest::new(
            launcher_target("operation_status"),
            &path,
        ))
        .expect("delete managed status");
        assert!(!path.exists());

        let missing = delete_launcher_managed_file(DeleteFileRequest::new(
            launcher_target("operation_status"),
            &path,
        ))
        .expect("missing delete is idempotent");
        assert!(
            missing
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FileMissing)
        );

        fs::create_dir(&path).expect("create directory at status path");
        let error = delete_launcher_managed_file(DeleteFileRequest::new(
            launcher_target("operation_status"),
            &path,
        ))
        .expect_err("directory delete is refused");
        assert_eq!(error.kind, FileCapabilityErrorKind::UnsupportedSource);
        assert!(path.is_dir());
        cleanup(&root);
    }

    #[test]
    fn unknown_ownership_blocks_managed_file_delete() {
        let root = test_root("unknown-delete");
        let path = root.join("status.json");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&path, b"terminal").expect("write status");

        let error = delete_launcher_managed_file(DeleteFileRequest::new(
            TargetDescriptor::new(
                StabilizationSystem::State,
                TargetKind::FilesystemPath,
                "unknown_status",
                OwnershipClass::Unknown,
            ),
            &path,
        ))
        .expect_err("unknown ownership should block delete");

        assert_eq!(error.kind, FileCapabilityErrorKind::OwnershipUnknown);
        assert_eq!(fs::read(&path).expect("status retained"), b"terminal");
        assert!(
            error
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FileOwnershipUnknown)
        );
        cleanup(&root);
    }

    #[test]
    fn user_owned_file_delete_is_refused_and_preserves_source() {
        let root = test_root("user-owned-delete");
        let path = root.join("status.json");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&path, b"user-owned").expect("write status");

        let error = delete_launcher_managed_file(DeleteFileRequest::new(
            TargetDescriptor::new(
                StabilizationSystem::State,
                TargetKind::FilesystemPath,
                "user_status",
                OwnershipClass::UserOwned,
            ),
            &path,
        ))
        .expect_err("user-owned file delete should be refused");

        assert_eq!(error.kind, FileCapabilityErrorKind::OwnershipRefused);
        assert_eq!(fs::read(&path).expect("user file retained"), b"user-owned");
        assert!(error.facts.is_empty());
        cleanup(&root);
    }

    #[test]
    fn missing_temp_promotion_preserves_destination() {
        let root = test_root("missing-temp-preserves");
        let destination = root.join("status.json");
        let temp_path = root.join("status.json.tmp");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"existing").expect("write existing");

        let error = promote_temp_file(PromoteTempFileRequest::new(
            launcher_target("operation_status"),
            &temp_path,
            &destination,
        ))
        .expect_err("missing temp should fail");

        assert_eq!(error.kind, FileCapabilityErrorKind::PromoteFailed);
        assert_eq!(
            fs::read(&destination).expect("read destination"),
            b"existing"
        );
        assert!(
            error
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FileMissing)
        );

        cleanup(&root);
    }

    #[test]
    fn failed_promotion_preserves_destination_and_temp() {
        let root = test_root("failed-promotion-preserves");
        let destination = root.join("status.json");
        let temp_path = root.join("status.json.tmp");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&temp_path, b"replacement").expect("write temp");
        fs::create_dir(&destination).expect("create destination directory");

        let error = promote_temp_file(PromoteTempFileRequest::new(
            launcher_target("operation_status"),
            &temp_path,
            &destination,
        ))
        .expect_err("directory destination should fail");

        assert_eq!(error.kind, FileCapabilityErrorKind::PromoteFailed);
        assert!(destination.is_dir());
        assert_eq!(fs::read(&temp_path).expect("read temp"), b"replacement");

        cleanup(&root);
    }

    #[test]
    fn temp_leftover_fact_is_emitted_before_overwrite() {
        let root = test_root("temp-leftover");
        let destination = root.join("status.json");
        let temp_path = destination.with_extension("json.tmp");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&temp_path, b"leftover").expect("write leftover");

        let report = write_file_atomically(FileWriteRequest::new(
            launcher_target("operation_status"),
            &destination,
            b"fresh",
        ))
        .expect("write file");

        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::FileTempLeftover)
        );
        assert_eq!(fs::read(&destination).expect("read destination"), b"fresh");

        cleanup(&root);
    }

    #[test]
    fn file_facts_sanitize_unsafe_target_ids() {
        let target = TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Artifact,
            id: r"C:\Users\Alice\.minecraft\libraries\bad.jar token=secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        };

        let fact = file_fact(ExecutionFactKind::FileQuarantined, None, &target);
        let encoded = serde_json::to_string(&fact).expect("fact json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("target")
        );
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("token"));
        assert!(!lower.contains("secret"));
        assert!(!lower.contains("-xmx"));
    }

    fn launcher_target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::Config,
            id,
            OwnershipClass::LauncherManaged,
        )
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "axial-file-capability-{name}-{}-{nanos}",
            std::process::id()
        ));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        path
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }
}
