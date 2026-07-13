use super::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
#[cfg(test)]
use axial_config::generate_instance_id;
use axial_config::{
    AppPaths, Instance, InstanceRegistrySnapshot, InstanceStore, InstanceStoreError,
    derive_instance_art_seed, is_canonical_instance_id,
};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const INSTANCE_REGISTRY_LOCK_INVARIANT: &str =
    "application instance registry lock poisoned; visible state may diverge from persistence";

struct InstanceRegistryPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl InstanceRegistryPersistence {
    fn claim(paths: &AppPaths) -> Result<Self, InstanceStoreError> {
        Self::claim_with_coordinator(paths, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, InstanceStoreError> {
        let owner = coordinator
            .claim_owner(&paths.instances_file)
            .map_err(instance_persistence_error)?;
        let writer = owner
            .writer(&paths.instances_file, instance_registry_target())
            .map_err(instance_persistence_error)?;
        Ok(Self { owner, writer })
    }
}

struct InstanceRegistryState {
    visible: InstanceRegistrySnapshot,
    retry_candidate: Option<(u64, InstanceRegistrySnapshot)>,
}

struct PendingInstanceRegistryCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: InstanceRegistrySnapshot,
}

pub struct AppInstanceStore {
    paths: AppPaths,
    mutation_allowed: bool,
    state: Arc<Mutex<InstanceRegistryState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    closed: AtomicBool,
    #[cfg(test)]
    delete_result_without_removal: AtomicU8,
    persistence: InstanceRegistryPersistence,
}

impl AppInstanceStore {
    pub(crate) fn claim(source: &InstanceStore) -> Result<Self, InstanceStoreError> {
        let paths = source.paths().clone();
        let persistence = InstanceRegistryPersistence::claim(&paths)?;
        Ok(Self::from_parts(
            paths,
            source.current(),
            source.mutation_allowed(),
            persistence,
        ))
    }

    #[cfg(test)]
    pub(crate) fn claim_with_coordinator(
        source: &InstanceStore,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, InstanceStoreError> {
        let paths = source.paths().clone();
        let persistence = InstanceRegistryPersistence::claim_with_coordinator(&paths, coordinator)?;
        Ok(Self::from_parts(
            paths,
            source.current(),
            source.mutation_allowed(),
            persistence,
        ))
    }

    fn from_parts(
        paths: AppPaths,
        visible: InstanceRegistrySnapshot,
        mutation_allowed: bool,
        persistence: InstanceRegistryPersistence,
    ) -> Self {
        Self {
            paths,
            mutation_allowed,
            state: Arc::new(Mutex::new(InstanceRegistryState {
                visible,
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            delete_result_without_removal: AtomicU8::new(0),
            persistence,
        }
    }

    pub fn current(&self) -> InstanceRegistrySnapshot {
        self.state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .visible
            .clone()
    }

    pub fn list(&self) -> Vec<Instance> {
        self.current().instances
    }

    pub fn get(&self, id: &str) -> Option<Instance> {
        self.state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .visible
            .instances
            .iter()
            .find(|instance| instance.id == id)
            .cloned()
    }

    pub fn last_instance_id(&self) -> Option<String> {
        let id = self
            .state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .visible
            .last_instance_id
            .clone();
        (!id.is_empty()).then_some(id)
    }

    pub fn game_dir(&self, id: &str) -> PathBuf {
        self.paths.instances_dir.join(id)
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub(crate) async fn acquire_mutation(&self) -> Result<OwnedMutexGuard<()>, InstanceStoreError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_instance_registry_error());
        }
        if !self.mutation_allowed {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance registry mutation is latched after startup admission failure",
            )));
        }
        Ok(gate)
    }

    #[cfg(test)]
    async fn mutate<ResultValue, Mutation>(
        &self,
        mutation: Mutation,
    ) -> Result<ResultValue, InstanceStoreError>
    where
        ResultValue: Send + 'static,
        Mutation: FnOnce(&mut InstanceRegistrySnapshot) -> Result<ResultValue, InstanceStoreError>
            + Send
            + 'static,
    {
        let gate = self.acquire_mutation().await?;
        self.mutate_with_gate(mutation, gate).await
    }

    pub(crate) async fn mutate_with_gate<ResultValue, Mutation>(
        &self,
        mutation: Mutation,
        gate: OwnedMutexGuard<()>,
    ) -> Result<ResultValue, InstanceStoreError>
    where
        ResultValue: Send + 'static,
        Mutation: FnOnce(&mut InstanceRegistrySnapshot) -> Result<ResultValue, InstanceStoreError>
            + Send
            + 'static,
    {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let result = mutation(&mut candidate)?;
        candidate.validate()?;
        if candidate == self.current() {
            drop(gate);
            return Ok(result);
        }
        self.commit(candidate, result, gate).await
    }

