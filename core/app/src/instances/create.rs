//! Retained instance publication. Only a verified, completely populated
//! directory may cross the registry's visibility commit point.

use super::{
    directory::{InstanceDirectories, Registry},
    model::{
        Instance, InstanceError, InstanceId, InstanceRecord, InstanceResult, validate_label,
        validate_name,
    },
};
use crate::{
    files::{PortableName, ScopedDirectory},
    library::GenerationPin,
    settings::InstanceSettings,
    storage::{
        Migration,
        rusqlite::{OptionalExtension, params},
    },
    tasks::{CancellationToken, ExclusionLease, TaskHandle, TaskOwner},
    telemetry::{Telemetry, TelemetryEvent, TelemetryLoader},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(super) const INITIAL_DIRECTORIES: [&str; 7] = [
    "mods",
    "saves",
    "resourcepacks",
    "shaderpacks",
    "config",
    "screenshots",
    "logs",
];

pub const MIGRATION: Migration = Migration {
    id: "instance_creations.v1",
    sql: "CREATE TABLE instance_creations (
        instance_id TEXT PRIMARY KEY NOT NULL,
        record_json TEXT NOT NULL,
        stage_name TEXT NOT NULL,
        directory_receipt TEXT,
        park_receipt TEXT,
        phase TEXT NOT NULL CHECK(phase IN ('building','ready','published','cancelling','complete','cancelled'))
    );
    CREATE TABLE instance_setups (
        instance_id TEXT PRIMARY KEY NOT NULL,
        plan_id TEXT NOT NULL UNIQUE,
        request_json TEXT NOT NULL,
        phase TEXT NOT NULL CHECK(phase IN ('pending','complete'))
    );",
};

