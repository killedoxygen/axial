#[cfg(test)]
use crate::execution::persistence::PersistenceCoordinator;
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceOwnerLease, WriteUrgency,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use axial_config::{AppPaths, validate_username};
use axial_minecraft::offline_uuid;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const ACCOUNT_STORE_SCHEMA: &str = "axial.accounts";
const ACCOUNT_STORE_SCHEMA_VERSION: u32 = 1;
const ACCOUNT_STORE_LOCK_INVARIANT: &str =
    "launcher account store lock poisoned; committed and persisted state may diverge";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherAccountKind {
    Microsoft,
    Offline,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherAccountRecord {
    pub account_id: String,
    pub kind: LauncherAccountKind,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minecraft_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline_uuid: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountIndex {
    schema: String,
    schema_version: u32,
    active_account_id: Option<String>,
    accounts: Vec<LauncherAccountRecord>,
}

struct AccountPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl AccountPersistence {
    fn claim(index_path: &Path) -> io::Result<Self> {
        let owner = PersistenceOwnerLease::claim(index_path).map_err(io::Error::from)?;
        Self::writer_for_owner(index_path, owner)
    }

    #[cfg(test)]
    fn claim_with_coordinator(
        index_path: &Path,
        coordinator: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let owner = coordinator
            .claim_owner(index_path)
            .map_err(io::Error::from)?;
        Self::writer_for_owner(index_path, owner)
    }

    fn writer_for_owner(index_path: &Path, owner: PersistenceOwnerLease) -> io::Result<Self> {
        let writer = owner
            .writer(index_path, account_index_target())
            .map_err(io::Error::from)?;
        Ok(Self { owner, writer })
    }
}

struct AccountState {
    visible: AccountIndex,
    retry_candidate: Option<(u64, AccountIndex)>,
}

struct PendingAccountCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: AccountIndex,
}

struct AccountMutation {
    guard: OwnedMutexGuard<()>,
    reconciliation: Option<AccountReconciliation>,
}

struct AccountReconciliation {
    before: AccountIndex,
    after: AccountIndex,
}

struct OfflineRenameCandidate {
    index: AccountIndex,
    record: LauncherAccountRecord,
    changed: bool,
}

pub struct LauncherAccountStore {
    state: Arc<Mutex<AccountState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: Option<AccountPersistence>,
}

impl LauncherAccountStore {
    pub fn load_from_paths(paths: &AppPaths) -> Self {
        Self::try_load_from_paths(paths).unwrap_or_else(|error| {
            panic!("failed to initialize launcher account persistence: {error}")
        })
    }

    pub fn try_load_from_paths(paths: &AppPaths) -> io::Result<Self> {
        let index_path = paths.accounts_file().to_path_buf();
        let persistence = AccountPersistence::claim(&index_path)?;
        Ok(Self::load_with_persistence(&index_path, Some(persistence)))
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let index_path = paths.accounts_file().to_path_buf();
        let persistence = AccountPersistence::claim_with_coordinator(&index_path, coordinator)?;
        Ok(Self::load_with_persistence(&index_path, Some(persistence)))
    }

    fn load_with_persistence(index_path: &Path, persistence: Option<AccountPersistence>) -> Self {
        let index = match load_index(index_path) {
            Ok(index) => index,
            Err(error) => {
                tracing::warn!("account store could not be loaded; starting empty: {error}");
                empty_index()
            }
        };

        Self {
            state: Arc::new(Mutex::new(AccountState {
                visible: index,
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence,
        }
    }

    pub fn list(&self) -> Vec<LauncherAccountRecord> {
        let state = self.state.lock().expect(ACCOUNT_STORE_LOCK_INVARIANT);
        let mut accounts = state.visible.accounts.clone();
        accounts.sort_by(|left, right| {
            kind_order(left.kind)
                .cmp(&kind_order(right.kind))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.display_name.cmp(&right.display_name))
                .then_with(|| left.account_id.cmp(&right.account_id))
        });
        accounts
    }

    pub fn active_account_id(&self) -> Option<String> {
        self.state
            .lock()
            .expect(ACCOUNT_STORE_LOCK_INVARIANT)
            .visible
            .active_account_id
            .clone()
    }

    pub fn active_account(&self) -> Option<LauncherAccountRecord> {
        let state = self.state.lock().expect(ACCOUNT_STORE_LOCK_INVARIANT);
        let active_account_id = state.visible.active_account_id.as_deref()?;
        state
            .visible
            .accounts
            .iter()
            .find(|account| account.account_id == active_account_id)
            .cloned()
    }

    pub async fn select(&self, account_id: &str) -> io::Result<Option<LauncherAccountRecord>> {
        let AccountMutation {
            guard: mutation, ..
        } = self.begin_mutation().await?;
        let mut next = self.committed_candidate()?;
        let Some(account) = next
            .accounts
            .iter()
            .find(|account| account.account_id == account_id)
            .cloned()
        else {
            return Ok(None);
        };
        if next.active_account_id.as_deref() == Some(account.account_id.as_str()) {
            return Ok(Some(account));
        }
        next.active_account_id = Some(account.account_id.clone());
        self.commit(next, mutation).await?;
        Ok(Some(account))
    }

    pub async fn upsert_microsoft_account(
        &self,
        login_id: &str,
        minecraft_profile_id: &str,
        display_name: &str,
    ) -> io::Result<LauncherAccountRecord> {
        self.upsert_microsoft_account_with_selection(
            login_id,
            minecraft_profile_id,
            display_name,
            true,
        )
        .await
    }

    pub async fn sync_microsoft_account(
        &self,
        login_id: &str,
        minecraft_profile_id: &str,
        display_name: &str,
        select: bool,
    ) -> io::Result<LauncherAccountRecord> {
        self.upsert_microsoft_account_with_selection(
            login_id,
            minecraft_profile_id,
            display_name,
            select,
        )
        .await
    }

    async fn upsert_microsoft_account_with_selection(
        &self,
        login_id: &str,
        minecraft_profile_id: &str,
        display_name: &str,
        select: bool,
    ) -> io::Result<LauncherAccountRecord> {
        let login_id = require_nonblank(login_id, "login id")?;
        let profile_id = require_nonblank(minecraft_profile_id, "minecraft profile id")?;
        let display_name = require_nonblank(display_name, "display name")?;
        let now = chrono::Utc::now().to_rfc3339();

        let AccountMutation {
            guard: mutation, ..
        } = self.begin_mutation().await?;
        let mut next = self.committed_candidate()?;
        let existing_position = next.accounts.iter().position(|account| {
            account.kind == LauncherAccountKind::Microsoft
                && (account.login_id.as_deref() == Some(login_id.as_str())
                    || account.minecraft_profile_id.as_deref() == Some(profile_id.as_str()))
        });
        let account_id = existing_position
            .and_then(|position| next.accounts.get(position))
            .map(|account| account.account_id.clone())
            .unwrap_or_else(|| microsoft_account_id(&login_id));
        let created_at = existing_position
            .and_then(|position| next.accounts.get(position))
            .map(|account| account.created_at.clone())
            .unwrap_or_else(|| now.clone());
        let record = LauncherAccountRecord {
            account_id,
            kind: LauncherAccountKind::Microsoft,
            display_name,
            login_id: Some(login_id),
            minecraft_profile_id: Some(profile_id),
            offline_uuid: None,
            created_at,
            updated_at: now,
        };
        if let Some(position) = existing_position {
            next.accounts[position] = record.clone();
        } else {
            next.accounts.push(record.clone());
        }
        if select {
            next.active_account_id = Some(record.account_id.clone());
        }
        self.commit(next, mutation).await?;
        Ok(record)
    }

    pub async fn create_offline_account(
        &self,
        username: &str,
    ) -> io::Result<LauncherAccountRecord> {
        let display_name =
            validate_username(username).map_err(|error| invalid_input(error.to_string()))?;
        let uuid = offline_uuid(&display_name);
        let account_id = offline_account_id(&uuid);
        let now = chrono::Utc::now().to_rfc3339();

        let AccountMutation {
            guard: mutation, ..
        } = self.begin_mutation().await?;
        let mut next = self.committed_candidate()?;
        let existing_position = next.accounts.iter().position(|account| {
            account.kind == LauncherAccountKind::Offline
                && account.offline_uuid.as_deref() == Some(uuid.as_str())
        });
        let created_at = existing_position
            .and_then(|position| next.accounts.get(position))
            .map(|account| account.created_at.clone())
            .unwrap_or_else(|| now.clone());
        let record = LauncherAccountRecord {
            account_id,
            kind: LauncherAccountKind::Offline,
            display_name,
            login_id: None,
            minecraft_profile_id: None,
            offline_uuid: Some(uuid),
            created_at,
            updated_at: now,
        };
        if let Some(position) = existing_position {
            next.accounts[position] = record.clone();
        } else {
            next.accounts.push(record.clone());
        }
        next.active_account_id = Some(record.account_id.clone());
        self.commit(next, mutation).await?;
        Ok(record)
    }

    pub async fn rename_offline_account(
        &self,
        account_id: &str,
        username: &str,
    ) -> io::Result<Option<LauncherAccountRecord>> {
        let display_name =
            validate_username(username).map_err(|error| invalid_input(error.to_string()))?;
        let uuid = offline_uuid(&display_name);
        let now = chrono::Utc::now().to_rfc3339();

        let AccountMutation {
            guard: mutation,
            reconciliation,
        } = self.begin_mutation().await?;
        let current = self.committed_candidate()?;
        let Some(rename) =
            rename_offline_candidate(&current, account_id, &display_name, &uuid, &now)?
        else {
            return Ok(reconciled_rename_outcome(
                reconciliation.as_ref(),
                account_id,
                &display_name,
                &uuid,
            ));
        };
        if !rename.changed {
            return Ok(Some(rename.record));
        }
        self.commit(rename.index, mutation).await?;
        Ok(Some(rename.record))
    }

    pub async fn remove(&self, account_id: &str) -> io::Result<Option<LauncherAccountRecord>> {
        let AccountMutation {
            guard: mutation,
            reconciliation,
        } = self.begin_mutation().await?;
        let current = self.committed_candidate()?;
        let Some((next, removed)) = remove_account_candidate(&current, account_id) else {
            return Ok(reconciled_remove_outcome(
                reconciliation.as_ref(),
                account_id,
            ));
        };
        self.commit(next, mutation).await?;
        Ok(Some(removed))
    }

    pub async fn remove_microsoft_login(
        &self,
        login_id: &str,
    ) -> io::Result<Option<LauncherAccountRecord>> {
        let AccountMutation {
            guard: mutation,
            reconciliation,
        } = self.begin_mutation().await?;
        let current = self.committed_candidate()?;
        let Some((next, removed)) = remove_microsoft_login_candidate(&current, login_id) else {
            return Ok(reconciled_remove_microsoft_login_outcome(
                reconciliation.as_ref(),
                login_id,
            ));
        };
        self.commit(next, mutation).await?;
        Ok(Some(removed))
    }

    pub async fn remove_all_microsoft_accounts(&self) -> io::Result<bool> {
        let AccountMutation {
            guard: mutation,
            reconciliation,
        } = self.begin_mutation().await?;
        let current = self.committed_candidate()?;
        let Some(next) = remove_all_microsoft_candidate(&current) else {
            return Ok(reconciled_remove_all_microsoft_outcome(
                reconciliation.as_ref(),
            ));
        };
        self.commit(next, mutation).await?;
        Ok(true)
    }

    pub async fn retry(&self) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        if self
            .state
            .lock()
            .expect(ACCOUNT_STORE_LOCK_INVARIANT)
            .retry_candidate
            .is_none()
        {
            return Err(retry_unavailable());
        }
        let mutation = self.reconcile_retry_candidate(mutation).await?;
        drop(mutation);
        Ok(())
    }

    pub async fn flush(&self) -> io::Result<()> {
        let _mutation = self.begin_mutation().await?;
        if let Some(persistence) = &self.persistence {
            persistence.owner.flush().await.map_err(io::Error::from)?;
        }
        Ok(())
    }

    pub async fn close(&self) -> io::Result<()> {
        let _mutation = self.begin_mutation().await?;
        if let Some(persistence) = &self.persistence {
            persistence.owner.close().await.map_err(io::Error::from)?;
        }
        Ok(())
    }

    async fn begin_mutation(&self) -> io::Result<AccountMutation> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        self.reconcile_retry_candidate(mutation).await
    }

    async fn reconcile_retry_candidate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<AccountMutation> {
        let (before, retained) = {
            let state = self.state.lock().expect(ACCOUNT_STORE_LOCK_INVARIANT);
            (
                state.visible.clone(),
                state
                    .retry_candidate
                    .as_ref()
                    .map(|(revision, candidate)| (*revision, candidate.clone())),
            )
        };
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(AccountMutation {
                guard: mutation,
                reconciliation: None,
            });
        };
        let Some(persistence) = &self.persistence else {
            self.state
                .lock()
                .expect(ACCOUNT_STORE_LOCK_INVARIANT)
                .retry_candidate = None;
            return Ok(AccountMutation {
                guard: mutation,
                reconciliation: None,
            });
        };
        let ticket = persistence.writer.retry().map_err(io::Error::from)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "launcher account retry revision diverged from the retained candidate"
        );
        let after = candidate.clone();
        let guard = self
            .await_commit_holding_gate(
                Some(PendingAccountCommit {
                    ticket,
                    revision,
                    candidate,
                }),
                mutation,
            )
            .await?;
        Ok(AccountMutation {
            guard,
            reconciliation: Some(AccountReconciliation { before, after }),
        })
    }

    fn committed_candidate(&self) -> io::Result<AccountIndex> {
        let state = self.state.lock().expect(ACCOUNT_STORE_LOCK_INVARIANT);
        if state.retry_candidate.is_some() {
            return Err(retry_required());
        }
        Ok(state.visible.clone())
    }

    async fn commit(
        &self,
        candidate: AccountIndex,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<()> {
        let Some(persistence) = &self.persistence else {
            self.state
                .lock()
                .expect(ACCOUNT_STORE_LOCK_INVARIANT)
                .visible = candidate;
            drop(mutation);
            return Ok(());
        };
        let ticket = persistence
            .writer
            .accept(candidate.clone(), WriteUrgency::Immediate, encode_index)
            .map_err(io::Error::from)?;
        let revision = ticket.revision().get();
        self.await_commit(
            Some(PendingAccountCommit {
                ticket,
                revision,
                candidate,
            }),
            mutation,
        )
        .await
    }

    async fn await_commit(
        &self,
        commit: Option<PendingAccountCommit>,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<()> {
        let mutation = self.await_commit_holding_gate(commit, mutation).await?;
        drop(mutation);
        Ok(())
    }

    async fn await_commit_holding_gate(
        &self,
        commit: Option<PendingAccountCommit>,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let Some(commit) = commit else {
            return Ok(mutation);
        };
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(ACCOUNT_STORE_LOCK_INVARIANT);
                    state.visible = commit.candidate;
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
                    state
                        .lock()
                        .expect(ACCOUNT_STORE_LOCK_INVARIANT)
                        .retry_candidate = Some((commit.revision, commit.candidate));
                    Err(io::Error::from(error))
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx.await.map_err(|_| {
            io::Error::other("launcher account commit observer stopped before reporting completion")
        })?;
        result?;
        Ok(mutation)
    }
}

impl Default for LauncherAccountStore {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(AccountState {
                visible: empty_index(),
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: None,
        }
    }
}

pub fn microsoft_account_id(login_id: &str) -> String {
    format!("microsoft-{}", account_id_component(login_id))
}

pub fn offline_account_id(uuid: &str) -> String {
    format!("offline-{}", account_id_component(uuid))
}

fn rename_offline_candidate(
    current: &AccountIndex,
    account_id: &str,
    display_name: &str,
    uuid: &str,
    updated_at: &str,
) -> io::Result<Option<OfflineRenameCandidate>> {
    let mut next = current.clone();
    let Some(position) = next.accounts.iter().position(|account| {
        account.account_id == account_id && account.kind == LauncherAccountKind::Offline
    }) else {
        return Ok(None);
    };
    if next.accounts[position].display_name == display_name
        && next.accounts[position].offline_uuid.as_deref() == Some(uuid)
    {
        return Ok(Some(OfflineRenameCandidate {
            index: next.clone(),
            record: next.accounts[position].clone(),
            changed: false,
        }));
    }
    if next
        .accounts
        .iter()
        .enumerate()
        .any(|(candidate_position, account)| {
            candidate_position != position
                && account.kind == LauncherAccountKind::Offline
                && account.offline_uuid.as_deref() == Some(uuid)
        })
    {
        return Err(invalid_input("offline account already exists"));
    }

    let record = LauncherAccountRecord {
        account_id: offline_account_id(uuid),
        kind: LauncherAccountKind::Offline,
        display_name: display_name.to_string(),
        login_id: None,
        minecraft_profile_id: None,
        offline_uuid: Some(uuid.to_string()),
        created_at: next.accounts[position].created_at.clone(),
        updated_at: updated_at.to_string(),
    };
    let was_active = next.active_account_id.as_deref() == Some(account_id);
    next.accounts[position] = record.clone();
    if was_active {
        next.active_account_id = Some(record.account_id.clone());
    }
    Ok(Some(OfflineRenameCandidate {
        index: next,
        record,
        changed: true,
    }))
}

fn reconciled_rename_outcome(
    reconciliation: Option<&AccountReconciliation>,
    account_id: &str,
    display_name: &str,
    uuid: &str,
) -> Option<LauncherAccountRecord> {
    let reconciliation = reconciliation?;
    let next_account_id = offline_account_id(uuid);
    let updated_at = reconciliation
        .after
        .accounts
        .iter()
        .find(|account| {
            account.account_id == next_account_id
                && account.kind == LauncherAccountKind::Offline
                && account.display_name == display_name
                && account.offline_uuid.as_deref() == Some(uuid)
        })?
        .updated_at
        .clone();
    let rename = rename_offline_candidate(
        &reconciliation.before,
        account_id,
        display_name,
        uuid,
        &updated_at,
    )
    .ok()??;
    (rename.changed && rename.index == reconciliation.after).then_some(rename.record)
}

fn remove_account_candidate(
    current: &AccountIndex,
    account_id: &str,
) -> Option<(AccountIndex, LauncherAccountRecord)> {
    let mut next = current.clone();
    let position = next
        .accounts
        .iter()
        .position(|account| account.account_id == account_id)?;
    let removed = next.accounts.remove(position);
    if next.active_account_id.as_deref() == Some(removed.account_id.as_str()) {
        next.active_account_id = next
            .accounts
            .iter()
            .find(|account| account.kind == LauncherAccountKind::Offline)
            .map(|account| account.account_id.clone());
    }
    Some((next, removed))
}

fn reconciled_remove_outcome(
    reconciliation: Option<&AccountReconciliation>,
    account_id: &str,
) -> Option<LauncherAccountRecord> {
    let reconciliation = reconciliation?;
    let (candidate, removed) = remove_account_candidate(&reconciliation.before, account_id)?;
    (candidate == reconciliation.after).then_some(removed)
}

fn remove_microsoft_login_candidate(
    current: &AccountIndex,
    login_id: &str,
) -> Option<(AccountIndex, LauncherAccountRecord)> {
    let account_id = current
        .accounts
        .iter()
        .find(|account| {
            account.kind == LauncherAccountKind::Microsoft
                && account.login_id.as_deref() == Some(login_id)
        })?
        .account_id
        .clone();
    remove_account_candidate(current, &account_id)
}

fn reconciled_remove_microsoft_login_outcome(
    reconciliation: Option<&AccountReconciliation>,
    login_id: &str,
) -> Option<LauncherAccountRecord> {
    let reconciliation = reconciliation?;
    let (candidate, removed) = remove_microsoft_login_candidate(&reconciliation.before, login_id)?;
    (candidate == reconciliation.after).then_some(removed)
}

fn remove_all_microsoft_candidate(current: &AccountIndex) -> Option<AccountIndex> {
    let mut next = current.clone();
    let previous_len = next.accounts.len();
    next.accounts
        .retain(|account| account.kind != LauncherAccountKind::Microsoft);
    let removed_any = next.accounts.len() != previous_len;
    let active_missing = next.active_account_id.as_ref().is_some_and(|account_id| {
        !next
            .accounts
            .iter()
            .any(|account| &account.account_id == account_id)
    });
    if active_missing {
        next.active_account_id = next
            .accounts
            .iter()
            .find(|account| account.kind == LauncherAccountKind::Offline)
            .map(|account| account.account_id.clone());
    }
    (removed_any || active_missing).then_some(next)
}

fn reconciled_remove_all_microsoft_outcome(reconciliation: Option<&AccountReconciliation>) -> bool {
    let Some(reconciliation) = reconciliation else {
        return false;
    };
    remove_all_microsoft_candidate(&reconciliation.before)
        .is_some_and(|candidate| candidate == reconciliation.after)
}

fn load_index(path: &Path) -> io::Result<AccountIndex> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_index()),
        Err(error) => return Err(error),
    };
    let index: AccountIndex = serde_json::from_str(&data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if index.schema != ACCOUNT_STORE_SCHEMA || index.schema_version != ACCOUNT_STORE_SCHEMA_VERSION
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported account store schema",
        ));
    }
    if index
        .active_account_id
        .as_deref()
        .is_some_and(|account_id| {
            !index
                .accounts
                .iter()
                .any(|account| account.account_id == account_id)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "active account is missing from account store",
        ));
    }
    Ok(index)
}

