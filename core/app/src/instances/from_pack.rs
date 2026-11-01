//! Creation requirements taken from the retained pack archive, never from its
//! provider summary. Queue admission remains with installation coordination.

use super::{
    create::{CreateInstanceRequest, CreateTarget, InstanceService, NativeEffect, SetupIntent},
    directory::RegisteredInstance,
    model::{Instance, InstanceError, InstanceResult},
    setup::{CreateInstanceResponse, CreateResultView, SetupService},
};
use crate::content::catalog::ContentService;
use crate::content::install::{ContentMutations, MutationError};
use crate::content::model::{CanonicalId, ProviderId};
use crate::content::packs::{
    MAX_PACK_ARCHIVE_BYTES, PackArchive, PackError, PackIndex, PackPlan, PackResult, PackVersion,
    ResolvedPack, read_local_pack, resolve_pack_version,
};
use crate::files::PortableName;
use crate::install::model::{InstallQueueContentActionRequest, InstallQueueRequest};
use crate::tasks::CancellationToken;
use axial_minecraft::loaders::{build_id_for, installed_version_id_for};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::sync::Arc;

/// The reserved launcher name of a file-imported pack archive kept in the
/// instance until its setup settles. Pack destinations can never use it.
const STORED_ARCHIVE: &str = ".axial-pack-setup.mrpack";

pub(super) fn stored_archive_name() -> PortableName {
    PortableName::new_exact(STORED_ARCHIVE).expect("fixed portable name")
}

#[derive(Clone, Debug, Serialize)]
pub struct ModpackTarget {
    pub canonical_id: CanonicalId,
    pub version_id: String,
    pub name: String,
    pub minecraft: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    pub loader_label: String,
    pub selection_id: String,
}

pub fn target_from_pack_index(
    canonical_id: CanonicalId,
    version_id: String,
    name: String,
    index: &PackIndex,
) -> ModpackTarget {
    let (loader, loader_label, selection_id) = match index.loader.as_ref() {
        Some(loader) => {
            let component = loader.component_id;
            let build_id = build_id_for(component, &index.minecraft, &loader.version);
            (
                Some(component.short_key().to_string()),
                component.display_name().to_string(),
                format!("loader_build|{}|{build_id}", component.as_str()),
            )
        }
        None => (
            None,
            "Vanilla".to_string(),
            format!("vanilla|{}", index.minecraft),
        ),
    };
    ModpackTarget {
        canonical_id,
        version_id,
        name,
        minecraft: index.minecraft.clone(),
        loader,
        loader_label,
        selection_id,
    }
}

/// A local `.mrpack` whose exact bytes are a published Modrinth version's
/// primary file keeps that provider identity, so updates and setup resume use
/// the provider. Any other valid archive becomes a local pack: it is held until
/// creation stores a copy in the instance. Both then use the ordinary modpack path.
pub async fn modpack_file_target(
    service: &ContentService,
    mutations: &ContentMutations,
    bytes: Vec<u8>,
) -> PackResult<ModpackTarget> {
    let bytes: Arc<[u8]> = bytes.into();
    let hash = format!("{:x}", Sha512::digest(bytes.as_ref()));
    let archive = PackArchive::read(bytes.clone())?;
    let published = service
        .identify(std::slice::from_ref(&hash))
        .await
        .map_err(|_| PackError::Provider)?
        .remove(&hash);
    if let Some(identity) = published {
        let canonical_id = CanonicalId::for_project(identity.provider, &identity.project_id);
        let (content, version) = resolve_pack_version(
            service,
            &canonical_id,
            PackVersion::Exact(&identity.version_id),
        )
        .await?;
        if version
            .primary_file()
            .and_then(|file| file.sha512.as_deref())
            == Some(hash.as_str())
        {
            return Ok(target_from_pack_index(
                content.canonical_id,
                version.id,
                content.title,
                archive.index(),
            ));
        }
    }
    drop(archive);
    let pack = read_local_pack(bytes)?;
    let target = target_from_pack_index(
        pack.canonical_id.clone(),
        pack.version_id.clone(),
        pack.name.clone(),
        pack.archive.index(),
    );
    mutations.retain_local_pack(pack);
    Ok(target)
}

