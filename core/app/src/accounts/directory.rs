//! Persisted account commands. One SQLite transaction owns identity and selection.

use std::sync::Arc;

use crate::storage::{
    MetadataStore, Migration,
    rusqlite::{Connection, Transaction, params},
};

use super::{
    model::{
        AccountError, AccountId, AccountKind, AccountPreconditions, AccountRecord, AccountSnapshot,
        LaunchAuthMode, MicrosoftIdentity, MicrosoftIdentityImport, OfflineIdentityImport,
        microsoft_account_id, offline_uuid, validate_username,
    },
    selection::CapturedAccount,
};

pub const MIGRATION: Migration = Migration {
    id: "accounts-directory-v1",
    sql: "CREATE TABLE account_directory (
        account_id TEXT PRIMARY KEY NOT NULL,
        record_json TEXT NOT NULL CHECK(length(record_json) <= 262144)
    );
    CREATE TABLE account_selection (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        revision INTEGER NOT NULL CHECK(revision >= 0),
        active_account_id TEXT REFERENCES account_directory(account_id) DEFERRABLE INITIALLY DEFERRED,
        launch_auth_mode TEXT NOT NULL CHECK(launch_auth_mode IN ('offline', 'online'))
    );
    INSERT INTO account_selection VALUES(1, 0, NULL, 'offline');",
};

const MAX_ACCOUNTS: usize = 256;
const MAX_SAFE_REVISION: u64 = 9_007_199_254_740_991;

#[derive(Clone)]
pub struct AccountDirectory {
    store: Arc<MetadataStore>,
}

impl AccountDirectory {
    pub fn uses_metadata(&self, store: &Arc<MetadataStore>) -> bool {
        Arc::ptr_eq(&self.store, store)
    }

    pub fn new(store: Arc<MetadataStore>) -> Result<Self, AccountError> {
        store.migrate(&[MIGRATION])?;
        let directory = Self { store };
        directory.snapshot()?;
        Ok(directory)
    }

    pub fn snapshot(&self) -> Result<AccountSnapshot, AccountError> {
        self.store.read(|connection| {
            let transaction = connection.unchecked_transaction()?;
            let snapshot = read_snapshot(&transaction)?;
            transaction.commit()?;
            Ok(snapshot)
        })
    }

    pub fn selection_revision(&self) -> Result<u64, AccountError> {
        Ok(self.snapshot()?.selection_revision)
    }

    pub fn capture_selected(&self) -> Result<CapturedAccount, AccountError> {
        let snapshot = self.snapshot()?;
        let id = snapshot
            .active_account_id
            .as_ref()
            .ok_or(AccountError::NoSelection)?;
        CapturedAccount::new(&snapshot, id.as_str())
    }

    /// Admit short in-memory work against the exact selection observed by its
    /// caller. The metadata lock fences every account/selection writer, including
    /// settings transactions. The callback must not reenter metadata or do I/O.
    pub fn with_selected_account<T>(
        &self,
        expected_account_id: &str,
        expected_selection_revision: u64,
        admit: impl FnOnce(&CapturedAccount) -> T,
    ) -> Result<T, AccountError> {
        self.store.read(|connection| {
            let transaction = connection.unchecked_transaction()?;
            let snapshot = read_snapshot(&transaction)?;
            transaction.commit()?;
            if snapshot.selection_revision != expected_selection_revision
                || snapshot.active_account_id.as_ref().map(AccountId::as_str)
                    != Some(expected_account_id)
            {
                return Err(AccountError::StaleCapture);
            }
            let capture = CapturedAccount::new(&snapshot, expected_account_id)?;
            Ok(admit(&capture))
        })
    }

    pub fn capture(&self, account_id: &str) -> Result<CapturedAccount, AccountError> {
        CapturedAccount::new(&self.snapshot()?, account_id)
    }

    pub fn capture_account(&self, account_id: &str) -> Result<CapturedAccount, AccountError> {
        self.capture(account_id)
    }

    pub fn validate_capture(&self, capture: &CapturedAccount) -> Result<(), AccountError> {
        capture.validate(&self.snapshot()?)
    }

    pub fn validate_account_capture(&self, capture: &CapturedAccount) -> Result<(), AccountError> {
        capture.validate_account(&self.snapshot()?)
    }

    pub fn create_offline_account(&self, username: &str) -> Result<AccountSnapshot, AccountError> {
        self.create_offline_with_preconditions(username, AccountPreconditions::default())
    }

    pub fn create_offline_with_preconditions(
        &self,
        username: &str,
        expected: AccountPreconditions,
    ) -> Result<AccountSnapshot, AccountError> {
        let name = validate_username(username)?;
        self.mutate(move |snapshot, revision| {
            check_preconditions(snapshot, None, expected)?;
            let uuid = offline_uuid(&name);
            let id = AccountId::parse(&format!("offline-{uuid}"))?;
            if !snapshot.accounts.iter().any(|a| a.account_id == id) {
                check_capacity(snapshot)?;
                let now = chrono::Utc::now().to_rfc3339();
                snapshot.accounts.push(AccountRecord {
                    account_id: id.clone(),
                    kind: AccountKind::Offline,
                    display_name: name,
                    login_id: None,
                    minecraft_profile_id: None,
                    offline_uuid: Some(uuid),
                    minecraft_profile: None,
                    account_revision: revision,
                    profile_revision: revision,
                    credential_revision: 0,
                    created_revision: revision,
                    created_at: now.clone(),
                    updated_at: now,
                });
            }
            select_identity(snapshot, Some(id));
            Ok(())
        })
    }

