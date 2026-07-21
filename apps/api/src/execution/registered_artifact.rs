//! Confined verification and mutation of exact registered launcher-managed artifacts.

use super::{AnchoredLeaf, AnchoredRegularFile};
use crate::execution::file::file_fact;
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::state::contracts::{OperationId, TargetDescriptor};
use axial_minecraft::known_good::KnownGoodPhysicalPath;
use futures_util::StreamExt;
use reqwest::Client;
use sha1::{Digest as _, Sha1};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) struct RegisteredArtifactMutationCapability {
    inner: AnchoredLeaf,
}

/// Fresh, read-only authority to verify one exact registered artifact leaf once.
pub(crate) struct RegisteredArtifactExactVerifier {
    inner: AnchoredLeaf,
    expected_sha1: String,
    expected_size: u64,
    identity: Arc<()>,
}

pub(crate) struct RegisteredArtifactExactVerification {
    identity: Arc<()>,
}

pub(crate) struct RegisteredArtifactExactProof {
    confined: AnchoredLeaf,
    verified: AnchoredRegularFile,
    identity: Arc<()>,
    #[cfg(test)]
    lifetime: Arc<()>,
}

pub(crate) struct RegisteredArtifactMutationReport {
    pub(crate) facts: Vec<ExecutionFact>,
}