/// Private retained creation input. The client selection is only a consistency
/// check; it cannot choose a different loader while importing these pack bytes.
#[derive(Clone, Debug)]
pub struct PreparedPackCreation {
    target: ModpackTarget,
    version_id: String,
    plan: PackPlan,
}

impl PreparedPackCreation {
    pub fn new(
        canonical_id: CanonicalId,
        pack_version_id: String,
        archive: PackArchive,
        requested_selection_id: &str,
    ) -> PackResult<Self> {
        let index = archive.index();
        let target =
            target_from_pack_index(canonical_id, pack_version_id, index.name.clone(), index);
        if requested_selection_id != target.selection_id {
            return Err(PackError::SelectionChanged);
        }
        let version_id = match &index.loader {
            Some(loader) => {
                installed_version_id_for(loader.component_id, &index.minecraft, &loader.version)
                    .map_err(|_| PackError::Invalid("loader coordinate is invalid"))?
            }
            None => index.minecraft.clone(),
        };
        let plan = archive.plan_all(true)?;
        Ok(Self {
            target,
            version_id,
            plan,
        })
    }

    pub fn target(&self) -> &ModpackTarget {
        &self.target
    }
    pub fn installed_version_id(&self) -> &str {
        &self.version_id
    }
    pub fn plan(&self) -> &PackPlan {
        &self.plan
    }
    pub fn into_plan(self) -> PackPlan {
        self.plan
    }
}

/// Retained /instances/modpack request shape. Instance settings are validated
/// again by the instance owner when its reservation is created.
#[derive(Clone, Debug)]
pub struct CreateFromModpackRequest {
    pub canonical_id: String,
    pub version_id: String,
    pub create: CreateInstanceRequest,
}

impl<'de> Deserialize<'de> for CreateFromModpackRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FlatRequest;
        impl<'de> serde::de::Visitor<'de> for FlatRequest {
            type Value = CreateFromModpackRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a modpack identity and flat instance creation fields")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                use serde::de::Error;
                let mut fields = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(M::Error::custom("duplicate modpack creation field"));
                    }
                }
                let canonical_id = serde_json::from_value(
                    fields
                        .remove("canonical_id")
                        .ok_or_else(|| M::Error::missing_field("canonical_id"))?,
                )
                .map_err(M::Error::custom)?;
                let version_id = serde_json::from_value(
                    fields
                        .remove("version_id")
                        .ok_or_else(|| M::Error::missing_field("version_id"))?,
                )
                .map_err(M::Error::custom)?;
                // Reuse the instance owner's settings and unknown-field boundary.
                let create = serde_json::from_value(serde_json::Value::Object(fields))
                    .map_err(M::Error::custom)?;
                Ok(CreateFromModpackRequest {
                    canonical_id,
                    version_id,
                    create,
                })
            }
        }
        deserializer.deserialize_map(FlatRequest)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PackSetupKind {
    Modpack,
}

/// The existing setup intent owns the handoff to content and installation.
/// Archive bytes and filesystem recovery remain with those feature owners.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredPackSetup {
    kind: PackSetupKind,
    canonical_id: CanonicalId,
    version_id: String,
    selection_id: String,
    installed_version_id: String,
    minecraft_version: String,
    loader_key: String,
    archive_fingerprint: String,
    install: InstallQueueRequest,
}

impl StoredPackSetup {
    pub(super) fn prerequisite(&self) -> &InstallQueueRequest {
        &self.install
    }

