use croopor_config::{AppPaths, validate_username};
use croopor_minecraft::offline_uuid;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

const ACCOUNT_STORE_SCHEMA: &str = "croopor.accounts";
const ACCOUNT_STORE_SCHEMA_VERSION: u32 = 1;

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

pub struct LauncherAccountStore {
    index_path: PathBuf,
    lock: Mutex<AccountIndex>,
}

impl LauncherAccountStore {
    pub fn load_from_paths(paths: &AppPaths) -> Self {
        let index_path = paths.config_dir.join("accounts.json");
        let index = match load_index(&index_path) {
            Ok(index) => index,
            Err(error) => {
                tracing::warn!("account store could not be loaded; starting empty: {error}");
                empty_index()
            }
        };

        Self {
            index_path,
            lock: Mutex::new(index),
        }
    }

    pub fn list(&self) -> io::Result<Vec<LauncherAccountRecord>> {
        let guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut accounts = guard.accounts.clone();
        accounts.sort_by(|left, right| {
            kind_order(left.kind)
                .cmp(&kind_order(right.kind))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.display_name.cmp(&right.display_name))
                .then_with(|| left.account_id.cmp(&right.account_id))
        });
        Ok(accounts)
    }

    pub fn active_account_id(&self) -> io::Result<Option<String>> {
        let guard = self.lock.lock().map_err(|_| lock_error())?;
        Ok(guard.active_account_id.clone())
    }

    pub fn active_account(&self) -> io::Result<Option<LauncherAccountRecord>> {
        let guard = self.lock.lock().map_err(|_| lock_error())?;
        let Some(active_account_id) = guard.active_account_id.as_deref() else {
            return Ok(None);
        };
        Ok(guard
            .accounts
            .iter()
            .find(|account| account.account_id == active_account_id)
            .cloned())
    }

    pub fn select(&self, account_id: &str) -> io::Result<Option<LauncherAccountRecord>> {
        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut next = guard.clone();
        let Some(account) = next
            .accounts
            .iter()
            .find(|account| account.account_id == account_id)
            .cloned()
        else {
            return Ok(None);
        };
        next.active_account_id = Some(account.account_id.clone());
        self.persist_index(&next)?;
        *guard = next;
        Ok(Some(account))
    }

    pub fn upsert_microsoft_account(
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
    }

    pub fn sync_microsoft_account(
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
    }

    fn upsert_microsoft_account_with_selection(
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

        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut next = guard.clone();
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
        self.persist_index(&next)?;
        *guard = next;
        Ok(record)
    }

    pub fn create_offline_account(&self, username: &str) -> io::Result<LauncherAccountRecord> {
        let display_name =
            validate_username(username).map_err(|error| invalid_input(error.to_string()))?;
        let uuid = offline_uuid(&display_name);
        let account_id = offline_account_id(&uuid);
        let now = chrono::Utc::now().to_rfc3339();

        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut next = guard.clone();
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
        self.persist_index(&next)?;
        *guard = next;
        Ok(record)
    }

    pub fn rename_offline_account(
        &self,
        account_id: &str,
        username: &str,
    ) -> io::Result<Option<LauncherAccountRecord>> {
        let display_name =
            validate_username(username).map_err(|error| invalid_input(error.to_string()))?;
        let uuid = offline_uuid(&display_name);
        let next_account_id = offline_account_id(&uuid);
        let now = chrono::Utc::now().to_rfc3339();

        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut next = guard.clone();
        let Some(position) = next.accounts.iter().position(|account| {
            account.account_id == account_id && account.kind == LauncherAccountKind::Offline
        }) else {
            return Ok(None);
        };
        if next
            .accounts
            .iter()
            .enumerate()
            .any(|(candidate_position, account)| {
                candidate_position != position
                    && account.kind == LauncherAccountKind::Offline
                    && account.offline_uuid.as_deref() == Some(uuid.as_str())
            })
        {
            return Err(invalid_input("offline account already exists"));
        }

        let created_at = next.accounts[position].created_at.clone();
        let record = LauncherAccountRecord {
            account_id: next_account_id,
            kind: LauncherAccountKind::Offline,
            display_name,
            login_id: None,
            minecraft_profile_id: None,
            offline_uuid: Some(uuid),
            created_at,
            updated_at: now,
        };
        let was_active = next.active_account_id.as_deref() == Some(account_id);
        next.accounts[position] = record.clone();
        if was_active {
            next.active_account_id = Some(record.account_id.clone());
        }
        self.persist_index(&next)?;
        *guard = next;
        Ok(Some(record))
    }

    pub fn remove(&self, account_id: &str) -> io::Result<Option<LauncherAccountRecord>> {
        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut next = guard.clone();
        let Some(position) = next
            .accounts
            .iter()
            .position(|account| account.account_id == account_id)
        else {
            return Ok(None);
        };
        let removed = next.accounts.remove(position);
        if next.active_account_id.as_deref() == Some(removed.account_id.as_str()) {
            next.active_account_id = next
                .accounts
                .iter()
                .find(|account| account.kind == LauncherAccountKind::Offline)
                .map(|account| account.account_id.clone());
        }
        self.persist_index(&next)?;
        *guard = next;
        Ok(Some(removed))
    }

    pub fn remove_microsoft_login(
        &self,
        login_id: &str,
    ) -> io::Result<Option<LauncherAccountRecord>> {
        let account_id = {
            let guard = self.lock.lock().map_err(|_| lock_error())?;
            guard
                .accounts
                .iter()
                .find(|account| {
                    account.kind == LauncherAccountKind::Microsoft
                        && account.login_id.as_deref() == Some(login_id)
                })
                .map(|account| account.account_id.clone())
        };
        match account_id {
            Some(account_id) => self.remove(&account_id),
            None => Ok(None),
        }
    }

    pub fn remove_all_microsoft_accounts(&self) -> io::Result<bool> {
        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let mut next = guard.clone();
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
        if !removed_any && !active_missing {
            return Ok(false);
        }
        self.persist_index(&next)?;
        *guard = next;
        Ok(removed_any)
    }

    pub fn clear_all(&self) -> io::Result<bool> {
        let mut guard = self.lock.lock().map_err(|_| lock_error())?;
        let had_accounts = !guard.accounts.is_empty() || guard.active_account_id.is_some();
        let next = empty_index();
        self.persist_index(&next)?;
        *guard = next;
        Ok(had_accounts)
    }

    fn persist_index(&self, index: &AccountIndex) -> io::Result<()> {
        if let Some(parent) = self.index_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(index)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temp_path = self.index_path.with_extension("json.tmp");
        fs::write(&temp_path, data)?;
        replace_file(&temp_path, &self.index_path)
    }
}

