//! Immutable account captures fence work which finishes after account changes.

use super::{
    microsoft::MinecraftProfile,
    model::{AccountError, AccountId, AccountKind, AccountRecord, AccountSnapshot, LaunchAuthMode},
};

/// Constructed by the directory only, never deserialized from an API request.
#[derive(Clone, Debug)]
pub struct CapturedAccount {
    record: AccountRecord,
    selection_revision: u64,
    selected_account_id: Option<AccountId>,
    mode: LaunchAuthMode,
}

impl CapturedAccount {
    pub(super) fn new(snapshot: &AccountSnapshot, account_id: &str) -> Result<Self, AccountError> {
        let account_id = AccountId::parse(account_id)?;
        let record = snapshot
            .accounts
            .iter()
            .find(|a| a.account_id == account_id)
            .cloned()
            .ok_or(AccountError::NotFound)?;
        Ok(Self {
            record,
            selection_revision: snapshot.selection_revision,
            selected_account_id: snapshot.active_account_id.clone(),
            mode: snapshot.launch_auth_mode,
        })
    }

    pub fn account_id(&self) -> &str {
        self.record.account_id.as_str()
    }
    pub fn identity(&self) -> &AccountId {
        &self.record.account_id
    }
    pub fn kind(&self) -> AccountKind {
        self.record.kind
    }
    pub fn launch_auth_mode(&self) -> LaunchAuthMode {
        self.mode
    }
    pub fn display_name(&self) -> &str {
        &self.record.display_name
    }
    pub fn minecraft_uuid(&self) -> &str {
        self.record.minecraft_uuid()
    }
    pub fn login_id(&self) -> Option<&str> {
        self.record.login_id.as_deref()
    }
    pub fn profile(&self) -> Option<&MinecraftProfile> {
        self.record.minecraft_profile.as_ref()
    }
    pub fn selection_revision(&self) -> u64 {
        self.selection_revision
    }
    pub fn account_revision(&self) -> u64 {
        self.record.account_revision
    }
    pub fn profile_revision(&self) -> u64 {
        self.record.profile_revision
    }
    pub fn credential_revision(&self) -> u64 {
        self.record.credential_revision
    }
    pub fn is_selected(&self) -> bool {
        self.selected_account_id.as_ref() == Some(&self.record.account_id)
    }

    pub(super) fn validate(&self, snapshot: &AccountSnapshot) -> Result<(), AccountError> {
        if self.selection_revision != snapshot.selection_revision
            || self.selected_account_id != snapshot.active_account_id
            || self.mode != snapshot.launch_auth_mode
            || !snapshot
                .accounts
                .iter()
                .any(|record| record == &self.record)
        {
            return Err(AccountError::StaleCapture);
        }
        Ok(())
    }

    pub(super) fn validate_account(&self, snapshot: &AccountSnapshot) -> Result<(), AccountError> {
        if snapshot
            .accounts
            .iter()
            .any(|record| record == &self.record)
        {
            Ok(())
        } else {
            Err(AccountError::StaleCapture)
        }
    }
}