    pub(super) fn queue_request(&self, instance: &Instance) -> InstallQueueRequest {
        InstallQueueRequest::Content {
            instance_id: instance.id.to_string(),
            label: format!("Setting up {}", instance.name),
            action: InstallQueueContentActionRequest::Modpack {
                canonical_id: self.canonical_id.as_str().to_owned(),
                version_id: self.version_id.clone(),
                selected_file_ids: Vec::new(),
                include_overrides: true,
            },
        }
    }

    fn new(
        prepared: &PreparedPackCreation,
        target: &CreateTarget,
        install: InstallQueueRequest,
    ) -> InstanceResult<Self> {
        if prepared.installed_version_id() != target.version_id()
            || prepared.target().selection_id != target.selection_id()
            || prepared.target().minecraft != target.minecraft_version()
            || prepared.target().loader.as_deref().unwrap_or("vanilla") != target.loader_key()
        {
            return Err(InstanceError::Conflict);
        }
        Ok(Self {
            kind: PackSetupKind::Modpack,
            canonical_id: prepared.target().canonical_id.clone(),
            version_id: prepared.target().version_id.clone(),
            selection_id: prepared.target().selection_id.clone(),
            installed_version_id: target.version_id().to_owned(),
            minecraft_version: target.minecraft_version().to_owned(),
            loader_key: target.loader_key().to_owned(),
            archive_fingerprint: prepared.plan().fingerprint().to_owned(),
            install,
        })
    }

    fn check_prepared(&self, prepared: &PreparedPackCreation) -> InstanceResult<()> {
        let target = prepared.target();
        if target.canonical_id != self.canonical_id
            || target.version_id != self.version_id
            || target.selection_id != self.selection_id
            || target.minecraft != self.minecraft_version
            || target.loader.as_deref().unwrap_or("vanilla") != self.loader_key
            || prepared.installed_version_id() != self.installed_version_id
            || prepared.plan().fingerprint() != self.archive_fingerprint
        {
            return Err(InstanceError::Conflict);
        }
        Ok(())
    }
}

impl SetupService {
    /// Resolve and authenticate before reserving an identity. Once accepted,
    /// instance creation, pack publication and queue handoff outlive the caller.
    pub async fn create_from_modpack(
        self: &Arc<Self>,
        request: CreateFromModpackRequest,
    ) -> InstanceResult<CreateInstanceResponse> {
        if request.version_id.is_empty() || request.version_id.len() > 256 {
            return Err(InstanceError::InvalidInput);
        }
        let (content, mutations) = self
            .content
            .as_ref()
            .ok_or(InstanceError::SetupUnavailable)?;
        let pack = mutations
            .resolve_pack(
                content,
                &CanonicalId(request.canonical_id),
                PackVersion::Exact(&request.version_id),
            )
            .await
            .map_err(pack_mutation_error)?;
        let archive = (pack.canonical_id.provider() == Some(ProviderId::Local))
            .then(|| pack.archive.bytes().clone());
        let prepared = PreparedPackCreation::new(
            pack.canonical_id,
            pack.version_id,
            pack.archive,
            &request.create.selection_id,
        )
        .map_err(pack_error)?;
        let (target, install, admission) = self.resolve(&prepared.target().selection_id).await?;
        let stored = StoredPackSetup::new(&prepared, &target, install)?;
        let setup = SetupIntent {
            plan_id: uuid::Uuid::new_v4().to_string(),
            request_json: serde_json::to_string(&stored)
                .map_err(|_| InstanceError::InvalidInput)?,
            archive,
        };
        let service = Arc::clone(self);
        let work = self
            .instances
            .tasks
            .try_spawn(admission.clone(), move |_cancel| async move {
                let admitted = service
                    .instances
                    .create_admitted(request.create, target, admission, setup)?
                    .join()
                    .await
                    .map_err(|_| InstanceError::SettlementRequired)??;
                service.queue_setup(admitted, Some(prepared), false).await
            })
            .map_err(|_| InstanceError::Closed)?;
        self.finish_setup(work).await
    }
}