    pub fn rename_offline_account(
        &self,
        account_id: &str,
        username: &str,
    ) -> Result<AccountSnapshot, AccountError> {
        self.rename_offline_with_preconditions(
            account_id,
            username,
            AccountPreconditions::default(),
        )
    }

    pub fn rename_offline_with_preconditions(
        &self,
        account_id: &str,
        username: &str,
        expected: AccountPreconditions,
    ) -> Result<AccountSnapshot, AccountError> {
        let name = validate_username(username)?;
        let id = AccountId::parse(account_id)?;
        self.mutate(move |snapshot, revision| {
            check_preconditions(snapshot, Some(&id), expected)?;
            rename_offline(snapshot, revision, &id, name)
        })
    }

    pub fn select(&self, account_id: &str) -> Result<AccountSnapshot, AccountError> {
        self.select_with_preconditions(account_id, AccountPreconditions::default())
    }

    pub fn select_with_preconditions(
        &self,
        account_id: &str,
        expected: AccountPreconditions,
    ) -> Result<AccountSnapshot, AccountError> {
        let id = AccountId::parse(account_id)?;
        self.mutate(move |snapshot, _| {
            check_preconditions(snapshot, Some(&id), expected)?;
            position(snapshot, &id)?;
            select_identity(snapshot, Some(id));
            Ok(())
        })
    }

    /// HTTP removal uses this for offline identities; Microsoft removal goes
    /// through the authentication owner so its keyring obligations are retained.
    pub fn remove(&self, account_id: &str) -> Result<AccountSnapshot, AccountError> {
        self.remove_offline_with_preconditions(account_id, AccountPreconditions::default())
    }

    pub fn remove_offline_with_preconditions(
        &self,
        account_id: &str,
        expected: AccountPreconditions,
    ) -> Result<AccountSnapshot, AccountError> {
        let id = AccountId::parse(account_id)?;
        self.mutate(move |snapshot, _| {
            check_preconditions(snapshot, Some(&id), expected)?;
            let index = position(snapshot, &id)?;
            if snapshot.accounts[index].kind != AccountKind::Offline {
                return Err(AccountError::NotOffline);
            }
            remove_identity(snapshot, index);
            Ok(())
        })
    }

    pub fn commit_microsoft(
        &self,
        expected_selection_revision: u64,
        identity: MicrosoftIdentity,
    ) -> Result<CapturedAccount, AccountError> {
        let id = validate_microsoft(&identity)?;
        let snapshot = self.mutate(|snapshot, revision| {
            check_preconditions(
                snapshot,
                None,
                AccountPreconditions {
                    expected_selection_revision: Some(expected_selection_revision),
                    expected_account_revision: None,
                },
            )?;
            upsert_microsoft(snapshot, revision, id.clone(), identity)?;
            select_identity(snapshot, Some(id.clone()));
            Ok(())
        })?;
        CapturedAccount::new(&snapshot, id.as_str())
    }

    pub fn refresh_microsoft(
        &self,
        capture: &CapturedAccount,
        identity: MicrosoftIdentity,
    ) -> Result<CapturedAccount, AccountError> {
        self.refresh_microsoft_inner(capture, identity, true)
    }

    pub fn refresh_account_microsoft(
        &self,
        capture: &CapturedAccount,
        identity: MicrosoftIdentity,
    ) -> Result<CapturedAccount, AccountError> {
        self.refresh_microsoft_inner(capture, identity, false)
    }

    fn refresh_microsoft_inner(
        &self,
        capture: &CapturedAccount,
        identity: MicrosoftIdentity,
        selection_bound: bool,
    ) -> Result<CapturedAccount, AccountError> {
        let id = validate_microsoft(&identity)?;
        if capture.kind() != AccountKind::Microsoft {
            return Err(AccountError::NotMicrosoft);
        }
        if capture.identity() != &id || capture.login_id() != Some(identity.login_id.as_str()) {
            return Err(AccountError::StaleCapture);
        }
        let snapshot = self.mutate(|snapshot, revision| {
            if selection_bound {
                capture.validate(snapshot)?;
            } else {
                capture.validate_account(snapshot)?;
            }
            if identity.credential_revision < capture.credential_revision() {
                return Err(AccountError::StaleCapture);
            }
            upsert_microsoft(snapshot, revision, id.clone(), identity)
        })?;
        CapturedAccount::new(&snapshot, id.as_str())
    }

    pub fn invalidate_microsoft(
        &self,
        capture: &CapturedAccount,
    ) -> Result<AccountSnapshot, AccountError> {
        if capture.kind() != AccountKind::Microsoft {
            return Err(AccountError::NotMicrosoft);
        }
        self.mutate(|snapshot, _| {
            capture.validate(snapshot)?;
            let index = position(snapshot, capture.identity())?;
            remove_identity(snapshot, index);
            Ok(())
        })
    }