fn empty_index() -> AccountIndex {
    AccountIndex {
        schema: ACCOUNT_STORE_SCHEMA.to_string(),
        schema_version: ACCOUNT_STORE_SCHEMA_VERSION,
        active_account_id: None,
        accounts: Vec::new(),
    }
}

fn account_index_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Account,
        "launcher_accounts",
        OwnershipClass::LauncherManaged,
    )
}

fn encode_index(index: AccountIndex) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&index)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn kind_order(kind: LauncherAccountKind) -> u8 {
    match kind {
        LauncherAccountKind::Microsoft => 0,
        LauncherAccountKind::Offline => 1,
    }
}

fn require_nonblank(value: &str, label: &str) -> io::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid_input(format!("{label} is required")));
    }
    Ok(trimmed.to_string())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn retry_required() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "launcher account persistence retry is required",
    )
}

fn retry_unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "launcher account persistence retry is unavailable",
    )
}

fn account_id_component(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    struct RecordingFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        committed: Mutex<Vec<Vec<u8>>>,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    struct WriteGateHandle(Arc<WriteGate>);

    impl RecordingFileBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
                committed: Mutex::new(Vec::new()),
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }

        fn committed_indexes(&self) -> Vec<AccountIndex> {
            self.committed
                .lock()
                .expect("committed snapshot lock")
                .iter()
                .map(|contents| serde_json::from_slice(contents).expect("decode account snapshot"))
                .collect()
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

        fn gate_next(&self) -> WriteGateHandle {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            WriteGateHandle(gate)
        }
    }

    impl AtomicWriteBackend for RecordingFileBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
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
                return Err(io::Error::other("injected account snapshot write failure"));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)?;
            self.committed
                .lock()
                .expect("committed snapshot lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }
    }

    impl WriteGateHandle {
        fn release(&self) {
            self.0.release();
        }
    }

    impl Drop for WriteGateHandle {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn persistence_fixture(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<RecordingFileBackend>,
        PersistenceCoordinator,
        LauncherAccountStore,
    ) {
        let root = test_root(name);
        let paths = test_paths(&root);
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator.clone())
                .expect("claim account persistence");
        (root, paths, backend, coordinator, store)
    }

    #[tokio::test]
    async fn account_store_keeps_same_name_online_and_offline_distinct() {
        let (root, _, _, _, store) = persistence_fixture("same-name");
        let microsoft = store
            .upsert_microsoft_account("msa-1", "profile-1", "Mateo")
            .await
            .expect("upsert microsoft account");
        let offline = store
            .create_offline_account("Mateo")
            .await
            .expect("create offline account");

        assert_ne!(microsoft.account_id, offline.account_id);
        assert_eq!(store.active_account().as_ref(), Some(&offline));
        store.close().await.expect("close account store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn accepted_snapshot_is_hidden_until_physical_commit() {
        let (root, _, backend, _, store) = persistence_fixture("gated-visibility");
        let store = Arc::new(store);
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task =
            tokio::spawn(async move { task_store.create_offline_account("LocalUser").await });

        backend.wait_for_attempt(1).await;
        assert!(store.list().is_empty());
        gate.release();
        let account = task
            .await
            .expect("join account mutation")
            .expect("commit account");

        assert_eq!(store.active_account(), Some(account));
        store.close().await.expect("close account store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_caller_cannot_cancel_promotion_or_release_serialization() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("cancelled-caller");
        let store = Arc::new(store);
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task =
            tokio::spawn(async move { task_store.create_offline_account("LocalUser").await });

        backend.wait_for_attempt(1).await;
        task.abort();
        assert!(task.await.expect_err("caller is cancelled").is_cancelled());
        assert!(store.list().is_empty());
        gate.release();
        store
            .flush()
            .await
            .expect("observer releases mutation gate");

        assert_eq!(store.list().len(), 1);
        store.close().await.expect("close first account store");
        drop(store);
        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload committed account");
        assert_eq!(reloaded.list().len(), 1);
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_reconciliation_commits_exact_candidate_before_releasing_gate() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("cancelled-reconciliation");
        let store = Arc::new(store);
        backend.fail_next();
        store
            .create_offline_account("FirstUser")
            .await
            .expect_err("initial write fails");
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task =
            tokio::spawn(async move { task_store.create_offline_account("LaterUser").await });

        backend.wait_for_attempt(2).await;
        task.abort();
        assert!(task.await.expect_err("caller is cancelled").is_cancelled());
        assert!(store.list().is_empty());
        gate.release();
        store
            .flush()
            .await
            .expect("retry observer releases mutation gate");

        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].display_name, "FirstUser");
        assert_eq!(backend.committed_indexes().len(), 1);
        store.close().await.expect("close account store");
        drop(store);
        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload reconciled account");
        assert_eq!(reloaded.list()[0].display_name, "FirstUser");
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn later_mutation_reconciles_exact_failed_candidate_before_deriving_its_snapshot() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("exact-retry");
        backend.fail_next();
        let error = store
            .create_offline_account("FirstUser")
            .await
            .expect_err("first write fails");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(store.list().is_empty());

        let later = store
            .create_offline_account("LaterUser")
            .await
            .expect("later mutation reconciles and commits");

        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        let committed = backend.committed_indexes();
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].accounts.len(), 1);
        assert_eq!(committed[0].accounts[0].display_name, "FirstUser");
        assert_eq!(committed[1].accounts.len(), 2);
        assert!(
            committed[1]
                .accounts
                .iter()
                .any(|account| account.display_name == "FirstUser")
        );
        assert!(
            committed[1]
                .accounts
                .iter()
                .any(|account| account == &later)
        );
        store.close().await.expect("close account store");
        drop(store);
        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload reconciled accounts");
        assert_eq!(reloaded.list().len(), 2);
        assert!(
            reloaded
                .list()
                .iter()
                .any(|account| account.display_name == "FirstUser")
        );
        assert!(reloaded.list().iter().any(|account| account == &later));
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn identical_rename_retry_reports_committed_success_and_reloads() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("rename-retry");
        let original = store
            .create_offline_account("OldName")
            .await
            .expect("create original account");
        backend.fail_next();
        store
            .rename_offline_account(&original.account_id, "NewName")
            .await
            .expect_err("rename write fails");

        let renamed = store
            .rename_offline_account(&original.account_id, "NewName")
            .await
            .expect("retry rename")
            .expect("reconciled rename reports the account");

        assert_eq!(renamed.display_name, "NewName");
        assert_ne!(renamed.account_id, original.account_id);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(backend.committed_indexes().len(), 2);
        store.close().await.expect("close account store");
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload renamed account");
        assert_eq!(reloaded.active_account(), Some(renamed));
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn identical_remove_retry_reports_committed_success_and_reloads() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("remove-retry");
        let account = store
            .create_offline_account("LocalUser")
            .await
            .expect("create account");
        backend.fail_next();
        store
            .remove(&account.account_id)
            .await
            .expect_err("remove write fails");

        assert_eq!(
            store
                .remove(&account.account_id)
                .await
                .expect("retry remove"),
            Some(account)
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(backend.committed_indexes().len(), 2);
        store.close().await.expect("close account store");
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload removed account");
        assert!(reloaded.list().is_empty());
        assert_eq!(reloaded.active_account(), None);
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unrelated_missing_remove_stays_noop_after_reconciliation() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("unrelated-remove-after-retry");
        let account = store
            .create_offline_account("LocalUser")
            .await
            .expect("create account");
        backend.fail_next();
        store
            .remove(&account.account_id)
            .await
            .expect_err("remove write fails");

        assert_eq!(
            store
                .remove("missing-account")
                .await
                .expect("missing remove"),
            None
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        assert!(store.list().is_empty());
        store.close().await.expect("close account store");
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload reconciled removal");
        assert!(reloaded.list().is_empty());
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn identical_microsoft_login_remove_retry_reports_committed_success_and_reloads() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("microsoft-remove-retry");
        let account = store
            .upsert_microsoft_account("msa-1", "profile-1", "Mateo")
            .await
            .expect("create Microsoft account");
        backend.fail_next();
        store
            .remove_microsoft_login("msa-1")
            .await
            .expect_err("Microsoft removal write fails");

        assert_eq!(
            store
                .remove_microsoft_login("msa-1")
                .await
                .expect("retry Microsoft removal"),
            Some(account)
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(backend.committed_indexes().len(), 2);
        store.close().await.expect("close account store");
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload Microsoft removal");
        assert!(reloaded.list().is_empty());
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn identical_remove_all_microsoft_retry_reports_committed_success_and_reloads() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("remove-all-microsoft-retry");
        let offline = store
            .create_offline_account("LocalUser")
            .await
            .expect("create offline account");
        store
            .upsert_microsoft_account("msa-1", "profile-1", "Mateo")
            .await
            .expect("create Microsoft account");
        backend.fail_next();
        store
            .remove_all_microsoft_accounts()
            .await
            .expect_err("Microsoft cleanup write fails");

        assert!(
            store
                .remove_all_microsoft_accounts()
                .await
                .expect("retry Microsoft cleanup")
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 4);
        assert_eq!(backend.committed_indexes().len(), 3);
        store.close().await.expect("close account store");
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload Microsoft cleanup");
        assert_eq!(reloaded.list(), vec![offline.clone()]);
        assert_eq!(reloaded.active_account(), Some(offline));
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_and_close_reconcile_exact_retry_before_lifecycle_transition() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("lifecycle-retry");
        backend.fail_next();
        store
            .create_offline_account("FirstUser")
            .await
            .expect_err("first write fails");

        store.flush().await.expect("flush retries exact candidate");
        assert_eq!(store.list()[0].display_name, "FirstUser");

        backend.fail_next();
        store
            .create_offline_account("LaterUser")
            .await
            .expect_err("second write fails");
        store.close().await.expect("close retries exact candidate");
        assert_eq!(store.list().len(), 2);
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload lifecycle retries");
        assert_eq!(reloaded.list().len(), 2);
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn truthful_no_ops_do_not_schedule_snapshot_writes() {
        let (root, _, backend, _, store) = persistence_fixture("truthful-noops");
        assert!(
            store
                .select("missing")
                .await
                .expect("missing select")
                .is_none()
        );
        assert!(
            store
                .remove("missing")
                .await
                .expect("missing remove")
                .is_none()
        );
        assert!(
            !store
                .remove_all_microsoft_accounts()
                .await
                .expect("empty Microsoft clear")
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);

        let account = store
            .create_offline_account("LocalUser")
            .await
            .expect("create account");
        let attempts = backend.attempts.load(Ordering::SeqCst);
        assert_eq!(
            store
                .select(&account.account_id)
                .await
                .expect("same select"),
            Some(account.clone())
        );
        assert_eq!(
            store
                .rename_offline_account(&account.account_id, &account.display_name)
                .await
                .expect("same rename"),
            Some(account)
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), attempts);
        store.close().await.expect("close account store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn selection_order_and_microsoft_removal_behavior_are_preserved() {
        let (root, _, _, _, store) = persistence_fixture("selection-removal");
        let first = store
            .upsert_microsoft_account("msa-1", "profile-1", "First")
            .await
            .expect("upsert first Microsoft account");
        let second = store
            .upsert_microsoft_account("msa-2", "profile-2", "Second")
            .await
            .expect("upsert second Microsoft account");
        let offline = store
            .create_offline_account("LocalUser")
            .await
            .expect("create offline account");
        let before = store
            .list()
            .into_iter()
            .map(|account| account.account_id)
            .collect::<Vec<_>>();

        store.select(&first.account_id).await.expect("select first");
        store
            .select(&offline.account_id)
            .await
            .expect("select offline");
        store
            .select(&second.account_id)
            .await
            .expect("select second");
        let after = store
            .list()
            .into_iter()
            .map(|account| account.account_id)
            .collect::<Vec<_>>();
        assert_eq!(before, after);

        assert!(
            store
                .remove_all_microsoft_accounts()
                .await
                .expect("remove Microsoft accounts")
        );
        assert_eq!(store.list(), vec![offline.clone()]);
        assert_eq!(store.active_account(), Some(offline));
        store.close().await.expect("close account store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn duplicate_snapshot_owner_is_rejected() {
        let (root, paths, _, coordinator, store) = persistence_fixture("owner-collision");
        let duplicate = match LauncherAccountStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        ) {
            Ok(_) => panic!("duplicate owner must fail"),
            Err(error) => error,
        };
        assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);
        store.close().await.expect("close first owner");
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn invalid_existing_input_is_preserved_until_explicit_mutation() {
        let root = test_root("invalid-preserved");
        let paths = test_paths(&root);
        fs::create_dir_all(
            paths
                .accounts_file()
                .parent()
                .expect("account index has a parent"),
        )
        .expect("create app root");
        let index_path = paths.accounts_file().to_path_buf();
        let invalid = br#"{"schema":"wrong","accounts":[]}"#;
        fs::write(&index_path, invalid).expect("write invalid account index");
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store = LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
            .expect("load invalid input safely");

        assert!(store.list().is_empty());
        assert_eq!(
            fs::read(&index_path).expect("read untouched input"),
            invalid
        );
        store
            .create_offline_account("LocalUser")
            .await
            .expect("explicit mutation replaces invalid input");
        assert_ne!(
            fs::read(&index_path).expect("read committed input"),
            invalid
        );
        store.close().await.expect("close account store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn close_rejects_mutation_and_allows_clean_restart() {
        let (root, paths, _, coordinator, store) = persistence_fixture("close-restart");
        let account = store
            .create_offline_account("LocalUser")
            .await
            .expect("create account");
        store.close().await.expect("close account store");
        let error = store
            .create_offline_account("LaterUser")
            .await
            .expect_err("closed store rejects mutation");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(store.active_account(), Some(account.clone()));
        drop(store);

        let reloaded =
            LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("restart account store");
        assert_eq!(reloaded.active_account(), Some(account));
        reloaded.close().await.expect("close reloaded store");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[should_panic(expected = "launcher account store lock poisoned")]
    fn poisoned_lock_uses_the_single_invariant_policy() {
        let store = LauncherAccountStore::default();
        let state = store.state.clone();
        let _ = std::thread::spawn(move || {
            let _guard = state.lock().expect("lock account state");
            panic!("poison account state");
        })
        .join();
        let _ = store.list();
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-account-store-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