impl Default for LauncherAccountStore {
    fn default() -> Self {
        Self {
            index_path: PathBuf::new(),
            lock: Mutex::new(empty_index()),
        }
    }
}

pub fn microsoft_account_id(login_id: &str) -> String {
    format!("microsoft-{}", account_id_component(login_id))
}

pub fn offline_account_id(uuid: &str) -> String {
    format!("offline-{}", account_id_component(uuid))
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

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    let first_error = match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match fs::symlink_metadata(source) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(first_error),
        Err(error) => return Err(error),
    }

    if destination.exists() && !destination.is_dir() {
        let _ = fs::remove_file(destination);
    }

    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(source);
            Err(error)
        }
    }
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

fn lock_error() -> io::Error {
    io::Error::other("account store lock poisoned")
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
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn account_store_does_not_create_offline_account_from_config_username() {
        let root = test_root("empty");
        let paths = test_paths(&root);
        let store = LauncherAccountStore::load_from_paths(&paths);

        assert_eq!(store.list().expect("list accounts"), Vec::new());
        assert_eq!(store.active_account().expect("active account"), None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_store_keeps_same_name_online_and_offline_distinct() {
        let root = test_root("same-name");
        let paths = test_paths(&root);
        let store = LauncherAccountStore::load_from_paths(&paths);

        let microsoft = store
            .upsert_microsoft_account("msa-1", "profile-1", "Mateo")
            .expect("upsert microsoft account");
        let offline = store
            .create_offline_account("Mateo")
            .expect("create offline account");

        assert_ne!(microsoft.account_id, offline.account_id);
        assert_eq!(microsoft.kind, LauncherAccountKind::Microsoft);
        assert_eq!(offline.kind, LauncherAccountKind::Offline);
        assert_eq!(
            store.active_account().expect("active account").as_ref(),
            Some(&offline)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_store_persists_active_selection() {
        let root = test_root("persist-active");
        let paths = test_paths(&root);
        let store = LauncherAccountStore::load_from_paths(&paths);
        let microsoft = store
            .upsert_microsoft_account("msa-1", "profile-1", "Mateo")
            .expect("upsert microsoft account");
        let offline = store
            .create_offline_account("Player")
            .expect("create offline account");

        store
            .select(&microsoft.account_id)
            .expect("select microsoft account");
        let reloaded = LauncherAccountStore::load_from_paths(&paths);

        assert_eq!(
            reloaded.active_account().expect("active account").as_ref(),
            Some(&microsoft)
        );
        assert!(
            reloaded
                .list()
                .expect("list accounts")
                .iter()
                .any(|account| account.account_id == offline.account_id)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_store_list_order_does_not_change_when_selection_changes() {
        let root = test_root("stable-order");
        let paths = test_paths(&root);
        let store = LauncherAccountStore::load_from_paths(&paths);
        let first = store
            .upsert_microsoft_account("msa-1", "profile-1", "First")
            .expect("upsert first microsoft account");
        let second = store
            .upsert_microsoft_account("msa-2", "profile-2", "Second")
            .expect("upsert second microsoft account");
        let offline = store
            .create_offline_account("LocalUser")
            .expect("create offline account");

        let before = store
            .list()
            .expect("list accounts")
            .into_iter()
            .map(|account| account.account_id)
            .collect::<Vec<_>>();

        store
            .select(&first.account_id)
            .expect("select first account");
        store
            .select(&offline.account_id)
            .expect("select offline account");
        store
            .select(&second.account_id)
            .expect("select second account");

        let after = store
            .list()
            .expect("list accounts")
            .into_iter()
            .map(|account| account.account_id)
            .collect::<Vec<_>>();

        assert_eq!(before, after);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_store_remove_all_microsoft_preserves_offline_accounts() {
        let root = test_root("remove-all-microsoft");
        let paths = test_paths(&root);
        let store = LauncherAccountStore::load_from_paths(&paths);
        let microsoft = store
            .upsert_microsoft_account("msa-1", "profile-1", "Mateo")
            .expect("upsert microsoft account");
        let offline = store
            .create_offline_account("LocalUser")
            .expect("create offline account");
        store
            .select(&microsoft.account_id)
            .expect("select microsoft account");

        assert!(
            store
                .remove_all_microsoft_accounts()
                .expect("remove microsoft accounts")
        );

        assert_eq!(store.list().expect("list accounts"), vec![offline.clone()]);
        assert_eq!(
            store.active_account().expect("active account"),
            Some(offline)
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths {
            config_file: root.join("config.json"),
            instances_file: root.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir: root.to_path_buf(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "croopor-account-store-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