pub const DUPLICATE_WITNESS_MIGRATION: Migration = Migration {
    id: "instance_creations.v2",
    sql: "ALTER TABLE instance_creations ADD COLUMN performance_witness TEXT
        CHECK(performance_witness IS NULL OR length(performance_witness) <= 256);",
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInstanceRequest {
    pub name: String,
    pub selection_id: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub accent: String,
    pub art_seed: Option<u32>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
    pub jvm_preset_id: Option<String>,
    pub auto_optimize: Option<bool>,
}

/// Constructed by setup only after provider/installed-version validation.
#[derive(Clone, Debug)]
pub struct CreateTarget {
    pub(crate) selection_id: String,
    pub(crate) version_id: String,
    pub(crate) minecraft_version: String,
    pub(crate) loader_key: String,
}

impl CreateTarget {
    pub fn selection_id(&self) -> &str {
        &self.selection_id
    }
    pub fn version_id(&self) -> &str {
        &self.version_id
    }
    pub fn minecraft_version(&self) -> &str {
        &self.minecraft_version
    }
    pub fn loader_key(&self) -> &str {
        &self.loader_key
    }
}

/// These are status records, never permission to reopen serialized paths.
#[derive(Clone, Debug, Serialize)]
pub struct PendingCreation {
    pub instance_id: InstanceId,
    pub phase: String,
}

#[derive(Clone)]
pub(crate) struct SetupIntent {
    pub plan_id: String,
    pub request_json: String,
}

pub(crate) enum PublicationSource {
    Duplicate {
        source: super::directory::RegisteredInstance,
        performance: crate::performance::duplicate::PreparedDuplicate,
    },
    Import(crate::import::PreparedInstanceImport),
}

#[derive(Clone)]
pub struct InstanceService {
    pub(crate) directories: InstanceDirectories,
    pub(crate) tasks: TaskOwner,
    // A domain error cannot revoke a native effect or its physical generation.
    // Composition must refuse shutdown/reset while these obligations remain.
    retained: Arc<Mutex<BTreeMap<Option<InstanceId>, Vec<NativeEffect>>>>,
    admissions: Arc<Mutex<BTreeMap<InstanceId, PendingAdmission>>>,
    operation: Option<InstanceId>,
    telemetry: Option<Arc<Telemetry>>,
}

#[derive(Clone)]
pub(crate) struct PendingAdmission {
    pub pin: GenerationPin,
    pub lease: ExclusionLease,
}

pub(crate) enum NativeEffect {
    DirectoryCreate(
        axial_fs::DirectoryCreateOutcome,
        GenerationPin,
        PortableName,
    ),
    DirectoryMove(axial_fs::DirectoryMoveOutcome, GenerationPin),
    DirectoryPark(axial_fs::DirectoryParkOutcome, GenerationPin),
    DirectoryRestore(axial_fs::DirectoryRestoreOutcome, GenerationPin),
    DirectoryRemove(axial_fs::DirectoryTreeRemovalOutcome, GenerationPin),
    StageCreate(crate::files::StageCreateOutcome),
    StageSeal(crate::files::StageSealFailure),
    StageDiscard(axial_fs::StageDiscardOutcome, GenerationPin),
    FilePublish(axial_fs::FilePromotionOutcome, GenerationPin),
}

impl InstanceService {
    pub fn new(directories: InstanceDirectories, tasks: TaskOwner) -> Self {
        Self {
            directories,
            tasks,
            retained: Arc::new(Mutex::new(BTreeMap::new())),
            admissions: Arc::new(Mutex::new(BTreeMap::new())),
            operation: None,
            telemetry: None,
        }
    }

    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    fn emit_created(&self, instance: &Instance) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.emit(TelemetryEvent::InstanceCreated {
                loader: TelemetryLoader::from_key(&instance.loader_key),
            });
        }
    }

    pub fn directories(&self) -> &InstanceDirectories {
        &self.directories
    }
    pub fn registry(&self) -> &Registry {
        self.directories.registry()
    }
    pub fn has_unsettled_effects(&self) -> bool {
        !self
            .retained
            .lock()
            .expect("instance effects lock poisoned")
            .is_empty()
            || !self
                .admissions
                .lock()
                .expect("instance admissions lock poisoned")
                .is_empty()
    }

    /// Durable intent remains a reset blocker even when all native work is
    /// settled and shutdown can safely leave it fenced for a later restart.
    pub fn has_pending_intents(&self) -> bool {
        self.pending().map_or(true, |rows| !rows.is_empty())
            || self
                .pending_deletions()
                .map_or(true, |rows| !rows.is_empty())
            || self
                .registry()
                .storage()
                .read(|db| -> Result<bool, crate::storage::StorageError> {
                    Ok(db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM instance_setups WHERE phase='pending')",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .unwrap_or(true)
    }

    pub(crate) fn retain(&self, effect: NativeEffect) {
        self.retained
            .lock()
            .expect("instance effects lock poisoned")
            .entry(self.operation.clone())
            .or_default()
            .push(effect);
    }

    pub(crate) fn for_operation(&self, id: InstanceId) -> Self {
        let mut service = self.clone();
        service.operation = Some(id);
        service
    }

    pub(crate) fn hold_admission(&self, id: InstanceId, pin: GenerationPin, lease: ExclusionLease) {
        self.admissions
            .lock()
            .expect("instance admissions lock poisoned")
            .insert(id, PendingAdmission { pin, lease });
    }

    pub(crate) fn retained_admission(&self, id: &InstanceId) -> Option<PendingAdmission> {
        self.admissions
            .lock()
            .expect("instance admissions lock poisoned")
            .remove(id)
    }

    pub(crate) fn settle_effects(&self) -> InstanceResult<()> {
        let effects = self
            .retained
            .lock()
            .expect("instance effects lock poisoned")
            .remove(&self.operation)
            .unwrap_or_default();
        for effect in effects {
            if let Some(effect) = self.settle_effect(effect) {
                self.retain(effect);
            }
        }
        if self
            .retained
            .lock()
            .expect("instance effects lock poisoned")
            .contains_key(&self.operation)
        {
            Err(InstanceError::SettlementRequired)
        } else {
            Ok(())
        }
    }

    fn settle_effect(&self, effect: NativeEffect) -> Option<NativeEffect> {
        use NativeEffect::*;
        match effect {
            DirectoryCreate(outcome, pin, name) => match outcome {
                axial_fs::DirectoryCreateOutcome::AppliedUnverified(obligation) => {
                    match obligation.reconcile() {
                        axial_fs::DirectoryCreateResolution::Created(directory) => {
                            self.retain_created_witness(directory, pin, name)
                        }
                        axial_fs::DirectoryCreateResolution::Indeterminate(obligation) => {
                            Some(DirectoryCreate(
                                axial_fs::DirectoryCreateOutcome::AppliedUnverified(obligation),
                                pin,
                                name,
                            ))
                        }
                    }
                }
                axial_fs::DirectoryCreateOutcome::CreatedUnclassified {
                    error,
                    preservation,
                } => preservation
                    .acknowledge_preserved()
                    .err()
                    .map(|preservation| {
                        DirectoryCreate(
                            axial_fs::DirectoryCreateOutcome::CreatedUnclassified {
                                error,
                                preservation,
                            },
                            pin,
                            name,
                        )
                    }),
                axial_fs::DirectoryCreateOutcome::Created(directory) => {
                    self.retain_created_witness(directory, pin, name)
                }
                axial_fs::DirectoryCreateOutcome::NoEffect(_) => None,
            },
            DirectoryMove(axial_fs::DirectoryMoveOutcome::AppliedUnverified(obligation), pin) => {
                match obligation.reconcile() {
                    axial_fs::DirectoryMoveResolution::Indeterminate(obligation) => {
                        Some(DirectoryMove(
                            axial_fs::DirectoryMoveOutcome::AppliedUnverified(obligation),
                            pin,
                        ))
                    }
                    _ => None,
                }
            }
            DirectoryMove(_, _) => None,
            DirectoryPark(outcome, pin) => match outcome {
                axial_fs::DirectoryParkOutcome::Parked(parked) => {
                    self.settle_effect(DirectoryRestore(parked.restore(), pin))
                }
                axial_fs::DirectoryParkOutcome::AppliedUnverified(obligation) => {
                    match obligation.restore() {
                        axial_fs::DirectoryParkResolution::Parked(parked) => {
                            self.settle_effect(DirectoryRestore(parked.restore(), pin))
                        }
                        axial_fs::DirectoryParkResolution::NoEffect(_) => None,
                        axial_fs::DirectoryParkResolution::Indeterminate(obligation) => {
                            Some(DirectoryPark(
                                axial_fs::DirectoryParkOutcome::AppliedUnverified(obligation),
                                pin,
                            ))
                        }
                    }
                }
                axial_fs::DirectoryParkOutcome::NoEffect { .. } => None,
            },
            DirectoryRestore(outcome, pin) => match outcome {
                axial_fs::DirectoryRestoreOutcome::Restored(_) => None,
                axial_fs::DirectoryRestoreOutcome::NoEffect { parked, .. } => {
                    match parked.restore() {
                        axial_fs::DirectoryRestoreOutcome::Restored(_) => None,
                        outcome => Some(DirectoryRestore(outcome, pin)),
                    }
                }
                axial_fs::DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                    match obligation.reconcile() {
                        axial_fs::DirectoryRestoreResolution::Restored(_) => None,
                        axial_fs::DirectoryRestoreResolution::NoEffect(parked) => {
                            match parked.restore() {
                                axial_fs::DirectoryRestoreOutcome::Restored(_) => None,
                                outcome => Some(DirectoryRestore(outcome, pin)),
                            }
                        }
                        axial_fs::DirectoryRestoreResolution::Indeterminate(obligation) => {
                            Some(DirectoryRestore(
                                axial_fs::DirectoryRestoreOutcome::AppliedUnverified(obligation),
                                pin,
                            ))
                        }
                    }
                }
            },
            DirectoryRemove(outcome, pin) => match outcome {
                axial_fs::DirectoryTreeRemovalOutcome::Removed => None,
                axial_fs::DirectoryTreeRemovalOutcome::Retained { retained, .. } => {
                    match retained.retry() {
                        axial_fs::DirectoryTreeRemovalOutcome::Removed => None,
                        outcome => Some(DirectoryRemove(outcome, pin)),
                    }
                }
                axial_fs::DirectoryTreeRemovalOutcome::Indeterminate(obligation) => {
                    match obligation.reconcile() {
                        axial_fs::DirectoryTreeRemovalResolution::Removed => None,
                        axial_fs::DirectoryTreeRemovalResolution::Indeterminate(obligation) => {
                            Some(DirectoryRemove(
                                axial_fs::DirectoryTreeRemovalOutcome::Indeterminate(obligation),
                                pin,
                            ))
                        }
                    }
                }
            },
            StageCreate(crate::files::StageCreateOutcome::Unresolved {
                obligation,
                parent,
                destination,
            }) => {
                let (obligation, pin) = obligation.into_parts();
                match obligation.reconcile() {
                    axial_fs::FileCreateResolution::Created(stage) => {
                        self.settle_effect(StageDiscard(stage.discard(), pin))
                    }
                    axial_fs::FileCreateResolution::NoEffect(_) => None,
                    axial_fs::FileCreateResolution::Indeterminate(obligation) => {
                        Some(StageCreate(crate::files::StageCreateOutcome::Unresolved {
                            obligation: crate::files::Retained::new(obligation, pin),
                            parent,
                            destination,
                        }))
                    }
                }
            }
            StageCreate(crate::files::StageCreateOutcome::Created(stage)) => {
                let (outcome, pin) = stage.discard().into_parts();
                self.settle_effect(StageDiscard(outcome, pin))
            }
            StageCreate(crate::files::StageCreateOutcome::NoEffect(_)) => None,
            StageSeal(error) => {
                let (outcome, pin) = error.into_staged().discard().into_parts();
                self.settle_effect(StageDiscard(outcome, pin))
            }
            StageDiscard(outcome, pin) => match outcome {
                axial_fs::StageDiscardOutcome::Discarded => None,
                axial_fs::StageDiscardOutcome::AppliedUnverified(obligation) => {
                    match obligation.reconcile() {
                        axial_fs::StageDiscardResolution::Discarded => None,
                        axial_fs::StageDiscardResolution::Indeterminate(obligation) => {
                            Some(StageDiscard(
                                axial_fs::StageDiscardOutcome::AppliedUnverified(obligation),
                                pin,
                            ))
                        }
                    }
                }
            },
            FilePublish(outcome, pin) => match outcome {
                axial_fs::FilePromotionOutcome::Applied(_) => None,
                axial_fs::FilePromotionOutcome::NoEffect { staged, .. } => {
                    self.settle_effect(StageDiscard(staged.discard(), pin))
                }
                axial_fs::FilePromotionOutcome::AppliedUnverified(obligation) => {
                    match obligation.reconcile() {
                        axial_fs::FilePromotionResolution::Applied(_) => None,
                        axial_fs::FilePromotionResolution::NoEffect(staged) => {
                            self.settle_effect(StageDiscard(staged.discard(), pin))
                        }
                        axial_fs::FilePromotionResolution::Indeterminate(obligation) => {
                            Some(FilePublish(
                                axial_fs::FilePromotionOutcome::AppliedUnverified(obligation),
                                pin,
                            ))
                        }
                    }
                }
            },
        }
    }

    fn retain_created_witness(
        &self,
        directory: axial_fs::Directory,
        pin: GenerationPin,
        name: PortableName,
    ) -> Option<NativeEffect> {
        let Some(id) = &self.operation else {
            return None;
        };
        if name.as_str() != format!("stage-{id}") {
            return None;
        }
        let persisted = (|| -> InstanceResult<()> {
            let scoped = ScopedDirectory::from_admitted(directory.clone(), pin.clone())
                .map_err(|_| InstanceError::DirectoryUnavailable)?;
            let record = self.registry().get_record(id)?;
            self.creation_phase(
                &record,
                "building",
                Some(
                    &scoped
                        .receipt()
                        .map_err(|_| InstanceError::DirectoryUnavailable)?,
                ),
                None,
                None,
            )
        })();
        persisted.err().map(|_| {
            NativeEffect::DirectoryCreate(
                axial_fs::DirectoryCreateOutcome::Created(directory),
                pin,
                name,
            )
        })
    }

    pub fn pending(&self) -> InstanceResult<Vec<PendingCreation>> {
        self.registry().storage().read(|db| {
            let mut query = db.prepare("SELECT instance_id,phase FROM instance_creations WHERE phase NOT IN ('complete','cancelled') ORDER BY rowid")?;
            let rows = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().map(|(id, phase)| Ok(PendingCreation { instance_id: id.parse()?, phase })).collect()
        })
    }

    pub async fn recover_pending(&self) -> InstanceResult<()> {
        let mut failed = false;
        let effects = self
            .retained
            .lock()
            .expect("instance effects lock poisoned")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for operation in effects {
            let mut service = self.clone();
            service.operation = operation;
            failed |= service.settle_effects().is_err();
        }
        for creation in self.pending()? {
            match self.recover_creation(&creation.instance_id) {
                Ok(task) => failed |= !matches!(task.join().await, Ok(Ok(_))),
                Err(_) => failed = true,
            }
        }
        for deletion in self
            .pending_deletions()
            .map_err(|_| InstanceError::SettlementRequired)?
        {
            match self.delete(
                &deletion.instance_id,
                deletion.intent,
                deletion.operation_id,
            ) {
                Ok(task) => failed |= !matches!(task.join().await, Ok(Ok(_))),
                Err(_) => failed = true,
            }
        }
        if failed {
            Err(InstanceError::SettlementRequired)
        } else {
            Ok(())
        }
    }

    /// Startup or explicit retry settles only the captured physical witness.
    /// A stage created before its receipt was durably saved stays preserved.
    pub fn recover_creation(
        &self,
        id: &InstanceId,
    ) -> InstanceResult<TaskHandle<InstanceResult<Option<Instance>>>> {
        // Reject unknown IDs before consuming or creating any retained
        // authority. A read-only retry must not invent a shutdown obligation.
        let pending = self
            .registry()
            .storage()
            .read(|db| -> InstanceResult<bool> {
                let phase: String = db
                    .query_row(
                        "SELECT phase FROM instance_creations WHERE instance_id=?1",
                        [id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or(InstanceError::NotFound)?;
                Ok(!matches!(phase.as_str(), "complete" | "cancelled"))
            })?;
        let PendingAdmission { pin, lease } = match self.retained_admission(id) {
            Some(admission) => admission,
            None => PendingAdmission {
                pin: self
                    .directories
                    .library()
                    .admit()
                    .map_err(|_| InstanceError::LibraryUnavailable)?,
                lease: self
                    .directories
                    .exclusions()
                    .try_acquire([id.as_str()], [])
                    .map_err(|_| InstanceError::Busy)?,
            },
        };
        let id = id.clone();
        let service = self.for_operation(id.clone());
        let fallback = (id.clone(), pin.clone(), lease.clone());
        let task = self
            .tasks
            .try_spawn((pin.clone(), lease.clone()), move |_cancel| async move {
                tokio::task::spawn_blocking(move || {
                    let result = service
                        .settle_effects()
                        .and_then(|()| service.recover_creation_inner(&id, &pin));
                    if result.is_err() && pending {
                        service.hold_admission(id, pin, lease);
                    }
                    result
                })
                .await
                .unwrap_or_else(|error| {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(InstanceError::Cancelled)
                })
            });
        if task.is_err() && pending {
            self.hold_admission(fallback.0, fallback.1, fallback.2);
        }
        task.map_err(|_| InstanceError::Closed)
    }

    fn recover_creation_inner(
        &self,
        id: &InstanceId,
        pin: &GenerationPin,
    ) -> InstanceResult<Option<Instance>> {
        self.recover_creation_with_import(id, pin, None)
    }

    pub(crate) fn recover_creation_with_import(
        &self,
        id: &InstanceId,
        pin: &GenerationPin,
        imported: Option<&crate::import::PreparedInstanceImport>,
    ) -> InstanceResult<Option<Instance>> {
        let (json, stage_name, receipt, park_receipt, phase, performance_witness) = self.registry().storage().read(|db| -> InstanceResult<_> {
            db.query_row("SELECT record_json,stage_name,directory_receipt,park_receipt,phase,performance_witness FROM instance_creations WHERE instance_id=?1",
                [id.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?))).optional()?.ok_or(InstanceError::NotFound)
        })?;
        if phase == "complete" {
            let record = self.registry().get_live(id)?;
            if let Some(imported) = imported {
                self.verify_imported_history(imported, &record.instance.id)?;
            }
            return Ok(Some(public_instance(record.instance)));
        }
        if phase == "cancelled" {
            return Ok(None);
        }
        if json.len() > 64 * 1024 {
            return Err(InstanceError::SettlementRequired);
        }
        let record: InstanceRecord =
            serde_json::from_str(&json).map_err(|_| InstanceError::SettlementRequired)?;
        if record.instance.id != *id
            || self.registry().get_record(id)? != record
            || record.library_id != pin.library_id().to_string()
            || stage_name != format!("stage-{id}")
            || record.directory_name != id.as_str()
        {
            return Err(InstanceError::SettlementRequired);
        }
        if let Some(imported) = imported {
            super::import::validate_destination_parent(imported, pin)?;
        }
        let parent = self.ensure_parent(pin)?;
        if let Some(imported) = imported {
            imported
                .validate_destination_root(parent.capability())
                .map_err(super::import::import_error)?;
        }
        if phase == "cancelling" {
            let plan = crate::files::DirectoryParkReceipt::decode(
                park_receipt
                    .as_deref()
                    .ok_or(InstanceError::SettlementRequired)?,
            )
            .map_err(|_| InstanceError::SettlementRequired)?;
            if plan.source_name() != stage_name
                || !plan.matches_source_receipt(
                    receipt
                        .as_deref()
                        .ok_or(InstanceError::SettlementRequired)?,
                )
            {
                return Err(InstanceError::SettlementRequired);
            }
            if let Some(imported) = imported {
                for name in [plan.source_name(), plan.park_name()] {
                    if let Some(directory) = optional_directory(&parent, name)? {
                        imported
                            .validate_destination_root(directory.capability())
                            .map_err(super::import::import_error)?;
                    }
                }
            }
            match parent
                .recover_park(&plan)
                .map_err(|_| InstanceError::SettlementRequired)?
            {
                crate::files::RecoveredDirectoryPark::Original(stage) => {
                    self.cancel_reserved(&record, &parent, stage)?
                }
                crate::files::RecoveredDirectoryPark::Parked(retained) => {
                    let (parked, pin) = retained.into_parts();
                    self.remove_parked(parked, pin)?;
                    self.abandon(&record)?;
                }
                crate::files::RecoveredDirectoryPark::Missing => self.abandon(&record)?,
                crate::files::RecoveredDirectoryPark::Conflict => {
                    return Err(InstanceError::SettlementRequired);
                }
            }
            return Ok(None);
        }
        let stage = optional_directory(&parent, &stage_name)?;
        let canonical = optional_directory(&parent, &record.directory_name)?;
        if let Some(imported) = imported {
            for directory in stage.iter().chain(canonical.iter()) {
                imported
                    .validate_destination_root(directory.capability())
                    .map_err(super::import::import_error)?;
            }
        }
        if phase == "building" {
            if canonical.is_some() {
                return Err(InstanceError::SettlementRequired);
            }
            match stage {
                Some(stage) => {
                    stage
                        .verify_receipt(
                            receipt
                                .as_deref()
                                .ok_or(InstanceError::SettlementRequired)?,
                        )
                        .map_err(|_| InstanceError::SettlementRequired)?;
                    self.cancel_reserved(&record, &parent, stage)?;
                }
                None => self.abandon(&record)?,
            }
            return Ok(None);
        }
        let receipt = receipt.ok_or(InstanceError::SettlementRequired)?;
        let canonical = match (stage, canonical) {
            (Some(stage), None) if phase == "ready" => {
                stage
                    .verify_receipt(&receipt)
                    .map_err(|_| InstanceError::SettlementRequired)?;
                self.verify_import_recovery(id, imported, &stage)?;
                if imported.is_none() {
                    crate::performance::duplicate::verify_recovered(
                        &stage,
                        performance_witness.as_deref(),
                    )
                    .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
                }
                self.promote(stage, &parent, &record)?
            }
            (None, Some(canonical)) if matches!(phase.as_str(), "ready" | "published") => canonical,
            _ => return Err(InstanceError::SettlementRequired),
        };
        canonical
            .verify_receipt(&receipt)
            .map_err(|_| InstanceError::SettlementRequired)?;
        self.verify_import_recovery(id, imported, &canonical)?;
        if imported.is_none() {
            crate::performance::duplicate::verify_recovered(
                &canonical,
                performance_witness.as_deref(),
            )
            .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
        }
        let history = imported
            .map(|imported| imported.bind_history(&record.instance.id))
            .transpose()
            .map_err(super::import::import_error)?;
        let committed = self
            .registry()
            .storage()
            .transaction(|tx| -> InstanceResult<_> {
                if let Some(imported) = imported {
                    super::import::validate_destination(tx, imported)?;
                }
                if let Some(history) = &history {
                    history
                        .reports
                        .insert_in(tx)
                        .map_err(super::import::report_error)?;
                    history
                        .benchmarks
                        .insert_in(tx)
                        .map_err(super::import::benchmark_error)?;
                    history
                        .operations
                        .insert_in(tx)
                        .map_err(super::import::operation_error)?;
                    if let Some(rules) = &history.rules {
                        rules.verify_in(tx).map_err(super::import::rules_error)?;
                    }
                }
                let committed = self.registry().commit_reserved(tx, &record, &receipt)?;
                if imported.is_some_and(|input| input.was_last_instance()) {
                    self.registry().restore_import_selection(tx, &committed)?;
                }
                let completed = tx.execute(
                    "UPDATE instance_creations SET phase='complete' WHERE instance_id=?1",
                    [id.as_str()],
                )?;
                if completed != 1 {
                    return Err(InstanceError::Conflict);
                }
                Ok(committed)
            })?;
        self.emit_created(&committed.instance);
        Ok(Some(public_instance(committed.instance)))
    }

    pub fn create(
        &self,
        request: CreateInstanceRequest,
        target: CreateTarget,
    ) -> InstanceResult<TaskHandle<InstanceResult<Instance>>> {
        let instance = build_instance(request, &target)?;
        let pin = self
            .directories
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let lease = self
            .directories
            .exclusions()
            .try_acquire([instance.id.as_str()], [])
            .map_err(|_| InstanceError::Busy)?;
        let service = self.for_operation(instance.id.clone());
        self.tasks
            .try_spawn((pin.clone(), lease.clone()), move |cancel| async move {
                service.publish_instance(instance, pin, lease, None, None, cancel)
            })
            .map_err(|_| InstanceError::Closed)
    }

    pub(crate) fn create_admitted(
        &self,
        request: CreateInstanceRequest,
        target: CreateTarget,
        setup: SetupIntent,
    ) -> InstanceResult<TaskHandle<InstanceResult<super::directory::RegisteredInstance>>> {
        let instance = build_instance(request, &target)?;
        let pin = self
            .directories
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let lease = self
            .directories
            .exclusions()
            .try_acquire([instance.id.as_str()], [])
            .map_err(|_| InstanceError::Busy)?;
        let service = self.for_operation(instance.id.clone());
        self.tasks
            .try_spawn((pin.clone(), lease.clone()), move |cancel| async move {
                let instance = service.publish_instance(
                    instance,
                    pin.clone(),
                    lease.clone(),
                    None,
                    Some(setup),
                    cancel,
                )?;
                super::directory::InstanceDirectories::admit_record(
                    service.registry().clone(),
                    service.registry().get_live(&instance.id)?,
                    pin,
                    lease,
                )
            })
            .map_err(|_| InstanceError::Closed)
    }

    pub fn update(
        &self,
        id: &InstanceId,
        patch: super::model::InstancePatch,
    ) -> InstanceResult<Instance> {
        let admitted = self.directories.admit(id)?;
        let record = self
            .registry()
            .update(id, admitted.record().revision, patch)?;
        Ok(public_instance(record.instance))
    }

    pub(crate) fn publish_instance(
        &self,
        instance: Instance,
        pin: GenerationPin,
        lease: ExclusionLease,
        source: Option<PublicationSource>,
        setup: Option<SetupIntent>,
        cancel: CancellationToken,
    ) -> InstanceResult<Instance> {
        if cancel.is_cancelled() {
            return Err(InstanceError::Cancelled);
        }
        if let Some(PublicationSource::Import(imported)) = &source {
            imported.revalidate().map_err(super::import::import_error)?;
            super::import::validate_destination_parent(imported, &pin)?;
        }
        let parent = self.ensure_parent(&pin)?;
        if let Some(PublicationSource::Import(imported)) = &source {
            imported
                .validate_destination_root(parent.capability())
                .map_err(super::import::import_error)?;
        }
        let stage_name = PortableName::new_exact(&format!("stage-{}", instance.id))
            .map_err(|_| InstanceError::InvalidId)?;
        let record = self.registry().storage().transaction(|tx| -> InstanceResult<_> {
            let record = match &source {
                Some(PublicationSource::Duplicate { source, .. }) => self.registry().reserve_duplicate(tx, source.record(), instance.id.clone(), Some(&instance.name), &pin.library_id().to_string())?,
                Some(PublicationSource::Import(imported)) => self.reserve_import(tx, imported, instance, &pin)?,
                None => self.registry().reserve(tx, instance, &pin.library_id().to_string())?,
            };
            let changed = tx.execute("INSERT INTO instance_creations(instance_id,record_json,stage_name,phase) VALUES(?1,?2,?3,'building')
                ON CONFLICT(instance_id) DO UPDATE SET record_json=excluded.record_json,stage_name=excluded.stage_name,directory_receipt=NULL,park_receipt=NULL,performance_witness=NULL,phase='building'
                WHERE instance_creations.phase='cancelled'",
                params![record.instance.id.as_str(), serde_json::to_string(&record).map_err(|_| InstanceError::InvalidInput)?, stage_name.as_str()])?;
            if changed != 1 { return Err(InstanceError::Conflict); }
            if let Some(setup) = setup {
                if setup.request_json.len() > 4 * 1024 * 1024 { return Err(InstanceError::InvalidInput); }
                tx.execute("INSERT INTO instance_setups(instance_id,plan_id,request_json,phase) VALUES(?1,?2,?3,'pending')",
                    params![record.instance.id.as_str(), setup.plan_id, setup.request_json])?;
            }
            Ok(record)
        })?;
        if cancel.is_cancelled() {
            self.abandon(&record).map_err(|error| {
                self.hold_admission(record.instance.id.clone(), pin.clone(), lease.clone());
                error
            })?;
            return Err(InstanceError::Cancelled);
        }
        let stage = match self.fresh_directory(&parent, &stage_name) {
            Ok(stage) => stage,
            Err(error) => {
                if !matches!(error, InstanceError::SettlementRequired)
                    && optional_directory(&parent, stage_name.as_str())
                        .map_err(|error| {
                            self.hold_admission(
                                record.instance.id.clone(),
                                pin.clone(),
                                lease.clone(),
                            );
                            error
                        })?
                        .is_none()
                {
                    self.abandon(&record).map_err(|error| {
                        self.hold_admission(record.instance.id.clone(), pin.clone(), lease.clone());
                        error
                    })?;
                } else {
                    self.hold_admission(record.instance.id.clone(), pin, lease);
                }
                return Err(error);
            }
        };
        let receipt = stage
            .receipt()
            .map_err(|_| InstanceError::DirectoryUnavailable);
        let performance_witness = match &source {
            Some(PublicationSource::Duplicate { performance, .. }) => Some(performance.witness()),
            _ => None,
        };
        let mut ready = false;
        let result = (|| -> InstanceResult<Instance> {
            let receipt = receipt?;
            self.creation_phase(&record, "building", Some(&receipt), None, None)?;
            let history = match &source {
                Some(PublicationSource::Import(imported)) => Some(
                    imported
                        .bind_history(&record.instance.id)
                        .map_err(super::import::import_error)?,
                ),
                _ => None,
            };
            if let Some(source) = &source {
                match source {
                    PublicationSource::Duplicate {
                        source,
                        performance,
                    } => {
                        super::duplicate::copy_payload(self, source, performance, &stage, &cancel)?;
                        performance
                            .verify_staged(&stage)
                            .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
                        source.validate_current()?;
                    }
                    PublicationSource::Import(imported) => {
                        super::import::copy_payload(self, imported, &stage, &cancel)?;
                        imported
                            .verify_staged(&stage, &cancel)
                            .map_err(super::import::import_error)?;
                        imported.revalidate().map_err(super::import::import_error)?;
                    }
                }
            } else {
                for name in INITIAL_DIRECTORIES {
                    self.fresh_directory(
                        &stage,
                        &PortableName::new_exact(name).expect("fixed portable name"),
                    )?;
                }
            }
            if cancel.is_cancelled() {
                return Err(InstanceError::Cancelled);
            }
            stage
                .verify_receipt(&receipt)
                .map_err(|_| InstanceError::DirectoryUnavailable)?;
            // Persist the ready witness before promotion. Once ready wins,
            // complete publication even if cancellation arrives afterwards.
            self.creation_phase(
                &record,
                "ready",
                None,
                match &source {
                    Some(PublicationSource::Import(imported)) => Some(imported),
                    _ => None,
                },
                performance_witness.as_deref(),
            )?;
            ready = true;
            let canonical = self.promote(stage.clone(), &parent, &record)?;
            canonical
                .verify_receipt(&receipt)
                .map_err(|_| InstanceError::DirectoryUnavailable)?;
            if let Some(PublicationSource::Import(imported)) = &source {
                self.verify_import_recovery(&record.instance.id, Some(imported), &canonical)?;
            }
            if let Some(witness) = &performance_witness {
                crate::performance::duplicate::verify_recovered(&canonical, Some(witness))
                    .map_err(|_| InstanceError::ManagedDuplicateUnavailable)?;
            }
            self.creation_phase(&record, "published", None, None, None)?;
            let committed = self
                .registry()
                .storage()
                .transaction(|tx| -> InstanceResult<_> {
                    if let Some(PublicationSource::Import(imported)) = &source {
                        super::import::validate_destination(tx, imported)?;
                    }
                    if let Some(history) = &history {
                        history
                            .reports
                            .insert_in(tx)
                            .map_err(super::import::report_error)?;
                        history
                            .benchmarks
                            .insert_in(tx)
                            .map_err(super::import::benchmark_error)?;
                        history
                            .operations
                            .insert_in(tx)
                            .map_err(super::import::operation_error)?;
                        if let Some(rules) = &history.rules {
                            rules.verify_in(tx).map_err(super::import::rules_error)?;
                        }
                    }
                    let committed = self.registry().commit_reserved(tx, &record, &receipt)?;
                    if matches!(&source, Some(PublicationSource::Import(input)) if input.was_last_instance()) {
                        self.registry().restore_import_selection(tx, &committed)?;
                    }
                    let completed = tx.execute(
                        "UPDATE instance_creations SET phase='complete' WHERE instance_id=?1",
                        [record.instance.id.as_str()],
                    )?;
                    if completed != 1 {
                        return Err(InstanceError::Conflict);
                    }
                    Ok(committed)
                })?;
            self.emit_created(&committed.instance);
            Ok(public_instance(committed.instance))
        })();
        if result.is_err() && !ready && self.settle_effects().is_ok() {
            if let Err(error) = self.cancel_reserved(&record, &parent, stage.clone()) {
                self.hold_admission(record.instance.id.clone(), pin, lease);
                return Err(error);
            }
        } else if result.is_err() {
            self.hold_admission(record.instance.id.clone(), pin, lease);
        }
        result
    }

    pub(crate) fn ensure_parent(&self, pin: &GenerationPin) -> InstanceResult<ScopedDirectory> {
        let root = pin.files().map_err(|_| InstanceError::LibraryUnavailable)?;
        let name = PortableName::new_exact("instances").expect("fixed portable name");
        match root.open_directory(&name) {
            Ok(parent) => Ok(parent),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.fresh_directory(&root, &name)
            }
            Err(_) => Err(InstanceError::DirectoryUnavailable),
        }
    }

    pub(crate) fn fresh_directory(
        &self,
        parent: &ScopedDirectory,
        name: &PortableName,
    ) -> InstanceResult<ScopedDirectory> {
        let (outcome, pin) = parent.create_directory(name).into_parts();
        match outcome {
            axial_fs::DirectoryCreateOutcome::Created(directory) => {
                ScopedDirectory::from_admitted(directory, pin)
                    .map_err(|_| InstanceError::DirectoryUnavailable)
            }
            axial_fs::DirectoryCreateOutcome::NoEffect(_) => {
                Err(InstanceError::DirectoryUnavailable)
            }
            unresolved => {
                self.retain(NativeEffect::DirectoryCreate(unresolved, pin, name.clone()));
                Err(InstanceError::SettlementRequired)
            }
        }
    }

    fn promote(
        &self,
        stage: ScopedDirectory,
        parent: &ScopedDirectory,
        record: &InstanceRecord,
    ) -> InstanceResult<ScopedDirectory> {
        let name = PortableName::new_exact(&record.directory_name)
            .map_err(|_| InstanceError::InvalidId)?;
        let (outcome, pin) = stage.move_no_replace(parent, &name).into_parts();
        match outcome {
            axial_fs::DirectoryMoveOutcome::Applied(directory) => {
                ScopedDirectory::from_admitted(directory, pin)
                    .map_err(|_| InstanceError::DirectoryUnavailable)
            }
            axial_fs::DirectoryMoveOutcome::NoEffect { .. } => {
                Err(InstanceError::DirectoryUnavailable)
            }
            unresolved => {
                self.retain(NativeEffect::DirectoryMove(unresolved, pin));
                Err(InstanceError::SettlementRequired)
            }
        }
    }

    pub(crate) fn remove_owned(&self, directory: ScopedDirectory) -> InstanceResult<()> {
        let (park, pin) = directory.park().into_parts();
        match park {
            axial_fs::DirectoryParkOutcome::Parked(parked) => self.remove_parked(parked, pin),
            axial_fs::DirectoryParkOutcome::NoEffect { .. } => {
                Err(InstanceError::DirectoryUnavailable)
            }
            unresolved => {
                self.retain(NativeEffect::DirectoryPark(unresolved, pin));
                Err(InstanceError::SettlementRequired)
            }
        }
    }

    fn cancel_reserved(
        &self,
        record: &InstanceRecord,
        parent: &ScopedDirectory,
        stage: ScopedDirectory,
    ) -> InstanceResult<()> {
        let name = PortableName::new_exact(&format!("stage-{}", record.instance.id))
            .map_err(|_| InstanceError::InvalidId)?;
        let park_name = PortableName::new_exact(&format!("cancelled-{}", record.instance.id))
            .map_err(|_| InstanceError::InvalidId)?;
        let plan = parent
            .plan_park(&name, &park_name)
            .map_err(|_| InstanceError::DirectoryUnavailable)?;
        if !plan.receipt().matches_source_receipt(
            &stage
                .receipt()
                .map_err(|_| InstanceError::DirectoryUnavailable)?,
        ) {
            return Err(InstanceError::SettlementRequired);
        }
        self.registry().storage().transaction(|tx| -> InstanceResult<()> {
            tx.execute("UPDATE instance_creations SET phase='cancelling',park_receipt=?2 WHERE instance_id=?1",
                params![record.instance.id.as_str(), plan.receipt().encode()])?;
            Ok(())
        })?;
        let (park, pin) = plan.execute().into_parts();
        match park {
            axial_fs::DirectoryParkOutcome::Parked(parked) => self.remove_parked(parked, pin)?,
            axial_fs::DirectoryParkOutcome::NoEffect { .. } => {
                return Err(InstanceError::DirectoryUnavailable);
            }
            unresolved => {
                self.retain(NativeEffect::DirectoryPark(unresolved, pin));
                return Err(InstanceError::SettlementRequired);
            }
        }
        self.abandon(record)
    }

    pub(crate) fn remove_parked(
        &self,
        parked: axial_fs::ParkedDirectory,
        pin: GenerationPin,
    ) -> InstanceResult<()> {
        match parked.remove_tree() {
            axial_fs::DirectoryTreeRemovalOutcome::Removed => Ok(()),
            unresolved => {
                self.retain(NativeEffect::DirectoryRemove(unresolved, pin));
                Err(InstanceError::SettlementRequired)
            }
        }
    }

    fn creation_phase(
        &self,
        record: &InstanceRecord,
        phase: &str,
        receipt: Option<&str>,
        imported: Option<&crate::import::PreparedInstanceImport>,
        performance_witness: Option<&str>,
    ) -> InstanceResult<()> {
        self.registry().storage().transaction(|tx| {
            if let Some(imported) = imported {
                super::import::validate_destination(tx, imported)?;
            }
            let changed = tx.execute("UPDATE instance_creations SET phase=?2,directory_receipt=COALESCE(?3,directory_receipt),performance_witness=CASE WHEN ?2='ready' THEN ?4 ELSE performance_witness END WHERE instance_id=?1",
                params![record.instance.id.as_str(), phase, receipt, performance_witness])?;
            if changed != 1 { return Err(InstanceError::Conflict); }
            Ok(())
        })
    }

    fn abandon(&self, record: &InstanceRecord) -> InstanceResult<()> {
        self.registry().storage().transaction(|tx| {
            self.registry().abandon_reserved(tx, record)?;
            tx.execute(
                "DELETE FROM instance_setups WHERE instance_id=?1",
                [record.instance.id.as_str()],
            )?;
            tx.execute(
                "UPDATE instance_creations SET phase='cancelled' WHERE instance_id=?1",
                [record.instance.id.as_str()],
            )?;
            Ok(())
        })
    }
}