    pub(crate) async fn create_with_gate(
        &self,
        mut instance: Instance,
        library_dir: Option<PathBuf>,
        gate: OwnedMutexGuard<()>,
    ) -> Result<Instance, InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let original_name = instance.name.clone();
        let original_seed =
            derive_instance_art_seed(&instance.id, &original_name, &instance.version_id);
        instance.name = available_create_name(&candidate, &original_name);
        if instance.name != original_name && instance.art_seed == original_seed {
            instance.art_seed =
                derive_instance_art_seed(&instance.id, &instance.name, &instance.version_id);
        }
        ensure_insertable(&candidate, &instance)?;
        candidate.instances.push(instance.clone());
        candidate.validate()?;
        prepare_new_instance_layout(self.paths.clone(), instance.id.clone(), library_dir).await?;
        match self.commit(candidate, instance.clone(), gate).await {
            Ok(instance) => Ok(instance),
            Err(error @ InstanceStoreError::TooLarge { .. }) => {
                Err(cleanup_failed_create(&self.paths, &instance.id, error).await)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn duplicate_with_gate(
        &self,
        source_id: String,
        target_id: String,
        requested_name: Option<String>,
        library_dir: Option<PathBuf>,
        gate: OwnedMutexGuard<()>,
    ) -> Result<Instance, InstanceStoreError> {
        let gate = self.reconcile_obligations(gate).await?;
        let mut candidate = self.current();
        let source = candidate
            .instances
            .iter()
            .find(|instance| instance.id == source_id)
            .cloned()
            .ok_or_else(instance_not_found_error)?;
        let name = duplicate_name(&candidate, &source.name, requested_name)?;
        let mut instance = new_instance(
            target_id,
            name,
            source.version_id.clone(),
            source.icon.clone(),
            source.accent.clone(),
        );
        instance.max_memory_mb = source.max_memory_mb;
        instance.min_memory_mb = source.min_memory_mb;
        instance.java_path = source.java_path;
        instance.window_width = source.window_width;
        instance.window_height = source.window_height;
        instance.jvm_preset = source.jvm_preset;
        instance.performance_mode = source.performance_mode;
        instance.extra_jvm_args = source.extra_jvm_args;
        instance.auto_optimize = source.auto_optimize;
        candidate.instances.push(instance.clone());
        candidate.validate()?;

        duplicate_instance_files(
            self.paths.clone(),
            source_id,
            instance.id.clone(),
            library_dir,
        )
        .await?;
        match self.commit(candidate, instance.clone(), gate).await {
            Ok(instance) => Ok(instance),
            Err(error @ InstanceStoreError::TooLarge { .. }) => {
                Err(cleanup_failed_create(&self.paths, &instance.id, error).await)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn delete_with_gate(
        &self,
        instance_id: String,
        delete_files: bool,
        gate: OwnedMutexGuard<()>,
    ) -> Result<(), InstanceStoreError> {
        let mut gate = self.reconcile_obligations(gate).await?;
        #[cfg(test)]
        match self.delete_result_without_removal.swap(0, Ordering::AcqRel) {
            1 => {
                drop(gate);
                return Ok(());
            }
            2 => {
                drop(gate);
                return Err(InstanceStoreError::Persistence(io::Error::other(
                    "injected instance registry deletion failure",
                )));
            }
            _ => {}
        }
        let mut candidate = self.current();
        if let Some(index) = candidate
            .instances
            .iter()
            .position(|instance| instance.id == instance_id)
        {
            candidate.instances.remove(index);
            if candidate.last_instance_id == instance_id {
                candidate.last_instance_id.clear();
            }
            if delete_files {
                candidate.pending_deletions.push(instance_id.clone());
            }
            candidate.validate()?;
            gate = self.commit_holding_gate(candidate, (), gate).await?.1;
        } else if !(delete_files
            && candidate
                .pending_deletions
                .iter()
                .any(|pending| pending == &instance_id))
        {
            return Err(instance_not_found_error());
        }

        if !delete_files {
            drop(gate);
            return Ok(());
        }

        gate = self.reconcile_obligations(gate).await?;
        drop(gate);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn succeed_next_delete_without_removal(&self) {
        self.delete_result_without_removal
            .store(1, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_delete_without_removal(&self) {
        self.delete_result_without_removal
            .store(2, Ordering::Release);
    }

    pub(crate) async fn close(&self) -> Result<(), InstanceStoreError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let _gate = self.reconcile_obligations(gate).await?;
        self.persistence
            .owner
            .close()
            .await
            .map_err(instance_persistence_error)?;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }

    async fn reconcile_retry(
        &self,
        gate: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, InstanceStoreError> {
        let retained = self
            .state
            .lock()
            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(gate);
        };
        let ticket = self
            .persistence
            .writer
            .retry()
            .map_err(instance_persistence_error)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "instance registry retry revision diverged from retained candidate"
        );
        self.await_commit(
            PendingInstanceRegistryCommit {
                ticket,
                revision,
                candidate,
            },
            gate,
        )
        .await
    }

    async fn reconcile_obligations(
        &self,
        gate: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, InstanceStoreError> {
        let mut gate = self.reconcile_retry(gate).await?;
        loop {
            let Some(instance_id) = self.current().pending_deletions.first().cloned() else {
                return Ok(gate);
            };
            remove_instance_directory(self.paths.clone(), instance_id.clone()).await?;
            let mut candidate = self.current();
            candidate
                .pending_deletions
                .retain(|pending| pending != &instance_id);
            candidate.validate()?;
            gate = self.commit_holding_gate(candidate, (), gate).await?.1;
        }
    }

    async fn commit<ResultValue>(
        &self,
        candidate: InstanceRegistrySnapshot,
        result: ResultValue,
        gate: OwnedMutexGuard<()>,
    ) -> Result<ResultValue, InstanceStoreError>
    where
        ResultValue: Send + 'static,
    {
        let (result, gate) = self.commit_holding_gate(candidate, result, gate).await?;
        drop(gate);
        Ok(result)
    }

    async fn commit_holding_gate<ResultValue>(
        &self,
        candidate: InstanceRegistrySnapshot,
        result: ResultValue,
        gate: OwnedMutexGuard<()>,
    ) -> Result<(ResultValue, OwnedMutexGuard<()>), InstanceStoreError>
    where
        ResultValue: Send + 'static,
    {
        let (candidate, encoded) = encode_instance_registry(candidate).await?;
        let ticket = self
            .persistence
            .writer
            .accept(encoded, WriteUrgency::Immediate, Ok)
            .map_err(instance_persistence_error)?;
        let revision = ticket.revision().get();
        let gate = self
            .await_commit(
                PendingInstanceRegistryCommit {
                    ticket,
                    revision,
                    candidate,
                },
                gate,
            )
            .await?;
        Ok((result, gate))
    }

    async fn await_commit(
        &self,
        commit: PendingInstanceRegistryCommit,
        gate: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, InstanceStoreError> {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
                    state.visible = commit.candidate.clone();
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|(revision, _)| *revision == commit.revision)
                    {
                        state.retry_candidate = None;
                    }
                    Ok(())
                }
                Err(error) => {
                    if matches!(error, PersistenceError::Write { .. }) {
                        state
                            .lock()
                            .expect(INSTANCE_REGISTRY_LOCK_INVARIANT)
                            .retry_candidate = Some((commit.revision, commit.candidate));
                    }
                    Err(instance_persistence_error(error))
                }
            };
            let _ = completed_tx.send((result, gate));
        });
        let (result, gate) = completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(io::Error::other(
                "instance registry commit observer stopped before reporting completion",
            ))
        })?;
        result?;
        Ok(gate)
    }

    #[cfg(test)]
    pub fn insert_for_test(
        &self,
        name: impl Into<String>,
        version_id: impl Into<String>,
    ) -> Result<Instance, InstanceStoreError> {
        let name = name.into();
        let version_id = version_id.into();
        let id = generate_instance_id();
        ensure_instance_layout_blocking(&self.paths, &id, None)?;
        let instance = new_instance(id, name, version_id, String::new(), String::new());
        let mut state = self.state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
        let mut candidate = state.visible.clone();
        candidate.instances.push(instance.clone());
        candidate.validate()?;
        state.visible = candidate;
        Ok(instance)
    }

    #[cfg(test)]
    pub fn replace_for_test(&self, instance: Instance) -> Result<Instance, InstanceStoreError> {
        let mut state = self.state.lock().expect(INSTANCE_REGISTRY_LOCK_INVARIANT);
        let mut candidate = state.visible.clone();
        let Some(index) = candidate
            .instances
            .iter()
            .position(|stored| stored.id == instance.id)
        else {
            return Err(instance_not_found_error());
        };
        candidate.instances[index] = instance.clone();
        candidate.validate()?;
        state.visible = candidate;
        Ok(instance)
    }
}

pub(crate) fn new_instance(
    id: String,
    name: String,
    version_id: String,
    icon: String,
    accent: String,
) -> Instance {
    let art_seed = derive_instance_art_seed(&id, &name, &version_id);
    Instance {
        id,
        name,
        version_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        last_played_at: String::new(),
        art_seed,
        max_memory_mb: 0,
        min_memory_mb: 0,
        java_path: String::new(),
        window_width: 0,
        window_height: 0,
        jvm_preset: String::new(),
        performance_mode: String::new(),
        extra_jvm_args: String::new(),
        auto_optimize: false,
        icon,
        accent,
    }
}

fn ensure_insertable(
    snapshot: &InstanceRegistrySnapshot,
    instance: &Instance,
) -> Result<(), InstanceStoreError> {
    if snapshot
        .instances
        .iter()
        .any(|stored| stored.id == instance.id || stored.name == instance.name)
        || snapshot
            .pending_deletions
            .iter()
            .any(|pending| pending == &instance.id)
    {
        return Err(instance_name_conflict_error());
    }
    Ok(())
}

fn available_create_name(snapshot: &InstanceRegistrySnapshot, requested: &str) -> String {
    if !snapshot
        .instances
        .iter()
        .any(|instance| instance.name == requested)
    {
        return requested.to_string();
    }
    for index in 1..=snapshot.instances.len().saturating_add(1) {
        let candidate = format!("{requested} ({index})");
        if !snapshot
            .instances
            .iter()
            .any(|instance| instance.name == candidate)
        {
            return candidate;
        }
    }
    unreachable!("bounded registry must leave an available create name")
}

fn duplicate_name(
    snapshot: &InstanceRegistrySnapshot,
    source_name: &str,
    requested_name: Option<String>,
) -> Result<String, InstanceStoreError> {
    if let Some(name) = requested_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    {
        if snapshot
            .instances
            .iter()
            .any(|instance| instance.name == name)
        {
            return Err(instance_name_conflict_error());
        }
        return Ok(name);
    }
    let base = format!("{source_name} copy");
    if !snapshot
        .instances
        .iter()
        .any(|instance| instance.name == base)
    {
        return Ok(base);
    }
    for index in 2.. {
        let candidate = format!("{base} {index}");
        if !snapshot
            .instances
            .iter()
            .any(|instance| instance.name == candidate)
        {
            return Ok(candidate);
        }
    }
    unreachable!("bounded registry must leave an available duplicate name")
}

pub(crate) async fn ensure_instance_layout(
    paths: AppPaths,
    instance_id: String,
    library_dir: Option<PathBuf>,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        ensure_instance_layout_blocking(&paths, &instance_id, library_dir.as_deref())
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance layout task stopped: {error}"
        )))
    })?
}

