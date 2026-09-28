//! Creation requirements taken from the retained pack archive, never from its
//! provider summary. Queue admission remains with installation coordination.

use super::{
    create::{CreateInstanceRequest, CreateTarget, SetupIntent},
    directory::RegisteredInstance,
    model::{Instance, InstanceError, InstanceResult},
    setup::{CreateInstanceResponse, CreateResultView, SetupService},
};
use crate::content::catalog::ContentService;
use crate::content::install::{ContentMutations, MutationError};
use crate::content::model::CanonicalId;
use crate::content::packs::{PackArchive, PackError, PackIndex, PackPlan, PackResult};
use crate::install::model::{InstallQueueContentActionRequest, InstallQueueRequest};
use crate::tasks::CancellationToken;
use axial_minecraft::loaders::{build_id_for, installed_version_id_for};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
                Some(&request.version_id),
            )
            .await
            .map_err(pack_mutation_error)?;
        let prepared = PreparedPackCreation::new(
            pack.canonical_id,
            pack.version_id,
            pack.archive,
            &request.create.selection_id,
        )
        .map_err(pack_error)?;
        let (target, install) = self.resolve(&prepared.target().selection_id).await?;
        let stored = StoredPackSetup::new(&prepared, &target, install)?;
        let setup = SetupIntent {
            plan_id: uuid::Uuid::new_v4().to_string(),
            request_json: serde_json::to_string(&stored)
                .map_err(|_| InstanceError::InvalidInput)?,
        };
        let service = Arc::clone(self);
        let work = self
            .instances
            .tasks
            .try_spawn((), move |_cancel| async move {
                let admitted = service
                    .instances
                    .create_admitted(request.create, target, setup)?
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
    pub(super) async fn install(
        &self,
        admitted: &RegisteredInstance,
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
                    let pack = mutations
                        .resolve_pack(content, &self.canonical_id, Some(&self.version_id))
                        .await
                        .map_err(pack_mutation_error)?;
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
        Ok(())
    }
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
        PackError::Invalid(_) | PackError::Unsupported | PackError::TooLarge => {
            InstanceError::InvalidInput
        }
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
                SetupIntent {
                    plan_id: "03b50763-6b11-4b43-9a17-4a2bd61a006b".into(),
                    request_json: serde_json::to_string(&stored).unwrap(),
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