impl StoredPackSetup {
    fn local(&self) -> bool {
        self.canonical_id.provider() == Some(ProviderId::Local)
    }

    pub(super) async fn install(
        &self,
        admitted: &RegisteredInstance,
        instances: &InstanceService,
        content: &ContentService,
        mutations: &ContentMutations,
        prepared: Option<PreparedPackCreation>,
        cancel: &CancellationToken,
    ) -> InstanceResult<()> {
        admitted.validate_current()?;
        let instance = &admitted.record().instance;
        if instance.version_id != self.installed_version_id
            || instance.minecraft_version != self.minecraft_version
            || instance.loader_key != self.loader_key
        {
            return Err(InstanceError::Conflict);
        }
        // A content commit can precede the setup handoff commit. Verify its
        // retained destination witnesses to resume without redownloading.
        if !mutations
            .installed_pack_admitted(
                admitted,
                &self.canonical_id,
                &self.version_id,
                &self.archive_fingerprint,
            )
            .map_err(pack_mutation_error)?
        {
            let prepared = match prepared {
                Some(prepared) => prepared,
                None => {
                    let pack = if self.local() {
                        read_stored_archive(admitted)?
                    } else {
                        mutations
                            .resolve_pack(
                                content,
                                &self.canonical_id,
                                PackVersion::Exact(&self.version_id),
                            )
                            .await
                            .map_err(pack_mutation_error)?
                    };
                    PreparedPackCreation::new(
                        pack.canonical_id,
                        pack.version_id,
                        pack.archive,
                        &self.selection_id,
                    )
                    .map_err(pack_error)?
                }
            };
            self.check_prepared(&prepared)?;
            mutations
                .install_pack_admitted(
                    content,
                    admitted.clone(),
                    self.canonical_id.clone(),
                    self.version_id.clone(),
                    prepared.into_plan(),
                    cancel,
                )
                .await
                .map_err(pack_mutation_error)?;
        }
        if self.local() {
            remove_stored_archive(instances, admitted)?;
        }
        Ok(())
    }
}

/// The stored copy must be the exact archive this setup accepted.
fn read_stored_archive(admitted: &RegisteredInstance) -> InstanceResult<ResolvedPack> {
    let file = admitted
        .game_directory()
        .open_file(&stored_archive_name())
        .map_err(|_| InstanceError::SetupUnavailable)?;
    let revision = file
        .revision()
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let bytes = file
        .read_bounded(MAX_PACK_ARCHIVE_BYTES)
        .map_err(|_| InstanceError::SetupUnavailable)?;
    file.validate_revision(&revision)
        .map_err(|_| InstanceError::Conflict)?;
    read_local_pack(bytes).map_err(pack_error)
}

/// Settling the setup retires its stored copy. Only the exact bytes observed
/// here are removed; unresolved effects stay with the instance owner.
fn remove_stored_archive(
    instances: &InstanceService,
    admitted: &RegisteredInstance,
) -> InstanceResult<()> {
    let root = admitted.game_directory();
    let file = match root.open_file(&stored_archive_name()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(InstanceError::DirectoryUnavailable),
    };
    let digest = (|| {
        let revision = file.revision()?;
        let mut reader = file.reader(MAX_PACK_ARCHIVE_BYTES)?;
        let mut digest = Sha256::new();
        std::io::copy(&mut reader, &mut digest)?;
        reader.finish()?;
        file.validate_revision(&revision)?;
        std::io::Result::Ok(axial_fs::ExpectedFileContent::new(
            revision,
            digest.finalize().into(),
        ))
    })()
    .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let owner = instances.for_operation(admitted.record().instance.id.clone());
    let (outcome, pin) = file.park(digest, root).into_parts();
    let effect = match outcome {
        axial_fs::FileParkOutcome::Parked(parked) => match parked.remove() {
            axial_fs::FileRemovalOutcome::Removed => return Ok(()),
            outcome => NativeEffect::FileRemove(outcome, pin),
        },
        axial_fs::FileParkOutcome::NoEffect { .. }
        | axial_fs::FileParkOutcome::Preserved { .. } => return Err(InstanceError::Conflict),
        outcome => NativeEffect::FilePark(outcome, pin),
    };
    owner.retain(effect);
    Err(InstanceError::SettlementRequired)
}