async fn prepare_new_instance_layout(
    paths: AppPaths,
    instance_id: String,
    library_dir: Option<PathBuf>,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        if !is_canonical_instance_id(&instance_id) {
            return Err(InstanceStoreError::Validation("instance id is invalid"));
        }
        ensure_instances_root(&paths)?;
        let game_dir = paths.instances_dir.join(&instance_id);
        std::fs::create_dir(&game_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                instance_name_conflict_error()
            } else {
                InstanceStoreError::Persistence(error)
            }
        })?;
        match ensure_instance_layout_blocking(&paths, &instance_id, library_dir.as_deref()) {
            Ok(()) => Ok(()),
            Err(error) => match std::fs::remove_dir_all(&game_dir) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(InstanceStoreError::Persistence(io::Error::other(
                    format!("{error}; failed to clean incomplete instance layout: {cleanup_error}"),
                ))),
            },
        }
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance layout task stopped: {error}"
        )))
    })?
}

fn ensure_instance_layout_blocking(
    paths: &AppPaths,
    instance_id: &str,
    library_dir: Option<&Path>,
) -> Result<(), InstanceStoreError> {
    if !is_canonical_instance_id(instance_id) {
        return Err(InstanceStoreError::Validation("instance id is invalid"));
    }
    ensure_instances_root(paths)?;
    let game_dir = paths.instances_dir.join(instance_id);
    ensure_directory(&game_dir)?;
    for subdir in [
        "mods",
        "saves",
        "resourcepacks",
        "shaderpacks",
        "config",
        "screenshots",
        "logs",
    ] {
        ensure_directory(&game_dir.join(subdir))?;
    }
    if let Some(library_dir) = library_dir {
        for file_name in ["options.txt", "servers.dat"] {
            copy_shared_file_if_missing(library_dir, &game_dir, file_name)
                .map_err(InstanceStoreError::Persistence)?;
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), InstanceStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance layout path is not a regular directory",
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(InstanceStoreError::Persistence)
        }
        Err(error) => Err(InstanceStoreError::Persistence(error)),
    }
}