fn optional_directory(
    parent: &ScopedDirectory,
    name: &str,
) -> InstanceResult<Option<ScopedDirectory>> {
    let name = PortableName::new_exact(name).map_err(|_| InstanceError::SettlementRequired)?;
    match parent.open_directory(&name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(InstanceError::DirectoryUnavailable),
    }
}

/// Runtime overrides may contain local paths or credentials. Keep them in the
/// registry, but retain the existing public redaction boundary.
pub fn public_instance(mut instance: Instance) -> Instance {
    instance.settings.java_path.clear();
    instance.settings.extra_jvm_args.clear();
    instance
}

fn build_instance(
    request: CreateInstanceRequest,
    target: &CreateTarget,
) -> InstanceResult<Instance> {
    if request.selection_id.trim() != target.selection_id {
        return Err(InstanceError::Conflict);
    }
    let name = validate_name(&request.name)?;
    validate_label(&request.icon, 1024)?;
    validate_label(&request.accent, 64)?;
    let id = InstanceId::new();
    let preset = request.jvm_preset_id.unwrap_or_default();
    let settings = InstanceSettings {
        max_memory_mb: request.max_memory_mb.unwrap_or_default(),
        min_memory_mb: request.min_memory_mb.unwrap_or_default(),
        window_width: request.window_width.unwrap_or_default(),
        window_height: request.window_height.unwrap_or_default(),
        jvm_preset: if crate::settings::ConfigJvmPreset::parse(&preset).is_some() {
            preset
        } else {
            String::new()
        },
        auto_optimize: request.auto_optimize.unwrap_or(true),
        ..Default::default()
    };
    settings
        .validate()
        .map_err(|_| InstanceError::InvalidSettings)?;
    Ok(Instance {
        art_seed: request
            .art_seed
            .unwrap_or_else(|| super::directory::new_art_seed(&id)),
        id,
        name,
        version_id: target.version_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        last_played_at: String::new(),
        settings,
        icon: request.icon,
        accent: request.accent,
        loader_key: target.loader_key.clone(),
        minecraft_version: target.minecraft_version.clone(),
        revision: 1,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{
        delete::{DeleteIntent, DeletionStatus},
        duplicate::DuplicateRequest,
    };
    use super::*;
    use crate::{
        library::{LibraryLifecycle, LibraryOpenOutcome},
        storage::MetadataStore,
        tasks::Exclusions,
    };

    pub(crate) fn fixture() -> (tempfile::TempDir, InstanceService) {
        let root = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated test library did not open: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(&root.path().join("metadata.sqlite")).unwrap());
        storage
            .migrate(&[
                super::super::directory::MIGRATION,
                MIGRATION,
                DUPLICATE_WITNESS_MIGRATION,
                super::super::import::MIGRATION,
                super::super::delete::MIGRATION,
                crate::content::install::MIGRATION,
                crate::performance::mutation::MIGRATION,
                crate::performance::mutation::MIGRATION_V2,
                crate::launch::reports::REPORT_MIGRATION,
                crate::performance::benchmarks::MIGRATION,
                crate::performance::benchmarks::MIGRATION_V2,
            ])
            .unwrap();
        crate::settings::SettingsStore::new(storage.clone()).unwrap();
        let service = InstanceService::new(
            InstanceDirectories::new(Registry::new(storage), library, Exclusions::new()),
            TaskOwner::new(16).unwrap(),
        );
        (root, service)
    }

    pub(crate) fn target() -> CreateTarget {
        CreateTarget {
            selection_id: "vanilla|1.21.4".into(),
            version_id: "1.21.4".into(),
            minecraft_version: "1.21.4".into(),
            loader_key: "vanilla".into(),
        }
    }

    pub(crate) fn request(name: &str) -> CreateInstanceRequest {
        CreateInstanceRequest {
            name: name.into(),
            selection_id: "vanilla|1.21.4".into(),
            ..Default::default()
        }
    }

    pub(crate) async fn create(service: &InstanceService, name: &str) -> Instance {
        service
            .create(request(name), target())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
    }

    pub(crate) fn payload_path(service: &InstanceService, id: &InstanceId) -> std::path::PathBuf {
        service
            .directories
            .library()
            .admit()
            .unwrap()
            .read_projection()
            .unwrap()
            .join("instances")
            .join(id.as_str())
    }

    #[tokio::test]
    async fn creation_publishes_verified_opaque_directory_and_rejects_duplicate_names() {
        let (_root, service) = fixture();
        let instance = create(&service, "Weekend / friends").await;
        let admitted = service.directories.admit(&instance.id).unwrap();
        admitted.validate_current().unwrap();
        assert_eq!(admitted.record().directory_name, instance.id.as_str());
        assert!(payload_path(&service, &instance.id).join("mods").is_dir());
        assert!(service.pending().unwrap().is_empty());
        assert!(matches!(
            service
                .create(request("Weekend / friends"), target())
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(InstanceError::NameConflict)
        ));
        assert_eq!(service.registry().list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn creation_telemetry_is_consent_gated_and_not_repeated_by_completed_retry() {
        let (_root, service) = fixture();
        let telemetry = Telemetry::configured_for_test();
        let service = service.with_telemetry(telemetry.clone());
        create(&service, "Consent disabled").await;
        assert!(telemetry.queued_events_for_test().is_empty());
        telemetry
            .consent_change()
            .await
            .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
        let instance = create(&service, "Private display label / not telemetry").await;
        assert_eq!(
            telemetry.queued_events_for_test(),
            [TelemetryEvent::InstanceCreated {
                loader: Some(TelemetryLoader::Vanilla)
            }]
        );
        assert!(
            service
                .create(request(&instance.name), target())
                .unwrap()
                .join()
                .await
                .unwrap()
                .is_err()
        );
        service
            .recover_creation(&instance.id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(telemetry.queued_events_for_test().len(), 1);
        service
            .duplicate(&instance.id, DuplicateRequest::default())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            telemetry.queued_events_for_test(),
            [TelemetryEvent::InstanceCreated {
                loader: Some(TelemetryLoader::Vanilla)
            }; 2]
        );
    }

    #[tokio::test]
    async fn duplicate_copies_independent_user_bytes_and_leaves_logs_and_screenshots_empty() {
        let (_root, service) = fixture();
        let source = create(&service, "Source").await;
        let source_path = payload_path(&service, &source.id);
        std::fs::write(source_path.join("options.txt"), b"music:0.5").unwrap();
        std::fs::write(source_path.join("mods/example.jar"), b"independent mod").unwrap();
        std::fs::create_dir(source_path.join("saves/World")).unwrap();
        std::fs::write(source_path.join("saves/World/level.dat"), b"valuable world").unwrap();
        std::fs::write(source_path.join("logs/latest.log"), b"old log").unwrap();
        std::fs::write(source_path.join("screenshots/old.png"), b"old picture").unwrap();
        let copied = service
            .duplicate(&source.id, DuplicateRequest::default())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let copied_path = payload_path(&service, &copied.id);
        assert_eq!(copied.name, "Source copy");
        assert_eq!(
            std::fs::read(copied_path.join("saves/World/level.dat")).unwrap(),
            b"valuable world"
        );
        assert_eq!(
            std::fs::read(copied_path.join("mods/example.jar")).unwrap(),
            b"independent mod"
        );
        assert_eq!(
            std::fs::read_dir(copied_path.join("logs")).unwrap().count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(copied_path.join("screenshots"))
                .unwrap()
                .count(),
            0
        );
        assert!(!copied_path.join("servers.dat").exists());
        std::fs::write(copied_path.join("options.txt"), b"music:0.0").unwrap();
        assert_eq!(
            std::fs::read(source_path.join("options.txt")).unwrap(),
            b"music:0.5"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_duplicate_compensates_its_stage_and_releases_the_source_lease() {
        let (_root, service) = fixture();
        let source = create(&service, "Source with link").await;
        let source_path = payload_path(&service, &source.id);
        std::os::unix::fs::symlink("/unrelated", source_path.join("config/link")).unwrap();
        assert!(
            service
                .duplicate(&source.id, DuplicateRequest::default())
                .unwrap()
                .join()
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(service.registry().list().unwrap().len(), 1);
        assert!(service.pending().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
        service
            .directories
            .admit(&source.id)
            .unwrap()
            .validate_current()
            .unwrap();
        assert!(
            std::fs::symlink_metadata(source_path.join("config/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn keep_files_preserves_payload_and_delete_files_only_removes_exact_target() {
        let (_root, service) = fixture();
        let kept = create(&service, "Keep").await;
        let deleted = create(&service, "Delete").await;
        let kept_path = payload_path(&service, &kept.id);
        let deleted_path = payload_path(&service, &deleted.id);
        std::fs::write(kept_path.join("options.txt"), b"user valued").unwrap();
        let operation_id = uuid::Uuid::new_v4();
        let snapshot = service
            .delete(&kept.id, DeleteIntent::KeepFiles, operation_id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.status, DeletionStatus::Removed);
        assert_eq!(
            std::fs::read(kept_path.join("options.txt")).unwrap(),
            b"user valued"
        );
        assert_eq!(
            service
                .delete(&kept.id, DeleteIntent::KeepFiles, operation_id)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap(),
            snapshot
        );
        let snapshot = service
            .delete(&deleted.id, DeleteIntent::DeleteFiles, uuid::Uuid::new_v4())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.status, DeletionStatus::Removed);
        assert!(!deleted_path.exists());
        assert!(kept_path.exists());
        assert!(service.registry().list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn admitted_target_refuses_delete_and_update_until_lease_settles() {
        let (_root, service) = fixture();
        let instance = create(&service, "Running").await;
        let retained = service.directories.admit(&instance.id).unwrap();
        assert!(matches!(
            service.update(&instance.id, Default::default()),
            Err(InstanceError::Busy)
        ));
        assert!(
            service
                .delete(
                    &instance.id,
                    DeleteIntent::DeleteFiles,
                    uuid::Uuid::new_v4()
                )
                .is_err()
        );
        drop(retained);
        assert!(service.update(&instance.id, Default::default()).is_ok());
    }

    #[tokio::test]
    async fn setup_intent_fences_the_live_instance_after_its_creator_releases_admission() {
        let (_root, service) = fixture();
        let setup = SetupIntent {
            plan_id: uuid::Uuid::new_v4().to_string(),
            request_json: "{}".into(),
        };
        let admitted = service
            .create_admitted(request("Content pending"), target(), setup)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let id = admitted.record().instance.id.clone();
        assert!(service.registry().get_live(&id).is_ok());
        drop(admitted);
        assert!(matches!(
            service.directories.admit(&id),
            Err(InstanceError::Busy)
        ));
        assert!(!service.has_unsettled_effects());
        assert!(service.has_pending_intents());
        let settlement = service.directories.admit_for_setup_settlement(&id).unwrap();
        settlement.validate_current().unwrap();
    }

    #[tokio::test]
    async fn pure_setup_intent_survives_only_exact_joined_owner_admission_release() {
        let (_root, service) = fixture();
        let admitted = service
            .create_admitted(
                request("Pending at exit"),
                target(),
                SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: "{}".into(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let id = admitted.record().instance.id.clone();
        let pending = Mutex::new(BTreeMap::from([(id.clone(), admitted)]));
        let foreign = TaskOwner::new(1).unwrap();
        foreign.try_close_idle().unwrap();
        assert!(matches!(
            super::super::setup::release_setup_admissions(
                &service,
                &pending,
                &foreign.shutdown_receipt().unwrap(),
            ),
            Err(InstanceError::Busy)
        ));
        assert_eq!(pending.lock().unwrap().len(), 1);
        assert!(service.tasks.shutdown_receipt().is_none());
        service
            .tasks
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        super::super::setup::release_setup_admissions(
            &service,
            &pending,
            &service.tasks.shutdown_receipt().unwrap(),
        )
        .unwrap();
        assert!(pending.lock().unwrap().is_empty());
        assert!(!service.has_unsettled_effects());
        assert!(service.has_pending_intents());
        assert!(matches!(
            service.directories.admit(&id),
            Err(InstanceError::Busy)
        ));
        // The setup-specific restart admission proves its old lease was released,
        // while the durable row continues to refuse normal admission.
        service.directories.admit_for_setup_settlement(&id).unwrap();
    }

    #[tokio::test]
    async fn unknown_and_removed_creation_retries_do_not_retain_phantom_authority() {
        let (_root, service) = fixture();
        let absent = InstanceId::new();
        assert!(matches!(
            service.recover_creation(&absent),
            Err(InstanceError::NotFound)
        ));
        assert!(!service.has_unsettled_effects());
        service
            .directories
            .exclusions()
            .try_acquire([absent.as_str()], [])
            .unwrap();
        let instance = create(&service, "Removed before retry").await;
        service
            .delete(&instance.id, DeleteIntent::KeepFiles, uuid::Uuid::new_v4())
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            service
                .recover_creation(&instance.id)
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(InstanceError::NotFound)
        ));
        assert!(!service.has_unsettled_effects());
        assert!(!service.has_pending_intents());
        service
            .directories
            .exclusions()
            .try_acquire([instance.id.as_str()], [])
            .unwrap();
    }

    #[tokio::test]
    async fn dropped_http_waiter_does_not_cancel_accepted_create() {
        let (_root, service) = fixture();
        drop(service.create(request("Accepted"), target()).unwrap());
        for _ in 0..100 {
            if !service.registry().list().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            service.registry().list().unwrap()[0].instance.name,
            "Accepted"
        );
    }

    #[tokio::test]
    async fn cancellation_before_effects_does_not_reserve_or_publish_an_instance() {
        let (_root, service) = fixture();
        let instance = build_instance(request("Cancelled"), &target()).unwrap();
        let pin = service.directories.library().admit().unwrap();
        let lease = service
            .directories
            .exclusions()
            .try_acquire([instance.id.as_str()], [])
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            service.publish_instance(instance, pin, lease, None, None, cancel),
            Err(InstanceError::Cancelled)
        ));
        assert!(service.registry().list().unwrap().is_empty());
        assert!(service.registry().pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn interrupted_ready_publication_replays_only_the_saved_directory_witness() {
        let (_root, service) = fixture();
        let pin = service.directories.library().admit().unwrap();
        let parent = service.ensure_parent(&pin).unwrap();
        let instance = build_instance(request("Interrupted"), &target()).unwrap();
        let stage_name = format!("stage-{}", instance.id);
        let record = service.registry().storage().transaction(|tx| -> InstanceResult<_> {
            let record = service.registry().reserve(tx, instance, &pin.library_id().to_string())?;
            tx.execute("INSERT INTO instance_creations(instance_id,record_json,stage_name,phase) VALUES(?1,?2,?3,'building')",
                params![record.instance.id.as_str(), serde_json::to_string(&record).unwrap(), stage_name])?;
            Ok(record)
        }).unwrap();
        let stage = service
            .fresh_directory(&parent, &PortableName::new_exact(&stage_name).unwrap())
            .unwrap();
        service
            .creation_phase(
                &record,
                "ready",
                Some(&stage.receipt().unwrap()),
                None,
                None,
            )
            .unwrap();
        drop(stage);
        let lease = service
            .directories
            .exclusions()
            .try_acquire([record.instance.id.as_str()], [])
            .unwrap();
        service.hold_admission(record.instance.id.clone(), pin.clone(), lease);
        assert!(service.registry().list().unwrap().is_empty());
        let recovered = service
            .recover_creation(&record.instance.id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(recovered.id, record.instance.id);
        service
            .directories
            .admit(&recovered.id)
            .unwrap()
            .validate_current()
            .unwrap();
        assert!(service.pending().unwrap().is_empty());
    }
}