pub(super) fn pack_setup_result(success: bool) -> CreateResultView {
    if success {
        CreateResultView {
            state_id: "setup_complete",
            tone: "success",
            title: "Instance created",
            summary: "Instance created and modpack installed.",
            detail: None,
        }
    } else {
        CreateResultView {
            state_id: "setup_pending",
            tone: "warn",
            title: "Instance setup incomplete",
            summary: "The instance was created, but its modpack or game installation setup needs to be resumed.",
            detail: Some(
                "Resume setup on this instance to retry. The instance stays unavailable until its setup is completed.",
            ),
        }
    }
}

fn pack_error(error: PackError) -> InstanceError {
    match error {
        PackError::SelectionChanged | PackError::Conflict(_) => InstanceError::Conflict,
        PackError::Cancelled => InstanceError::Cancelled,
        PackError::Invalid(_)
        | PackError::Unsupported
        | PackError::TooLarge
        | PackError::DisallowedHost => InstanceError::InvalidInput,
        _ => InstanceError::SetupUnavailable,
    }
}

fn pack_mutation_error(error: MutationError) -> InstanceError {
    match error {
        MutationError::Pack(error) => pack_error(error),
        _ => InstanceError::SetupUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::model::ProviderId;
    use crate::content::packs::{PackLoader, parse_pack_index};
    use axial_minecraft::loaders::{LoaderComponentId, parse_build_id};
    use std::io::{Cursor, Write};

    fn archive(config: &[u8]) -> PackArchive {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, bytes) in [
            ("modrinth.index.json", br#"{"formatVersion":1,"game":"minecraft","versionId":"display-v1","name":"Fixture pack","dependencies":{"minecraft":"1.21.4"},"files":[]}"#.as_slice()),
            ("overrides/config/fixture.txt", b"common defaults".as_slice()),
            ("client-overrides/config/fixture.txt", config),
        ] {
            zip.start_file(path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        PackArchive::read(zip.finish().unwrap().into_inner()).unwrap()
    }

    fn prepared(config: &[u8]) -> PreparedPackCreation {
        PreparedPackCreation::new(
            CanonicalId::for_project(ProviderId::Modrinth, "fixture"),
            "provider-v1".into(),
            archive(config),
            "vanilla|1.21.4",
        )
        .unwrap()
    }

    fn target() -> CreateTarget {
        CreateTarget {
            selection_id: "vanilla|1.21.4".into(),
            version_id: "1.21.4".into(),
            minecraft_version: "1.21.4".into(),
            loader_key: "vanilla".into(),
        }
    }

    #[test]
    fn flat_frontend_request_uses_the_instance_settings_boundary() {
        let request: CreateFromModpackRequest = serde_json::from_str(
            r##"{
            "canonical_id":"modrinth:fixture","version_id":"provider-v1",
            "name":"Weekend pack","selection_id":"vanilla|1.21.4","icon":"box",
            "accent":"#112233","max_memory_mb":6144,"art_seed":42,
            "window_width":1280,"window_height":720,"jvm_preset_id":"balanced",
            "auto_optimize":false
        }"##,
        )
        .unwrap();
        assert_eq!(request.canonical_id, "modrinth:fixture");
        assert_eq!(request.version_id, "provider-v1");
        assert_eq!(request.create.name, "Weekend pack");
        assert_eq!(request.create.max_memory_mb, Some(6144));
        assert_eq!(request.create.art_seed, Some(42));
        assert_eq!(request.create.auto_optimize, Some(false));
        assert!(
            serde_json::from_str::<CreateFromModpackRequest>(
                r#"{
            "canonical_id":"modrinth:fixture","version_id":"provider-v1",
            "name":"Pack","selection_id":"vanilla|1.21.4","path":"/unrelated"
        }"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<CreateFromModpackRequest>(
                r#"{
            "canonical_id":"modrinth:fixture","version_id":"provider-v1",
            "name":"Pack","name":"Other","selection_id":"vanilla|1.21.4"
        }"#
            )
            .is_err()
        );
    }

    #[test]
    fn creation_includes_client_overrides_and_pins_the_archive_for_retry() {
        let prepared = prepared(b"client defaults");
        assert_eq!(prepared.target().version_id, "provider-v1");
        assert_eq!(prepared.plan().override_count(), 1);
        assert_eq!(prepared.plan().destinations(), vec!["config/fixture.txt"]);
        let stored = StoredPackSetup::new(
            &prepared,
            &target(),
            InstallQueueRequest::Vanilla {
                version_id: "1.21.4".into(),
            },
        )
        .unwrap();
        stored.check_prepared(&prepared).unwrap();
        let replacement = PreparedPackCreation::new(
            stored.canonical_id.clone(),
            stored.version_id.clone(),
            archive(b"changed defaults"),
            "vanilla|1.21.4",
        )
        .unwrap();
        assert!(matches!(
            stored.check_prepared(&replacement),
            Err(InstanceError::Conflict)
        ));
    }

    #[test]
    fn requested_or_resolved_loader_mismatch_cannot_create_an_instance() {
        assert!(matches!(
            PreparedPackCreation::new(
                CanonicalId::for_project(ProviderId::Modrinth, "fixture"),
                "provider-v1".into(),
                archive(b"defaults"),
                "loader_auto|net.fabricmc.fabric-loader|1.21.4",
            ),
            Err(PackError::SelectionChanged)
        ));
        let mut resolved = target();
        resolved.loader_key = "fabric".into();
        assert!(matches!(
            StoredPackSetup::new(
                &prepared(b"defaults"),
                &resolved,
                InstallQueueRequest::Vanilla {
                    version_id: "1.21.4".into()
                }
            ),
            Err(InstanceError::Conflict)
        ));
    }

    #[tokio::test]
    async fn durable_pack_setup_retains_the_created_identity_and_fences_ordinary_admission() {
        let (_root, service) = super::super::create::tests::fixture();
        let stored = StoredPackSetup::new(
            &prepared(b"client defaults"),
            &target(),
            InstallQueueRequest::Vanilla {
                version_id: "1.21.4".into(),
            },
        )
        .unwrap();
        let admitted = service
            .create_admitted(
                CreateInstanceRequest {
                    name: "Retry this pack".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    max_memory_mb: Some(4096),
                    auto_optimize: Some(false),
                    ..Default::default()
                },
                target(),
                service.creation_admission_for_tests().await.unwrap(),
                SetupIntent {
                    plan_id: "03b50763-6b11-4b43-9a17-4a2bd61a006b".into(),
                    request_json: serde_json::to_string(&stored).unwrap(),
                    archive: None,
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let id = admitted.record().instance.id.clone();
        assert_eq!(admitted.record().instance.settings.max_memory_mb, 4096);
        drop(admitted);
        assert!(super::super::setup::has_pending(service.registry().storage(), &id).unwrap());
        assert!(matches!(
            service.directories().admit(&id),
            Err(InstanceError::Busy)
        ));
        let admitted = service
            .directories()
            .admit_for_setup_settlement(&id)
            .unwrap();
        assert_eq!(admitted.record().instance.id, id);
        let json: String = service
            .registry()
            .storage()
            .read(|db| {
                db.query_row(
                    "SELECT request_json FROM instance_setups WHERE instance_id=?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(crate::storage::StorageError::from)
            })
            .unwrap();
        let resumed: StoredPackSetup = serde_json::from_str(&json).unwrap();
        assert_eq!(resumed.archive_fingerprint, stored.archive_fingerprint);
        assert_eq!(resumed.install, stored.install);
        assert_eq!(service.registry().list().unwrap().len(), 1);
    }

    /// A loopback origin standing in for `cdn.modrinth.com`, answering each
    /// download with the next body.
    async fn member_origin(
        bodies: Vec<&'static [u8]>,
    ) -> (
        reqwest::Url,
        axial_minecraft::download::TransferClient,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = reqwest::Url::parse(&format!(
            "http://{}/member.zip",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let task = tokio::spawn(async move {
            for body in bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0 && request.len() + count <= 8192);
                    request.extend_from_slice(&chunk[..count]);
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            }
        });
        let client = axial_minecraft::download::TransferClient::build(
            axial_minecraft::download::TransferClientConfig::bounded(
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(10),
                vec![
                    axial_minecraft::download::TransferOrigin::from_loopback_http_for_test_support(
                        &url,
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        (url, client, task)
    }

    #[tokio::test]
    async fn local_pack_setup_resumes_from_its_stored_copy_and_retires_it() {
        use sha1::Sha1;
        const MEMBER: &[u8] = b"member bytes";
        const CDN: &str = "https://cdn.modrinth.com/data/private/member.zip";
        let (_root, service) = super::super::create::tests::fixture();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let index = serde_json::json!({"formatVersion":1,"game":"minecraft","versionId":"2.0",
            "name":"Private pack","dependencies":{"minecraft":"1.21.4"},"files":[{
                "path":"resourcepacks/member.zip","downloads":[CDN],"fileSize":MEMBER.len(),
                "hashes":{"sha1":format!("{:x}", Sha1::digest(MEMBER)),
                    "sha512":format!("{:x}", Sha512::digest(MEMBER))}}]});
        for (path, bytes) in [
            ("modrinth.index.json", serde_json::to_vec(&index).unwrap()),
            ("overrides/config/fixture.txt", b"defaults".to_vec()),
        ] {
            zip.start_file(path, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&bytes).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let pack = read_local_pack(bytes.clone()).unwrap();
        let prepared = PreparedPackCreation::new(
            pack.canonical_id.clone(),
            pack.version_id.clone(),
            pack.archive.clone(),
            "vanilla|1.21.4",
        )
        .unwrap();
        let stored = StoredPackSetup::new(
            &prepared,
            &target(),
            InstallQueueRequest::Vanilla {
                version_id: "1.21.4".into(),
            },
        )
        .unwrap();
        let id = service
            .create_admitted(
                CreateInstanceRequest {
                    name: "Private pack".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                target(),
                service.creation_admission_for_tests().await.unwrap(),
                SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: serde_json::to_string(&stored).unwrap(),
                    archive: Some(pack.archive.bytes().clone()),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
            .record()
            .instance
            .id
            .clone();
        let game = super::super::create::tests::payload_path(&service, &id);
        assert_eq!(std::fs::read(game.join(STORED_ARCHIVE)).unwrap(), bytes);
        drop((pack, prepared));

        // After a restart nothing is retained in memory: the setup reads the
        // stored copy. A payload failing its hashes installs nothing.
        let (url, client, origin) = member_origin(vec![b"member BYTES", MEMBER]).await;
        let mutations = ContentMutations::new(
            service.directories().clone(),
            crate::network::ProviderClient::new(crate::network::ClientConfig::default()).unwrap(),
            crate::tasks::TaskOwner::new(16).unwrap(),
        )
        .with_transfer_fixture(CDN, url, client);
        let (content, provider) =
            crate::content::install::tests::metadata_provider(vec!["{}".into(), "{}".into()]).await;
        let install = || async {
            let admitted = service
                .directories()
                .admit_for_setup_settlement(&id)
                .unwrap();
            stored
                .install(
                    &admitted,
                    &service,
                    &content,
                    &mutations,
                    None,
                    &CancellationToken::new(),
                )
                .await
        };
        assert!(install().await.is_err());
        assert!(!game.join("resourcepacks/member.zip").exists());
        assert!(!game.join("config/fixture.txt").exists());
        assert!(!crate::content::install::has_pending(service.registry().storage(), &id).unwrap());
        assert_eq!(std::fs::read(game.join(STORED_ARCHIVE)).unwrap(), bytes);

        install().await.unwrap();
        origin.await.unwrap();
        provider.await.unwrap();
        assert_eq!(
            std::fs::read(game.join("resourcepacks/member.zip")).unwrap(),
            MEMBER
        );
        assert_eq!(
            std::fs::read(game.join("config/fixture.txt")).unwrap(),
            b"defaults"
        );
        assert!(!game.join(STORED_ARCHIVE).exists());
        let manifest = crate::content::provenance::ContentManifest::decode_managed(Some(
            &std::fs::read(game.join(crate::content::provenance::MANIFEST_FILE)).unwrap(),
        ))
        .unwrap();
        let entry = manifest.find(&stored.canonical_id).unwrap();
        assert_eq!(entry.provider(), ProviderId::Local);
        assert_eq!(entry.title(), Some("Private pack"));
        assert_eq!(entry.version_number(), Some("2.0"));
        assert_eq!(
            entry.pack_paths().collect::<Vec<_>>(),
            vec!["resourcepacks/member.zip", "config/fixture.txt"]
        );
        // The settled setup is idempotent without its stored copy.
        install().await.unwrap();
    }

    #[tokio::test]
    async fn a_stored_archive_keeps_an_untouched_setup_removable() {
        let (_root, service) = super::super::create::tests::fixture();
        let admitted = service
            .create_admitted(
                CreateInstanceRequest {
                    name: "Abandoned pack".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                target(),
                service.creation_admission_for_tests().await.unwrap(),
                SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: "{}".into(),
                    archive: Some(Arc::from(b"archive".as_slice())),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let id = admitted.record().instance.id.clone();
        let snapshot = service
            .delete_pristine_setup_admitted(admitted, uuid::Uuid::new_v4())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.status,
            super::super::delete::DeletionStatus::Removed
        );
        assert!(!super::super::create::tests::payload_path(&service, &id).exists());
    }

    #[test]
    fn each_declared_loader_keeps_its_exact_build_coordinate() {
        for component in [
            LoaderComponentId::Fabric,
            LoaderComponentId::Quilt,
            LoaderComponentId::Forge,
            LoaderComponentId::NeoForge,
        ] {
            let index = PackIndex {
                name: "Example".into(),
                version: "pack-v1".into(),
                minecraft: "1.21.6".into(),
                loader: Some(PackLoader {
                    component_id: component,
                    version: "exact-build.7".into(),
                }),
                files: vec![],
            };
            let target = target_from_pack_index(
                CanonicalId::for_project(ProviderId::Modrinth, "project"),
                "release".into(),
                "Example".into(),
                &index,
            );
            let (_, rest) = target.selection_id.split_once('|').unwrap();
            let (loader, build) = rest.split_once('|').unwrap();
            assert_eq!(loader, component.as_str());
            assert_eq!(
                parse_build_id(build),
                Some((component, "1.21.6".into(), "exact-build.7".into()))
            );
        }
    }

    #[test]
    fn vanilla_target_does_not_gain_a_loader_from_display_metadata() {
        let index = parse_pack_index(r#"{"dependencies":{"minecraft":"1.21.6"}}"#).unwrap();
        let target = target_from_pack_index(
            CanonicalId::for_project(ProviderId::Modrinth, "project"),
            "v1".into(),
            "Fabric in the display name".into(),
            &index,
        );
        assert_eq!(target.selection_id, "vanilla|1.21.6");
        assert!(target.loader.is_none());
    }
}