    /// Logout/removal targets the captured account even if another identity has
    /// been selected while the secure store acknowledges deletion.
    pub fn remove_microsoft(
        &self,
        capture: &CapturedAccount,
    ) -> Result<AccountSnapshot, AccountError> {
        if capture.kind() != AccountKind::Microsoft {
            return Err(AccountError::NotMicrosoft);
        }
        self.mutate(|snapshot, _| {
            capture.validate_account(snapshot)?;
            let index = position(snapshot, capture.identity())?;
            remove_identity(snapshot, index);
            Ok(())
        })
    }

    pub fn import_offline_identity(
        &self,
        input: OfflineIdentityImport,
        select: bool,
    ) -> Result<AccountSnapshot, AccountError> {
        let id = AccountId::parse(&input.account_id)?;
        self.mutate(move |snapshot, revision| {
            import_offline(snapshot, revision, &input)?;
            if select {
                select_identity(snapshot, Some(id));
            }
            Ok(())
        })
    }

    /// Microsoft rows are identities awaiting reauthentication, never secure
    /// sessions. All rows and the selected mode share the caller's transaction.
    /// No source selection explicitly clears the destination selection.
    pub(crate) fn import_identities_in_transaction(
        transaction: &Transaction<'_>,
        offline: &[OfflineIdentityImport],
        microsoft: &[MicrosoftIdentityImport],
        active_id: Option<&str>,
        expected_selection_revision: u64,
    ) -> Result<AccountSnapshot, AccountError> {
        let active_id = active_id.map(AccountId::parse).transpose()?;
        mutate_transaction(transaction, |snapshot, revision| {
            if snapshot.selection_revision != expected_selection_revision {
                return Err(AccountError::StaleCapture);
            }
            let mut seen = std::collections::HashSet::new();
            for input in offline {
                if !seen.insert(input.account_id.clone()) {
                    return Err(AccountError::InvalidInput(
                        "Duplicate imported account identity.",
                    ));
                }
                import_offline(snapshot, revision, input)?;
            }
            for input in microsoft {
                if !seen.insert(input.account_id()?) {
                    return Err(AccountError::InvalidInput(
                        "Duplicate imported account identity.",
                    ));
                }
                import_microsoft(snapshot, revision, input)?;
            }
            if active_id
                .as_ref()
                .is_some_and(|id| !seen.contains(id.as_str()))
            {
                return Err(AccountError::NoSelection);
            }
            select_identity(snapshot, active_id);
            Ok(())
        })
    }

    /// The settings owner calls this inside its settings write transaction.
    /// A duplicate username selects the existing identity, as the legacy
    /// username setting did. Microsoft names are provider-owned.
    pub fn sync_active_offline_username_in_transaction(
        transaction: &Transaction<'_>,
        username: &str,
    ) -> Result<(), AccountError> {
        let name = validate_username(username)?;
        mutate_transaction(transaction, |snapshot, revision| {
            let Some(active) = snapshot.active_account() else {
                return Ok(());
            };
            if active.kind != AccountKind::Offline {
                return Ok(());
            }
            let active_id = active.account_id.clone();
            let next_id = AccountId::parse(&format!("offline-{}", offline_uuid(&name)))?;
            if snapshot.accounts.iter().any(|a| a.account_id == next_id) {
                select_identity(snapshot, Some(next_id));
                return Ok(());
            }
            rename_offline(snapshot, revision, &active_id, name)
        })?;
        Ok(())
    }

    /// Call within the caller's read transaction to project the authoritative
    /// account username/mode into retained settings without duplicating state.
    pub fn selection_in_transaction(
        connection: &Connection,
    ) -> Result<Option<(String, LaunchAuthMode)>, AccountError> {
        Ok(read_snapshot(connection)?
            .active_account()
            .map(|a| (a.display_name.clone(), a.kind.launch_mode())))
    }

    pub fn selection_revision_in_transaction(connection: &Connection) -> Result<u64, AccountError> {
        Ok(read_snapshot(connection)?.selection_revision)
    }

    pub fn select_launch_mode_in_transaction(
        transaction: &Transaction<'_>,
        mode: LaunchAuthMode,
    ) -> Result<(), AccountError> {
        mutate_transaction(transaction, |snapshot, _| {
            if mode == LaunchAuthMode::Offline && snapshot.active_account_id.is_none() {
                return Ok(());
            }
            if snapshot
                .active_account()
                .is_some_and(|a| a.kind.launch_mode() == mode)
            {
                return Ok(());
            }
            let id = snapshot
                .accounts
                .iter()
                .find(|a| a.kind.launch_mode() == mode)
                .map(|a| a.account_id.clone())
                .ok_or(AccountError::NoSelection)?;
            select_identity(snapshot, Some(id));
            Ok(())
        })?;
        Ok(())
    }

    fn mutate(
        &self,
        mutation: impl FnOnce(&mut AccountSnapshot, u64) -> Result<(), AccountError>,
    ) -> Result<AccountSnapshot, AccountError> {
        self.store
            .transaction(|transaction| mutate_transaction(transaction, mutation))
    }
}