pub(crate) struct RegisteredArtifactMutationError {
    pub(crate) facts: Vec<ExecutionFact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredArtifactPhysicalState {
    Missing,
    Exact,
    Corrupt,
}

impl RegisteredArtifactMutationCapability {
    pub(crate) async fn mint(path: KnownGoodPhysicalPath) -> io::Result<Self> {
        #[cfg(any(unix, windows))]
        {
            let (root, relative) = physical_path_parts(path);
            tokio::task::spawn_blocking(move || {
                AnchoredLeaf::open(&root, &relative).map(|inner| Self { inner })
            })
            .await
            .map_err(|error| io::Error::other(error.to_string()))?
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "registered artifact confined mutation is unavailable on this platform",
            ))
        }
    }

    pub(crate) fn is_current(&self) -> bool {
        self.inner.revalidate().is_ok()
    }

    pub(crate) fn target_is_missing(&self) -> bool {
        self.inner.target_is_missing().unwrap_or(false)
    }

    pub(crate) fn quarantine_existing(
        &self,
        operation_id: &OperationId,
        target: &TargetDescriptor,
    ) -> Result<RegisteredArtifactMutationReport, RegisteredArtifactMutationError> {
        self.inner
            .quarantine_existing()
            .map(|()| RegisteredArtifactMutationReport {
                facts: vec![file_fact(
                    ExecutionFactKind::FileQuarantined,
                    Some(operation_id.clone()),
                    target,
                )],
            })
            .map_err(|error| mutation_error(error, operation_id, target))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_verify_promote(
        &self,
        operation_id: &OperationId,
        target: &TargetDescriptor,
        provider_url: &str,
        expected_sha1: &str,
        expected_size: u64,
        client: &Client,
    ) -> Result<RegisteredArtifactMutationReport, RegisteredArtifactMutationError> {
        #[cfg(any(unix, windows))]
        {
            if !self.is_current() || !self.target_is_missing() {
                return Err(mutation_error(
                    io::Error::new(io::ErrorKind::PermissionDenied, "confined leaf changed"),
                    operation_id,
                    target,
                ));
            }
            let mut temp = self
                .inner
                .create_temp()
                .map_err(|error| mutation_error(error, operation_id, target))?;
            let temp_file = temp.take_writer().ok_or_else(|| {
                mutation_error(
                    io::Error::other("confined temp writer is unavailable"),
                    operation_id,
                    target,
                )
            })?;
            let result = stream_exact_artifact(
                temp_file,
                provider_url,
                expected_sha1,
                expected_size,
                client,
            )
            .await;
            if let Err(kind) = result {
                self.inner.remove_temp(temp);
                return Err(RegisteredArtifactMutationError {
                    facts: vec![execution_fact(kind, operation_id, target)],
                });
            }
            if !self.is_current() || !self.target_is_missing() {
                self.inner.remove_temp(temp);
                return Err(mutation_error(
                    io::Error::new(io::ErrorKind::PermissionDenied, "confined leaf changed"),
                    operation_id,
                    target,
                ));
            }
            if let Err(error) = self.inner.promote_temp(&temp) {
                self.inner.remove_temp(temp);
                return Err(mutation_error(error, operation_id, target));
            }
            if !self.is_current() {
                return Err(mutation_error(
                    io::Error::new(io::ErrorKind::PermissionDenied, "confined parent changed"),
                    operation_id,
                    target,
                ));
            }
            Ok(RegisteredArtifactMutationReport {
                facts: vec![
                    execution_fact(
                        ExecutionFactKind::DownloadWrittenToTemp,
                        operation_id,
                        target,
                    ),
                    execution_fact(ExecutionFactKind::DownloadPromoted, operation_id, target),
                ],
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (provider_url, expected_sha1, expected_size, client);
            Err(mutation_error(
                io::Error::new(io::ErrorKind::Unsupported, "confined download unavailable"),
                operation_id,
                target,
            ))
        }
    }

    pub(crate) async fn verify_exact(&self, expected_sha1: &str, expected_size: u64) -> bool {
        #[cfg(any(unix, windows))]
        {
            if !self.is_current() {
                return false;
            }
            let expected_sha1 = expected_sha1.to_string();
            let Ok(Some(verification)) = self.inner.open_regular() else {
                return false;
            };
            let verified = tokio::task::spawn_blocking(move || {
                verification.verify_sha1(&expected_sha1, expected_size)
            })
            .await
            .is_ok_and(|verified| verified.is_some());
            verified && self.is_current()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (expected_sha1, expected_size);
            false
        }
    }

    pub(crate) async fn classify(
        &self,
        expected_sha1: &str,
        expected_size: u64,
    ) -> Option<RegisteredArtifactPhysicalState> {
        if !self.is_current() {
            return None;
        }
        if self.verify_exact(expected_sha1, expected_size).await {
            return Some(RegisteredArtifactPhysicalState::Exact);
        }
        if !self.is_current() {
            return None;
        }
        if self.target_is_missing() {
            Some(RegisteredArtifactPhysicalState::Missing)
        } else {
            Some(RegisteredArtifactPhysicalState::Corrupt)
        }
    }
}

impl RegisteredArtifactExactVerifier {
    pub(crate) async fn mint(
        path: KnownGoodPhysicalPath,
        expected_sha1: String,
        expected_size: u64,
    ) -> io::Result<(Self, RegisteredArtifactExactVerification)> {
        let identity = Arc::new(());
        #[cfg(any(unix, windows))]
        {
            let (root, relative) = physical_path_parts(path);
            let inner = tokio::task::spawn_blocking(move || AnchoredLeaf::open(&root, &relative))
                .await
                .map_err(|error| io::Error::other(error.to_string()))??;
            Ok((
                Self {
                    inner,
                    expected_sha1,
                    expected_size,
                    identity: identity.clone(),
                },
                RegisteredArtifactExactVerification { identity },
            ))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (path, expected_sha1, expected_size, identity);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "registered artifact exact verification is unavailable on this platform",
            ))
        }
    }

    pub(crate) async fn verify(self) -> Result<RegisteredArtifactExactProof, ()> {
        #[cfg(any(unix, windows))]
        {
            if self.inner.revalidate().is_err() {
                return Err(());
            }
            let Some(verification) = self.inner.open_regular().ok().flatten() else {
                return Err(());
            };
            let expected_sha1 = self.expected_sha1;
            let expected_size = self.expected_size;
            let verified = tokio::task::spawn_blocking(move || {
                verification.verify_sha1(&expected_sha1, expected_size)
            })
            .await
            .ok()
            .flatten();
            if let Some(verified) = verified
                && self.inner.revalidate().is_ok()
                && verified.revalidate()
            {
                return Ok(RegisteredArtifactExactProof {
                    confined: self.inner,
                    verified,
                    identity: self.identity,
                    #[cfg(test)]
                    lifetime: Arc::new(()),
                });
            }
            Err(())
        }
        #[cfg(not(any(unix, windows)))]
        Err(())
    }
}