fn ensure_instances_root(paths: &AppPaths) -> Result<(), InstanceStoreError> {
    match std::fs::symlink_metadata(&paths.config_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&paths.config_dir).map_err(InstanceStoreError::Persistence)?;
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance config root is not a regular directory",
            )));
        }
        Ok(_) => {}
        Err(error) => return Err(InstanceStoreError::Persistence(error)),
    }
    ensure_directory(&paths.instances_dir)
}

async fn duplicate_instance_files(
    paths: AppPaths,
    source_id: String,
    target_id: String,
    library_dir: Option<PathBuf>,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        ensure_instance_layout_blocking(&paths, &source_id, library_dir.as_deref())?;
        ensure_instances_root(&paths)?;
        let target_dir = paths.instances_dir.join(&target_id);
        std::fs::create_dir(&target_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                instance_name_conflict_error()
            } else {
                InstanceStoreError::Persistence(error)
            }
        })?;
        if let Err(error) =
            ensure_instance_layout_blocking(&paths, &target_id, library_dir.as_deref())
        {
            let _ = std::fs::remove_dir_all(&target_dir);
            return Err(error);
        }
        let source_dir = paths.instances_dir.join(source_id);
        let copied = (|| {
            for directory in ["mods", "saves", "resourcepacks", "shaderpacks", "config"] {
                copy_directory_contents(&source_dir.join(directory), &target_dir.join(directory))?;
            }
            for file_name in ["options.txt", "servers.dat"] {
                let source = source_dir.join(file_name);
                copy_regular_file_if_present(&source, &target_dir.join(file_name))?;
            }
            Ok(())
        })();
        if let Err(error) = copied {
            let _ = std::fs::remove_dir_all(&target_dir);
            return Err(error);
        }
        Ok(())
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance duplicate task stopped: {error}"
        )))
    })?
}