fn import_offline(
    snapshot: &mut AccountSnapshot,
    revision: u64,
    input: &OfflineIdentityImport,
) -> Result<(), AccountError> {
    input.validate()?;
    let id = AccountId::parse(&input.account_id)?;
    if let Some(existing) = snapshot
        .accounts
        .iter()
        .find(|account| account.account_id == id)
    {
        if existing.kind != AccountKind::Offline
            || existing.display_name != input.display_name
            || existing.offline_uuid.as_deref() != Some(input.offline_uuid.as_str())
            || existing.created_at != input.created_at
            || existing.updated_at != input.updated_at
        {
            return Err(AccountError::AlreadyExists);
        }
        return Ok(());
    }
    check_capacity(snapshot)?;
    snapshot.accounts.push(AccountRecord {
        account_id: id,
        kind: AccountKind::Offline,
        display_name: input.display_name.clone(),
        login_id: None,
        minecraft_profile_id: None,
        offline_uuid: Some(input.offline_uuid.clone()),
        minecraft_profile: None,
        account_revision: revision,
        profile_revision: revision,
        credential_revision: 0,
        created_revision: revision,
        created_at: input.created_at.clone(),
        updated_at: input.updated_at.clone(),
    });
    Ok(())
}

fn import_microsoft(
    snapshot: &mut AccountSnapshot,
    revision: u64,
    input: &MicrosoftIdentityImport,
) -> Result<(), AccountError> {
    input.validate()?;
    let id = AccountId::parse(&input.account_id()?)?;
    if let Some(existing) = snapshot
        .accounts
        .iter()
        .find(|account| account.account_id == id)
    {
        if existing.kind != AccountKind::Microsoft
            || existing.credential_revision != 0
            || existing.display_name != input.display_name
            || existing.created_at != input.created_at
            || existing.updated_at != input.updated_at
        {
            return Err(AccountError::AlreadyExists);
        }
        return Ok(());
    }
    check_capacity(snapshot)?;
    snapshot.accounts.push(AccountRecord {
        account_id: id,
        kind: AccountKind::Microsoft,
        display_name: input.display_name.clone(),
        login_id: None,
        minecraft_profile_id: Some(
            uuid::Uuid::parse_str(&input.profile_id)
                .map_err(|_| AccountError::InvalidStoredData)?
                .simple()
                .to_string(),
        ),
        offline_uuid: None,
        minecraft_profile: None,
        account_revision: revision,
        profile_revision: revision,
        credential_revision: 0,
        created_revision: revision,
        created_at: input.created_at.clone(),
        updated_at: input.updated_at.clone(),
    });
    Ok(())
}

fn mutate_transaction(
    transaction: &Transaction<'_>,
    mutation: impl FnOnce(&mut AccountSnapshot, u64) -> Result<(), AccountError>,
) -> Result<AccountSnapshot, AccountError> {
    let before = read_snapshot(transaction)?;
    let mut next = before.clone();
    let revision = before
        .revision
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_REVISION)
        .ok_or(AccountError::InvalidStoredData)?;
    mutation(&mut next, revision)?;
    if next == before {
        return Ok(before);
    }
    next.revision = revision;
    next.selection_revision = revision;
    sort_accounts(&mut next.accounts);
    validate_snapshot(&next)?;
    write_snapshot(transaction, &before, &next)?;
    Ok(next)
}

fn rename_offline(
    snapshot: &mut AccountSnapshot,
    revision: u64,
    id: &AccountId,
    name: String,
) -> Result<(), AccountError> {
    let index = position(snapshot, id)?;
    if snapshot.accounts[index].kind != AccountKind::Offline {
        return Err(AccountError::NotOffline);
    }
    if snapshot.accounts[index].display_name == name {
        return Ok(());
    }
    let uuid = offline_uuid(&name);
    let next_id = AccountId::parse(&format!("offline-{uuid}"))?;
    if snapshot.accounts.iter().any(|a| a.account_id == next_id) {
        return Err(AccountError::AlreadyExists);
    }
    let account = &mut snapshot.accounts[index];
    account.account_id = next_id.clone();
    account.display_name = name;
    account.offline_uuid = Some(uuid);
    account.account_revision = revision;
    account.profile_revision = revision;
    account.updated_at = chrono::Utc::now().to_rfc3339();
    if snapshot.active_account_id.as_ref() == Some(id) {
        select_identity(snapshot, Some(next_id));
    }
    Ok(())
}