impl RegisteredArtifactExactVerification {
    pub(crate) fn matches(&self, proof: &RegisteredArtifactExactProof) -> bool {
        Arc::ptr_eq(&self.identity, &proof.identity) && proof.is_current()
    }
}

impl RegisteredArtifactExactProof {
    #[cfg(test)]
    pub(crate) fn lifetime_for_test(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.lifetime)
    }

    fn is_current(&self) -> bool {
        self.confined.revalidate().is_ok() && self.verified.revalidate()
    }
}

fn physical_path_parts(path: KnownGoodPhysicalPath) -> (PathBuf, PathBuf) {
    (path.root().to_path_buf(), path.relative().to_path_buf())
}

fn mutation_error(
    error: io::Error,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => ExecutionFactKind::FileMissing,
        io::ErrorKind::PermissionDenied => ExecutionFactKind::FilePermissionDenied,
        _ => ExecutionFactKind::PrimitiveRefused,
    };
    RegisteredArtifactMutationError {
        facts: vec![file_fact(kind, Some(operation_id.clone()), target)],
    }
}

fn execution_fact(
    kind: ExecutionFactKind,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> ExecutionFact {
    file_fact(kind, Some(operation_id.clone()), target)
}

#[cfg(any(unix, windows))]
async fn stream_exact_artifact(
    file: std::fs::File,
    provider_url: &str,
    expected_sha1: &str,
    expected_size: u64,
    client: &Client,
) -> Result<(), ExecutionFactKind> {
    use tokio::io::AsyncWriteExt as _;

    let response = client
        .get(provider_url)
        .send()
        .await
        .map_err(|_| ExecutionFactKind::DownloadNetworkFailure)?;
    if !response.status().is_success() {
        return Err(ExecutionFactKind::DownloadProviderFailure);
    }
    if response
        .content_length()
        .is_some_and(|length| length != expected_size)
    {
        return Err(ExecutionFactKind::DownloadSizeMismatch);
    }
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut observed = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ExecutionFactKind::DownloadInterrupted)?;
        observed = observed
            .checked_add(chunk.len() as u64)
            .ok_or(ExecutionFactKind::DownloadSizeMismatch)?;
        if observed > expected_size {
            return Err(ExecutionFactKind::DownloadSizeMismatch);
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| ExecutionFactKind::DownloadTempWriteFailed)?;
        hasher.update(&chunk);
    }
    if observed != expected_size {
        return Err(ExecutionFactKind::DownloadSizeMismatch);
    }
    if format!("{:x}", hasher.finalize()) != expected_sha1 {
        return Err(ExecutionFactKind::DownloadChecksumMismatch);
    }
    file.flush()
        .await
        .map_err(|_| ExecutionFactKind::DownloadTempWriteFailed)?;
    file.sync_all()
        .await
        .map_err(|_| ExecutionFactKind::DownloadTempWriteFailed)
}