fn copy_directory_contents(source: &Path, target: &Path) -> Result<(), InstanceStoreError> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InstanceStoreError::Persistence(io::Error::new(
            io::ErrorKind::InvalidData,
            "instance resource directory is not a regular directory",
        )));
    }
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance resources cannot contain symbolic links",
            )));
        }
        if file_type.is_dir() {
            copy_directory_contents(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

async fn remove_instance_directory(
    paths: AppPaths,
    instance_id: String,
) -> Result<(), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        let directory = paths.instances_dir.join(instance_id);
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(InstanceStoreError::Persistence(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "instance directory is not a regular directory",
                )))
            }
            Ok(_) => std::fs::remove_dir_all(directory).map_err(InstanceStoreError::Persistence),
            Err(error) => Err(InstanceStoreError::Persistence(error)),
        }
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance deletion task stopped: {error}"
        )))
    })?
}

async fn cleanup_failed_create(
    paths: &AppPaths,
    instance_id: &str,
    persistence_error: InstanceStoreError,
) -> InstanceStoreError {
    match remove_instance_directory(paths.clone(), instance_id.to_string()).await {
        Ok(()) => persistence_error,
        Err(cleanup_error) => InstanceStoreError::Persistence(io::Error::other(format!(
            "{persistence_error}; failed to clean uncommitted instance files: {cleanup_error}"
        ))),
    }
}

fn copy_shared_file_if_missing(
    source_dir: &Path,
    target_dir: &Path,
    file_name: &str,
) -> io::Result<()> {
    let source = source_dir.join(file_name);
    match std::fs::symlink_metadata(&source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shared instance source is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) => return Err(error),
    }
    let target = target_dir.join(file_name);
    let mut input = std::fs::File::open(source)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(mut output) => {
            io::copy(&mut input, &mut output)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&target)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "shared instance target is not a regular file",
                ))
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

fn copy_regular_file_if_present(source: &Path, target: &Path) -> Result<(), InstanceStoreError> {
    match std::fs::symlink_metadata(source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(InstanceStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "instance source file is not a regular file",
            )));
        }
        Ok(_) => {}
        Err(error) => return Err(InstanceStoreError::Persistence(error)),
    }
    if let Ok(metadata) = std::fs::symlink_metadata(target)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(InstanceStoreError::Persistence(io::Error::new(
            io::ErrorKind::InvalidData,
            "instance target file is not a regular file",
        )));
    }
    std::fs::copy(source, target)
        .map(|_| ())
        .map_err(InstanceStoreError::Persistence)
}

async fn encode_instance_registry(
    snapshot: InstanceRegistrySnapshot,
) -> Result<(InstanceRegistrySnapshot, Vec<u8>), InstanceStoreError> {
    tokio::task::spawn_blocking(move || {
        let encoded = snapshot.encode()?;
        Ok((snapshot, encoded))
    })
    .await
    .map_err(|error| {
        InstanceStoreError::Persistence(io::Error::other(format!(
            "instance registry encoder stopped: {error}"
        )))
    })?
}

fn instance_registry_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Instance,
        "instance_registry",
        OwnershipClass::LauncherManaged,
    )
}