fn read_snapshot(connection: &Connection) -> Result<AccountSnapshot, AccountError> {
    let (revision, active, mode): (u64, Option<String>, String) = connection.query_row(
        "SELECT revision, active_account_id, launch_auth_mode FROM account_selection WHERE singleton = 1", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    let mut statement =
        connection.prepare("SELECT account_id, record_json FROM account_directory LIMIT 257")?;
    let records = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut accounts = Vec::new();
    for record in records {
        let (id, json) = record?;
        if json.len() > 262_144 {
            return Err(AccountError::InvalidStoredData);
        }
        let account: AccountRecord =
            serde_json::from_str(&json).map_err(|_| AccountError::InvalidStoredData)?;
        if account.account_id.as_str() != id {
            return Err(AccountError::InvalidStoredData);
        }
        accounts.push(account);
    }
    sort_accounts(&mut accounts);
    let snapshot = AccountSnapshot {
        revision,
        selection_revision: revision,
        active_account_id: active
            .as_deref()
            .map(AccountId::parse)
            .transpose()
            .map_err(|_| AccountError::InvalidStoredData)?,
        launch_auth_mode: match mode.as_str() {
            "offline" => LaunchAuthMode::Offline,
            "online" => LaunchAuthMode::Online,
            _ => return Err(AccountError::InvalidStoredData),
        },
        accounts,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn write_snapshot(
    transaction: &Transaction<'_>,
    before: &AccountSnapshot,
    next: &AccountSnapshot,
) -> Result<(), AccountError> {
    for account in &before.accounts {
        if !next
            .accounts
            .iter()
            .any(|a| a.account_id == account.account_id)
        {
            transaction.execute(
                "DELETE FROM account_directory WHERE account_id = ?1",
                [account.account_id.as_str()],
            )?;
        }
    }
    for account in &next.accounts {
        if before.accounts.iter().any(|a| a == account) {
            continue;
        }
        let json = serde_json::to_string(account).map_err(|_| AccountError::InvalidStoredData)?;
        transaction.execute("INSERT INTO account_directory(account_id, record_json) VALUES(?1, ?2) ON CONFLICT(account_id) DO UPDATE SET record_json = excluded.record_json", params![account.account_id.as_str(), json])?;
    }
    if transaction.execute("UPDATE account_selection SET revision = ?1, active_account_id = ?2, launch_auth_mode = ?3 WHERE singleton = 1",
        params![next.revision, next.active_account_id.as_ref().map(AccountId::as_str), match next.launch_auth_mode { LaunchAuthMode::Offline => "offline", LaunchAuthMode::Online => "online" }])? != 1
        || read_snapshot(transaction)? != *next
    {
        return Err(AccountError::InvalidStoredData);
    }
    Ok(())
}

fn check_preconditions(
    snapshot: &AccountSnapshot,
    id: Option<&AccountId>,
    expected: AccountPreconditions,
) -> Result<(), AccountError> {
    if expected
        .expected_selection_revision
        .is_some_and(|revision| revision != snapshot.selection_revision)
    {
        return Err(AccountError::StaleCapture);
    }
    if let Some(revision) = expected.expected_account_revision {
        let id = id.ok_or(AccountError::InvalidInput(
            "An account identity is required for this precondition.",
        ))?;
        if snapshot
            .accounts
            .get(position(snapshot, id)?)
            .is_none_or(|a| a.account_revision != revision)
        {
            return Err(AccountError::StaleCapture);
        }
    }
    Ok(())
}

fn position(snapshot: &AccountSnapshot, id: &AccountId) -> Result<usize, AccountError> {
    snapshot
        .accounts
        .iter()
        .position(|a| &a.account_id == id)
        .ok_or(AccountError::NotFound)
}

fn check_capacity(snapshot: &AccountSnapshot) -> Result<(), AccountError> {
    if snapshot.accounts.len() >= MAX_ACCOUNTS {
        return Err(AccountError::InvalidInput(
            "Remove an account before adding another.",
        ));
    }
    Ok(())
}

fn select_identity(snapshot: &mut AccountSnapshot, id: Option<AccountId>) {
    snapshot.launch_auth_mode = id
        .as_ref()
        .and_then(|id| snapshot.accounts.iter().find(|a| &a.account_id == id))
        .map(|a| a.kind.launch_mode())
        .unwrap_or_default();
    snapshot.active_account_id = id;
}

fn remove_identity(snapshot: &mut AccountSnapshot, index: usize) {
    let removed = snapshot.accounts.remove(index);
    if snapshot.active_account_id.as_ref() == Some(&removed.account_id) {
        let fallback = snapshot
            .accounts
            .iter()
            .find(|a| a.kind == AccountKind::Offline)
            .map(|a| a.account_id.clone());
        select_identity(snapshot, fallback);
    }
}

fn sort_accounts(accounts: &mut [AccountRecord]) {
    accounts.sort_by(|a, b| {
        let kind_order = |kind| match kind {
            AccountKind::Microsoft => 0,
            AccountKind::Offline => 1,
        };
        kind_order(a.kind)
            .cmp(&kind_order(b.kind))
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.account_id.as_str().cmp(b.account_id.as_str()))
    });
}

fn validate_microsoft(identity: &MicrosoftIdentity) -> Result<AccountId, AccountError> {
    let id = AccountId::parse(&microsoft_account_id(&identity.profile_id)?)?;
    if uuid::Uuid::parse_str(&identity.login_id)
        .ok()
        .is_none_or(|id| id.is_nil())
        || super::microsoft::validate_profile(&identity.profile).is_err()
        || identity.profile.name != identity.display_name
        || microsoft_account_id(&identity.profile.id)? != id.as_str()
        || identity.credential_revision == 0
        || identity.credential_revision > MAX_SAFE_REVISION
    {
        return Err(AccountError::InvalidInput(
            "Microsoft account identity is invalid.",
        ));
    }
    if serde_json::to_vec(&identity.profile)
        .map_err(|_| AccountError::InvalidStoredData)?
        .len()
        > 131_072
    {
        return Err(AccountError::InvalidInput(
            "Minecraft profile is too large.",
        ));
    }
    Ok(id)
}

fn upsert_microsoft(
    snapshot: &mut AccountSnapshot,
    revision: u64,
    id: AccountId,
    identity: MicrosoftIdentity,
) -> Result<(), AccountError> {
    let existing = snapshot.accounts.iter().position(|a| a.account_id == id);
    if existing.is_none() {
        check_capacity(snapshot)?;
    }
    if snapshot
        .accounts
        .iter()
        .any(|a| a.account_id != id && a.login_id.as_deref() == Some(&identity.login_id))
    {
        return Err(AccountError::AlreadyExists);
    }
    let prior = existing.map(|index| &snapshot.accounts[index]);
    if prior.is_some_and(|a| identity.credential_revision < a.credential_revision) {
        return Err(AccountError::StaleCapture);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let profile_changed =
        prior.is_none_or(|a| a.minecraft_profile.as_ref() != Some(&identity.profile));
    let record = AccountRecord {
        account_id: id,
        kind: AccountKind::Microsoft,
        display_name: identity.display_name,
        login_id: Some(identity.login_id),
        minecraft_profile_id: Some(
            uuid::Uuid::parse_str(&identity.profile_id)
                .map_err(|_| AccountError::InvalidStoredData)?
                .simple()
                .to_string(),
        ),
        offline_uuid: None,
        minecraft_profile: Some(identity.profile),
        account_revision: revision,
        profile_revision: if profile_changed {
            revision
        } else {
            prior.expect("existing unchanged profile").profile_revision
        },
        credential_revision: identity.credential_revision,
        created_revision: prior.map(|a| a.created_revision).unwrap_or(revision),
        created_at: prior
            .map(|a| a.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
    };
    if let Some(index) = existing {
        snapshot.accounts[index] = record;
    } else {
        snapshot.accounts.push(record);
    }
    Ok(())
}

fn valid_timestamp(value: &str) -> bool {
    value.len() <= 64 && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

fn validate_snapshot(snapshot: &AccountSnapshot) -> Result<(), AccountError> {
    let invalid = || AccountError::InvalidStoredData;
    if snapshot.accounts.len() > MAX_ACCOUNTS
        || snapshot.revision > MAX_SAFE_REVISION
        || snapshot.selection_revision != snapshot.revision
    {
        return Err(invalid());
    }
    for account in &snapshot.accounts {
        if account.account_revision == 0
            || account.account_revision > snapshot.revision
            || account.profile_revision == 0
            || account.profile_revision > account.account_revision
            || account.created_revision == 0
            || account.created_revision > account.account_revision
            || !valid_timestamp(&account.created_at)
            || !valid_timestamp(&account.updated_at)
        {
            return Err(invalid());
        }
        match account.kind {
            AccountKind::Offline => {
                let uuid = offline_uuid(&account.display_name);
                if account.display_name
                    != validate_username(&account.display_name).map_err(|_| invalid())?
                    || account.account_id.as_str() != format!("offline-{uuid}")
                    || account.offline_uuid.as_deref() != Some(uuid.as_str())
                    || account.login_id.is_some()
                    || account.minecraft_profile_id.is_some()
                    || account.minecraft_profile.is_some()
                    || account.credential_revision != 0
                {
                    return Err(invalid());
                }
            }
            AccountKind::Microsoft => {
                if account.credential_revision == 0 {
                    let input = MicrosoftIdentityImport {
                        profile_id: account.minecraft_profile_id.clone().ok_or_else(invalid)?,
                        display_name: account.display_name.clone(),
                        created_at: account.created_at.clone(),
                        updated_at: account.updated_at.clone(),
                    };
                    input.validate().map_err(|_| invalid())?;
                    if input.account_id().map_err(|_| invalid())? != account.account_id.as_str()
                        || account.minecraft_profile_id.as_deref()
                            != Some(account.account_id.as_str().replace('-', "").as_str())
                        || account.login_id.is_some()
                        || account.minecraft_profile.is_some()
                        || account.offline_uuid.is_some()
                    {
                        return Err(invalid());
                    }
                    continue;
                }
                let identity = MicrosoftIdentity {
                    login_id: account.login_id.clone().ok_or_else(invalid)?,
                    profile_id: account.minecraft_profile_id.clone().ok_or_else(invalid)?,
                    display_name: account.display_name.clone(),
                    credential_revision: account.credential_revision,
                    profile: account.minecraft_profile.clone().ok_or_else(invalid)?,
                };
                if validate_microsoft(&identity).map_err(|_| invalid())? != account.account_id
                    || account.offline_uuid.is_some()
                {
                    return Err(invalid());
                }
            }
        }
    }
    match (&snapshot.active_account_id, snapshot.active_account()) {
        (Some(_), None) => Err(invalid()),
        (_, Some(account)) if snapshot.launch_auth_mode != account.kind.launch_mode() => {
            Err(invalid())
        }
        (None, _) if snapshot.launch_auth_mode != LaunchAuthMode::Offline => Err(invalid()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported_microsoft() -> MicrosoftIdentityImport {
        MicrosoftIdentityImport {
            profile_id: "12345678123442348234123456789ABC".into(),
            display_name: "A".into(),
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: "2024-01-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn imported_microsoft_identity_and_online_selection_survive_restart() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("accounts.sqlite");
        let input = imported_microsoft();
        let expected = {
            let store = Arc::new(MetadataStore::open(&path).unwrap());
            let directory = AccountDirectory::new(store.clone()).unwrap();
            let imported = store
                .transaction(|tx| {
                    AccountDirectory::import_identities_in_transaction(
                        tx,
                        &[],
                        &[input.clone()],
                        Some(&input.account_id()?),
                        0,
                    )
                })
                .unwrap();
            let active = imported.active_account().unwrap();
            assert_eq!(active.created_at, input.created_at);
            assert_eq!(active.updated_at, input.updated_at);
            assert!(active.login_id.is_none() && active.minecraft_profile.is_none());
            assert_eq!(active.credential_revision, 0);
            assert_eq!(active.minecraft_uuid(), "12345678123442348234123456789abc");
            assert_eq!(imported.launch_auth_mode, LaunchAuthMode::Online);
            let repeated = store
                .transaction(|tx| {
                    AccountDirectory::import_identities_in_transaction(
                        tx,
                        &[],
                        &[input.clone()],
                        Some(&input.account_id()?),
                        imported.selection_revision,
                    )
                })
                .unwrap();
            assert_eq!(repeated, imported);
            assert_eq!(directory.snapshot().unwrap(), imported);
            imported
        };
        let reopened =
            AccountDirectory::new(Arc::new(MetadataStore::open(&path).unwrap())).unwrap();
        assert_eq!(reopened.snapshot().unwrap(), expected);
    }

    #[test]
    fn mixed_import_conflicts_roll_back_every_row_and_never_downgrade_authentication() {
        let store = Arc::new(MetadataStore::in_memory().unwrap());
        let directory = AccountDirectory::new(store.clone()).unwrap();
        let input = imported_microsoft();
        let offline = OfflineIdentityImport {
            account_id: format!("offline-{}", offline_uuid("Steve")),
            display_name: "Steve".into(),
            offline_uuid: offline_uuid("Steve"),
            created_at: input.created_at.clone(),
            updated_at: input.updated_at.clone(),
        };
        let original = store
            .transaction(|tx| {
                AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[],
                    &[input.clone()],
                    Some(&input.account_id()?),
                    0,
                )
            })
            .unwrap();
        let changed = MicrosoftIdentityImport {
            display_name: "Different".into(),
            ..input.clone()
        };
        assert!(
            store
                .transaction(|tx| AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[offline.clone()],
                    &[changed],
                    Some(&offline.account_id),
                    original.selection_revision
                ))
                .is_err()
        );
        assert_eq!(directory.snapshot().unwrap(), original);
        assert!(
            store
                .transaction(|tx| AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[offline.clone()],
                    &[input.clone(), input.clone()],
                    Some(&offline.account_id),
                    original.selection_revision
                ))
                .is_err()
        );
        assert_eq!(directory.snapshot().unwrap(), original);
        assert!(
            store
                .transaction(|tx| AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[offline.clone()],
                    &[],
                    Some(&input.account_id()?),
                    original.selection_revision
                ))
                .is_err()
        );
        assert_eq!(directory.snapshot().unwrap(), original);
        assert!(
            store
                .transaction(|tx| AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[offline.clone()],
                    &[input.clone()],
                    Some(&offline.account_id),
                    0
                ))
                .is_err()
        );
        assert_eq!(directory.snapshot().unwrap(), original);

        let mixed = store
            .transaction(|tx| {
                AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[offline.clone()],
                    &[input.clone()],
                    Some(&offline.account_id),
                    original.selection_revision,
                )
            })
            .unwrap();
        assert_eq!(mixed.accounts.len(), 2);
        assert_eq!(mixed.launch_auth_mode, LaunchAuthMode::Offline);
        directory
            .commit_microsoft(
                mixed.selection_revision,
                MicrosoftIdentity {
                    login_id: uuid::Uuid::new_v4().to_string(),
                    profile_id: input.profile_id.clone(),
                    display_name: input.display_name.clone(),
                    credential_revision: 1,
                    profile: super::super::microsoft::MinecraftProfile {
                        id: input.profile_id.clone(),
                        name: input.display_name.clone(),
                        skins: vec![],
                        capes: vec![],
                    },
                },
            )
            .unwrap();
        let authenticated = directory.snapshot().unwrap();
        assert!(matches!(
            store.transaction(|tx| AccountDirectory::import_identities_in_transaction(
                tx,
                &[],
                &[input.clone()],
                Some(&input.account_id()?),
                authenticated.selection_revision
            )),
            Err(AccountError::AlreadyExists)
        ));
        assert_eq!(directory.snapshot().unwrap(), authenticated);
    }

    #[test]
    fn zero_credential_revision_requires_exact_unverified_record_shape() {
        let store = Arc::new(MetadataStore::in_memory().unwrap());
        AccountDirectory::new(store.clone()).unwrap();
        let input = imported_microsoft();
        let original = store
            .transaction(|tx| {
                AccountDirectory::import_identities_in_transaction(
                    tx,
                    &[],
                    &[input.clone()],
                    Some(&input.account_id()?),
                    0,
                )
            })
            .unwrap();
        for field in [
            "login",
            "profile",
            "offline_uuid",
            "noncanonical_uuid",
            "credential",
        ] {
            let mut invalid = original.clone();
            let record = &mut invalid.accounts[0];
            match field {
                "login" => record.login_id = Some(uuid::Uuid::new_v4().to_string()),
                "profile" => {
                    record.minecraft_profile = Some(super::super::microsoft::MinecraftProfile {
                        id: input.profile_id.clone(),
                        name: input.display_name.clone(),
                        skins: vec![],
                        capes: vec![],
                    })
                }
                "offline_uuid" => record.offline_uuid = Some(offline_uuid("Steve")),
                "noncanonical_uuid" => record.minecraft_profile_id = Some(input.profile_id.clone()),
                _ => record.credential_revision = 1,
            }
            assert!(validate_snapshot(&invalid).is_err(), "{field}");
        }
    }

    fn directory() -> AccountDirectory {
        AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap()
    }

    #[test]
    fn offline_identity_changes_persist_and_invalidate_old_captures() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("accounts.sqlite");
        let expected = {
            let directory =
                AccountDirectory::new(Arc::new(MetadataStore::open(&path).unwrap())).unwrap();
            let created = directory.create_offline_account("Steve").unwrap();
            let capture = directory.capture_selected().unwrap();
            assert_eq!(capture.minecraft_uuid(), "5627dd98e6be3c21b8a8e92344183641");
            assert_eq!(directory.create_offline_account("Steve").unwrap(), created);
            let renamed = directory
                .rename_offline_account(capture.account_id(), "Notch")
                .unwrap();
            assert!(matches!(
                directory.validate_capture(&capture),
                Err(AccountError::StaleCapture)
            ));
            assert_eq!(
                renamed.active_account().unwrap().minecraft_uuid(),
                "b50ad385829d3141a2167e7d7539ba7f"
            );
            assert_eq!(renamed.launch_auth_mode, LaunchAuthMode::Offline);
            renamed
        };
        let directory =
            AccountDirectory::new(Arc::new(MetadataStore::open(&path).unwrap())).unwrap();
        assert_eq!(directory.snapshot().unwrap(), expected);
        let removed = directory
            .remove(expected.active_account_id.as_ref().unwrap().as_str())
            .unwrap();
        assert!(removed.accounts.is_empty());
        assert!(removed.active_account_id.is_none());
        assert!(matches!(
            directory.capture_selected(),
            Err(AccountError::NoSelection)
        ));
    }

    #[test]
    fn stale_mutations_and_colliding_names_preserve_the_committed_snapshot() {
        let directory = directory();
        let first = directory.create_offline_account("Steve").unwrap();
        let steve = first.active_account_id.as_ref().unwrap().as_str();
        let current = directory.create_offline_account("Notch").unwrap();
        assert!(matches!(
            directory.select_with_preconditions(
                steve,
                AccountPreconditions {
                    expected_selection_revision: Some(first.selection_revision),
                    expected_account_revision: None,
                }
            ),
            Err(AccountError::StaleCapture)
        ));
        assert!(matches!(
            directory.rename_offline_account(steve, "Notch"),
            Err(AccountError::AlreadyExists)
        ));
        assert_eq!(directory.snapshot().unwrap(), current);
        let retained = directory.capture(steve).unwrap();
        directory
            .remove(current.active_account_id.as_ref().unwrap().as_str())
            .unwrap();
        assert!(directory.validate_account_capture(&retained).is_ok());
        assert!(directory.validate_capture(&retained).is_err());
        assert_eq!(directory.capture_selected().unwrap().account_id(), steve);
    }

    #[test]
    fn settings_failure_rolls_back_identity_and_selection_together() {
        let directory = directory();
        let before = directory.create_offline_account("Steve").unwrap();
        let result = directory
            .store
            .transaction(|tx| -> Result<(), AccountError> {
                AccountDirectory::sync_active_offline_username_in_transaction(tx, "Notch")?;
                Err(AccountError::InvalidInput("settings rejected"))
            });
        assert!(result.is_err());
        assert_eq!(directory.snapshot().unwrap(), before);
    }

    #[test]
    fn malformed_persisted_identity_is_rejected_without_repair() {
        let directory = directory();
        let created = directory.create_offline_account("Steve").unwrap();
        directory
            .store
            .transaction(|tx| -> Result<(), AccountError> {
                tx.execute("UPDATE account_directory SET record_json = '{}'", [])?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            directory.snapshot(),
            Err(AccountError::InvalidStoredData)
        ));
        let count: usize = directory
            .store
            .read(|connection| -> Result<_, AccountError> {
                Ok(connection.query_row(
                    "SELECT count(*) FROM account_directory WHERE record_json = '{}'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, created.accounts.len());
    }
}
