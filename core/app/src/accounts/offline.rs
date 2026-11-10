//! Offline identity commands, committed by the directory under the auth task owner.

use super::{
    directory::AccountDirectory,
    model::{AccountError, AccountPreconditions, AccountSnapshot, offline_uuid, validate_username},
    session::{AuthError, AuthService},
    view::AccountCommandResponse,
};

impl AuthService {
    pub async fn create_offline(
        &self,
        username: &str,
        expected: AccountPreconditions,
    ) -> Result<AccountCommandResponse, AuthError> {
        let name = validate_username(username)?;
        let id = offline_id(&name);
        let snapshot = self
            .commit_offline(move |directory| {
                directory.create_offline_with_preconditions(&name, expected)
            })
            .await?;
        Ok(AccountCommandResponse::new(
            snapshot,
            &id,
            "account_created",
            "Offline identity created.",
        )?)
    }

    pub async fn rename_offline(
        &self,
        account_id: String,
        username: &str,
        expected: AccountPreconditions,
    ) -> Result<AccountCommandResponse, AuthError> {
        let name = validate_username(username)?;
        let next_id = offline_id(&name);
        let snapshot = self
            .commit_offline(move |directory| {
                directory.rename_offline_with_preconditions(&account_id, &name, expected)
            })
            .await?;
        Ok(AccountCommandResponse::new(
            snapshot,
            &next_id,
            "account_updated",
            "Offline identity updated.",
        )?)
    }

    async fn commit_offline(
        &self,
        apply: impl FnOnce(&AccountDirectory) -> Result<AccountSnapshot, AccountError> + Send + 'static,
    ) -> Result<AccountSnapshot, AuthError> {
        let directory = self.directory().clone();
        self.retain(async move {
            tokio::task::spawn_blocking(move || apply(&directory))
                .await
                .map_err(|_| AuthError::Unavailable)?
                .map_err(AuthError::from)
        })
        .await
    }
}

fn offline_id(name: &str) -> String {
    format!("offline-{}", offline_uuid(name))
}