fn instance_persistence_error(error: impl Into<io::Error>) -> InstanceStoreError {
    InstanceStoreError::Persistence(error.into())
}

pub(crate) fn instance_not_found_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::new(
        io::ErrorKind::NotFound,
        "instance not found",
    ))
}

pub(crate) fn instance_name_conflict_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "an instance with this name already exists",
    ))
}

fn closed_instance_registry_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "instance registry persistence is closed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use axial_config::INSTANCE_REGISTRY_MAX_BYTES;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct RecordingBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
        attempted: Mutex<Vec<Vec<u8>>>,
        committed: Mutex<Vec<Vec<u8>>>,
        destinations: Mutex<Vec<PathBuf>>,
        targets: Mutex<Vec<TargetDescriptor>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl RecordingBackend {
        fn new(failures: usize) -> Arc<Self> {
            Arc::new(Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(failures),
                started: Notify::new(),
                gate: Mutex::new(None),
                attempted: Mutex::new(Vec::new()),
                committed: Mutex::new(Vec::new()),
                destinations: Mutex::new(Vec::new()),
                targets: Mutex::new(Vec::new()),
            })
        }

        fn gate_next(&self) -> Arc<WriteGate> {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            gate
        }

        async fn wait_for_attempt(&self, expected: usize) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) >= expected {
                    return;
                }
                started.await;
            }
        }

        fn committed_snapshots(&self) -> Vec<InstanceRegistrySnapshot> {
            self.committed
                .lock()
                .expect("committed registry lock")
                .iter()
                .map(|contents| {
                    serde_json::from_slice(contents).expect("decode committed instance registry")
                })
                .collect()
        }
    }

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.attempted
                .lock()
                .expect("attempted registry lock")
                .push(contents.to_vec());
            self.destinations
                .lock()
                .expect("registry destinations lock")
                .push(destination.to_path_buf());
            self.targets
                .lock()
                .expect("registry targets lock")
                .push(target.clone());
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("backend gate lock").take() {
                gate.wait();
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected instance registry write failure"));
            }
            self.committed
                .lock()
                .expect("committed registry lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }
    }

    #[tokio::test]
    async fn concurrent_mutations_derive_from_latest_committed_registry() {
        let (store, backend) = test_store("concurrent", InstanceRegistrySnapshot::default(), 0);
        let first = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .mutate(|snapshot| {
                        snapshot
                            .instances
                            .push(test_instance("0000000000000001", "First"));
                        Ok(())
                    })
                    .await
            })
        };
        let second = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .mutate(|snapshot| {
                        snapshot
                            .instances
                            .push(test_instance("0000000000000002", "Second"));
                        Ok(())
                    })
                    .await
            })
        };

        first
            .await
            .expect("first mutation task")
            .expect("first mutation");
        second
            .await
            .expect("second mutation task")
            .expect("second mutation");

        let current = store.current();
        assert_eq!(current.instances.len(), 2);
        assert!(
            current
                .instances
                .iter()
                .any(|instance| instance.name == "First")
        );
        assert!(
            current
                .instances
                .iter()
                .any(|instance| instance.name == "Second")
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn accepted_commit_survives_waiter_cancellation() {
        let (store, backend) = test_store("accepted-cancellation", snapshot_with_one(), 0);
        let gate = backend.gate_next();
        let first_store = store.clone();
        let first = tokio::spawn(async move {
            first_store
                .mutate(|snapshot| {
                    snapshot.instances[0].name = "Owned".to_string();
                    Ok(())
                })
                .await
        });
        backend.wait_for_attempt(1).await;
        first.abort();
        assert!(first.await.expect_err("cancel waiter").is_cancelled());
        gate.release();

        store
            .mutate(|snapshot| {
                snapshot.instances[0].performance_mode = "balanced".to_string();
                Ok(())
            })
            .await
            .expect("successor waits for owned commit");

        assert_eq!(store.current().instances[0].name, "Owned");
        assert_eq!(store.current().instances[0].performance_mode, "balanced");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_before_admission_drops_mutation_before_close() {
        let (store, backend) = test_store(
            "pre-admission-cancellation",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let gate = store
            .acquire_mutation()
            .await
            .expect("hold instance registry gate");
        let waiting_store = store.clone();
        let waiting = tokio::spawn(async move {
            waiting_store
                .mutate(|snapshot| {
                    snapshot
                        .instances
                        .push(test_instance("0000000000000001", "Must not commit"));
                    Ok(())
                })
                .await
        });
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(
            waiting
                .await
                .expect_err("cancel waiting mutation")
                .is_cancelled()
        );
        drop(gate);

        store
            .close()
            .await
            .expect("close after canceled pre-admission mutation");
        assert!(store.current().instances.is_empty());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_exact_bytes_commit_before_successor_derivation() {
        let (store, backend) = test_store("retry", snapshot_with_one(), 1);
        let first = store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Retained".to_string();
                Ok(())
            })
            .await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));
        assert_eq!(store.current().instances[0].name, "Original");

        store
            .mutate(|snapshot| {
                snapshot.instances[0].performance_mode = "balanced".to_string();
                Ok(())
            })
            .await
            .expect("successor reconciles retained bytes");

        let attempted = backend.attempted.lock().expect("attempted registry lock");
        assert_eq!(attempted.len(), 3);
        assert_eq!(attempted[0], attempted[1]);
        assert_ne!(attempted[1], attempted[2]);
        drop(attempted);

        let committed = backend.committed_snapshots();
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].instances[0].name, "Retained");
        assert!(committed[0].instances[0].performance_mode.is_empty());
        assert_eq!(committed[1].instances[0].name, "Retained");
        assert_eq!(committed[1].instances[0].performance_mode, "balanced");
    }

    #[tokio::test]
    async fn close_retries_retained_bytes_and_rejects_later_mutations() {
        let (store, backend) = test_store("close-retry", snapshot_with_one(), 1);
        let first = store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Retained for close".to_string();
                Ok(())
            })
            .await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));

        store
            .close()
            .await
            .expect("close retries retained registry");
        store.close().await.expect("close is idempotent");
        assert_eq!(store.current().instances[0].name, "Retained for close");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        let after_close = store
            .mutate(|snapshot| {
                snapshot.instances[0].name = "Must not commit".to_string();
                Ok(())
            })
            .await;
        assert!(matches!(
            after_close,
            Err(InstanceStoreError::Persistence(_))
        ));
        assert_eq!(store.current().instances[0].name, "Retained for close");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn validation_and_oversize_failures_accept_no_bytes() {
        let (store, backend) = test_store(
            "pre-acceptance-rejections",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let invalid = store
            .mutate(|snapshot| {
                snapshot.schema_version = u32::MAX;
                Ok(())
            })
            .await;
        assert!(matches!(invalid, Err(InstanceStoreError::Validation(_))));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);

        let oversized = store
            .mutate(|snapshot| {
                for index in 1..=100 {
                    let mut instance =
                        test_instance(&format!("{index:016x}"), &format!("Oversized {index}"));
                    instance.java_path = "j".repeat(4096);
                    instance.extra_jvm_args = "x".repeat(8192);
                    snapshot.instances.push(instance);
                }
                Ok(())
            })
            .await;
        assert!(matches!(
            oversized,
            Err(InstanceStoreError::TooLarge { .. })
        ));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(store.current(), InstanceRegistrySnapshot::default());

        let encoded_size = {
            let mut snapshot = InstanceRegistrySnapshot::default();
            for index in 1..=100 {
                let mut instance =
                    test_instance(&format!("{index:016x}"), &format!("Oversized {index}"));
                instance.java_path = "j".repeat(4096);
                instance.extra_jvm_args = "x".repeat(8192);
                snapshot.instances.push(instance);
            }
            serde_json::to_vec_pretty(&snapshot)
                .expect("encode oversized proof")
                .len() as u64
        };
        assert!(encoded_size > INSTANCE_REGISTRY_MAX_BYTES);
    }

    #[tokio::test]
    async fn invalid_create_prepares_no_directory_or_registry_bytes() {
        let (store, backend) = test_store("invalid-create", InstanceRegistrySnapshot::default(), 0);
        let instance = new_instance(
            "0000000000000001".to_string(),
            "x".repeat(129),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
        );
        let instance_path = store.game_dir(&instance.id);
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire invalid create mutation");

        let result = store.create_with_gate(instance, None, gate).await;

        assert!(matches!(result, Err(InstanceStoreError::Validation(_))));
        assert!(!instance_path.exists());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        cleanup_test_store(&store);
    }

    #[tokio::test]
    async fn failed_accepted_create_retains_directory_for_exact_retry() {
        let (store, backend) =
            test_store("create-exact-retry", InstanceRegistrySnapshot::default(), 1);
        let instance = test_instance("0000000000000001", "Retained create");
        let instance_path = store.game_dir(&instance.id);
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire create mutation");

        let first = store.create_with_gate(instance.clone(), None, gate).await;
        assert!(matches!(first, Err(InstanceStoreError::Persistence(_))));
        assert!(instance_path.is_dir());
        assert!(store.current().instances.is_empty());

        store
            .mutate(|_| Ok(()))
            .await
            .expect("successor reconciles accepted create");
        assert_eq!(store.current().instances, vec![instance]);
        assert!(instance_path.is_dir());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        let attempted = backend.attempted.lock().expect("attempted registry lock");
        assert_eq!(attempted[0], attempted[1]);
        drop(attempted);
        cleanup_test_store(&store);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_rejects_symlinked_shared_file_and_cleans_uncommitted_directory() {
        use std::os::unix::fs::symlink;

        let (store, backend) = test_store(
            "create-symlink-source",
            InstanceRegistrySnapshot::default(),
            0,
        );
        let library_dir = store.paths().config_dir.join("library-source");
        std::fs::create_dir_all(&library_dir).expect("create library source");
        let outside = store.paths().config_dir.join("outside-options.txt");
        std::fs::write(&outside, b"outside").expect("write outside source");
        symlink(&outside, library_dir.join("options.txt")).expect("symlink shared options");
        let instance = test_instance("0000000000000001", "Symlink source");
        let instance_path = store.game_dir(&instance.id);
        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire create mutation");

        let result = store
            .create_with_gate(instance, Some(library_dir), gate)
            .await;

        assert!(matches!(result, Err(InstanceStoreError::Persistence(_))));
        assert!(!instance_path.exists());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        cleanup_test_store(&store);
    }

    #[tokio::test]
    async fn close_retries_pending_deletion_cleanup_and_commits_marker_removal() {
        let snapshot = snapshot_with_one();
        let instance_id = snapshot.instances[0].id.clone();
        let (store, backend) = test_store("pending-deletion", snapshot, 0);
        std::fs::create_dir_all(&store.paths().instances_dir).expect("create instances root");
        let instance_path = store.game_dir(&instance_id);
        std::fs::write(&instance_path, b"blocks directory deletion")
            .expect("seed non-directory instance path");

        let gate = store
            .acquire_mutation()
            .await
            .expect("acquire deletion mutation");
        let deletion = store
            .delete_with_gate(instance_id.clone(), true, gate)
            .await;
        assert!(matches!(deletion, Err(InstanceStoreError::Persistence(_))));
        assert!(store.current().instances.is_empty());
        assert_eq!(store.current().pending_deletions, vec![instance_id.clone()]);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);

        std::fs::remove_file(&instance_path).expect("remove blocking instance file");
        std::fs::create_dir(&instance_path).expect("create retryable instance directory");
        std::fs::write(instance_path.join("owned.txt"), b"owned").expect("seed instance directory");

        store
            .close()
            .await
            .expect("close retries pending deletion cleanup");
        assert!(!instance_path.exists());
        assert!(store.current().pending_deletions.is_empty());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        let committed = backend.committed_snapshots();
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].pending_deletions, vec![instance_id]);
        assert!(committed[1].pending_deletions.is_empty());
        cleanup_test_store(&store);
    }

    #[tokio::test]
    async fn claim_owns_launcher_managed_registry_target_and_destination() {
        let paths = test_paths("ownership");
        let source =
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("instance source");
        let backend = RecordingBackend::new(0);
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = AppInstanceStore::claim_with_coordinator(&source, coordinator.clone())
            .expect("claim instance registry");
        assert!(matches!(
            AppInstanceStore::claim_with_coordinator(&source, coordinator),
            Err(InstanceStoreError::Persistence(_))
        ));

        store
            .mutate(|snapshot| {
                snapshot
                    .instances
                    .push(test_instance("0000000000000001", "Owned"));
                Ok(())
            })
            .await
            .expect("persist owned registry");

        assert_eq!(
            *backend.targets.lock().expect("registry targets lock"),
            vec![instance_registry_target()]
        );
        assert_eq!(
            *backend
                .destinations
                .lock()
                .expect("registry destinations lock"),
            vec![paths.instances_file]
        );
    }

    fn test_store(
        name: &str,
        snapshot: InstanceRegistrySnapshot,
        failures: usize,
    ) -> (Arc<AppInstanceStore>, Arc<RecordingBackend>) {
        let paths = test_paths(name);
        let source = InstanceStore::from_snapshot(paths, snapshot).expect("instance source");
        let backend = RecordingBackend::new(failures);
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = AppInstanceStore::claim_with_coordinator(&source, coordinator)
            .expect("claim instance store");
        (Arc::new(store), backend)
    }

    fn cleanup_test_store(store: &AppInstanceStore) {
        if let Some(root) = store.paths().config_dir.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn snapshot_with_one() -> InstanceRegistrySnapshot {
        InstanceRegistrySnapshot::new(
            vec![test_instance("0000000000000001", "Original")],
            String::new(),
            Vec::new(),
        )
        .expect("valid instance snapshot")
    }

    fn test_instance(id: &str, name: &str) -> Instance {
        new_instance(
            id.to_string(),
            name.to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
        )
    }

    fn test_paths(name: &str) -> AppPaths {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "axial-instance-registry-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