#[cfg(all(test, unix))]
mod tests {
    use super::RegisteredArtifactMutationCapability;
    use crate::state::contracts::{
        OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::thread;

    #[tokio::test]
    async fn zero_byte_download_is_verified_and_promoted() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind zero-byte artifact server");
        let address = listener.local_addr().expect("zero-byte server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept zero-byte request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write zero-byte response");
        });
        let base =
            std::env::temp_dir().join(format!("axial-zero-byte-artifact-{}", uuid::Uuid::new_v4()));
        let relative = PathBuf::from("assets/objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709");
        fs::create_dir_all(base.join(relative.parent().expect("zero-byte artifact parent")))
            .expect("create zero-byte artifact parent");
        let capability = RegisteredArtifactMutationCapability::mint(
            KnownGoodPhysicalPath::for_test(base.clone(), relative.clone()),
        )
        .await
        .expect("mint zero-byte artifact capability");
        let operation_id = OperationId::deterministic_test("zero-byte-artifact-promotion");
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "zero-byte-artifact-promotion",
            OwnershipClass::LauncherManaged,
        );

        let result = capability
            .download_verify_promote(
                &operation_id,
                &target,
                &format!("http://{address}/artifact"),
                "da39a3ee5e6b4b0d3255bfef95601890afd80709",
                0,
                &reqwest::Client::new(),
            )
            .await;
        assert!(result.is_ok(), "download and promote zero-byte artifact");

        assert_eq!(
            fs::metadata(base.join(&relative))
                .expect("promoted zero-byte artifact")
                .len(),
            0
        );
        assert!(
            capability
                .verify_exact("da39a3ee5e6b4b0d3255bfef95601890afd80709", 0)
                .await
        );
        server.join().expect("join zero-byte artifact server");
        fs::remove_dir_all(&base).expect("remove zero-byte artifact fixture");
    }

    #[tokio::test]
    async fn ancestor_swap_cannot_redirect_quarantine_outside_the_held_root() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-confinement-{}",
            uuid::Uuid::new_v4()
        ));
        let managed_root = base.join("managed");
        let detached_root = base.join("detached-managed");
        let outside_root = base.join("outside");
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(managed_root.join("libraries/example"))
            .expect("create managed artifact parent");
        fs::create_dir_all(outside_root.join("libraries/example"))
            .expect("create outside artifact parent");
        fs::write(managed_root.join(&relative), b"managed-corrupt")
            .expect("write managed artifact");
        fs::write(outside_root.join(&relative), b"user-owned").expect("write outside artifact");

        let capability = RegisteredArtifactMutationCapability::mint(
            KnownGoodPhysicalPath::for_test(managed_root.clone(), relative.clone()),
        )
        .await
        .expect("mint confined mutation capability");
        fs::rename(&managed_root, &detached_root).expect("detach held managed root");
        symlink(&outside_root, &managed_root).expect("redirect configured root");

        let result = capability.quarantine_existing(
            &OperationId::deterministic_test("registered-artifact-confinement-test"),
            &TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "registered-artifact-confinement-test",
                OwnershipClass::LauncherManaged,
            ),
        );

        assert!(result.is_err(), "ancestor drift must fail closed");
        assert_eq!(
            fs::read(outside_root.join(&relative)).expect("read outside artifact"),
            b"user-owned"
        );
        assert_eq!(
            fs::read(detached_root.join(&relative)).expect("read held managed artifact"),
            b"managed-corrupt"
        );
        assert!(
            fs::read_dir(outside_root.join("libraries/example"))
                .expect("read outside artifact parent")
                .all(|entry| !entry
                    .expect("outside directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-quarantine-"))
        );

        fs::remove_file(&managed_root).expect("remove redirected root link");
        fs::remove_dir_all(&base).expect("remove confinement fixture");
    }

    #[tokio::test]
    async fn hard_link_alias_prevents_registered_artifact_quarantine() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-hard-link-{}",
            uuid::Uuid::new_v4()
        ));
        let relative = PathBuf::from("libraries/example/leaf.jar");
        let source = base.join(&relative);
        let alias = base.join("libraries/example/alias.jar");
        fs::create_dir_all(source.parent().expect("managed artifact parent"))
            .expect("create managed artifact parent");
        fs::write(&source, b"managed-corrupt").expect("write managed artifact");
        fs::hard_link(&source, &alias).expect("create managed artifact alias");

        let capability = RegisteredArtifactMutationCapability::mint(
            KnownGoodPhysicalPath::for_test(base.clone(), relative),
        )
        .await
        .expect("mint confined mutation capability");
        let result = capability.quarantine_existing(
            &OperationId::deterministic_test("registered-artifact-hard-link-test"),
            &TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "registered-artifact-hard-link-test",
                OwnershipClass::LauncherManaged,
            ),
        );

        assert!(result.is_err(), "hard-linked artifact must fail closed");
        assert_eq!(fs::read(&source).expect("read source"), b"managed-corrupt");
        assert_eq!(fs::read(&alias).expect("read alias"), b"managed-corrupt");
        assert!(
            fs::read_dir(source.parent().expect("managed artifact parent"))
                .expect("read managed artifact parent")
                .all(|entry| !entry
                    .expect("managed artifact entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-quarantine-"))
        );

        fs::remove_dir_all(&base).expect("remove hard-link fixture");
    }
}
