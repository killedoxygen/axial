use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLoginSession {
    pub login_id: String,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLoginMsaToken {
    pub login_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: Option<String>,
    pub authenticated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAuthLoginMsaToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAuthLoginSession {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: Option<String>,
}

pub struct AuthLoginStore {
    sessions: RwLock<HashMap<String, AuthLoginSession>>,
    active_msa_token: RwLock<Option<AuthLoginMsaToken>>,
    next_id: AtomicU64,
}

impl AuthLoginStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            active_msa_token: RwLock::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn insert(&self, new_session: NewAuthLoginSession) -> AuthLoginSession {
        let created_at = Utc::now();
        let expires_at =
            created_at + chrono::Duration::seconds(saturating_u64_to_i64(new_session.expires_in));
        let session = AuthLoginSession {
            login_id: self.next_login_id(),
            device_code: new_session.device_code,
            user_code: new_session.user_code,
            verification_uri: new_session.verification_uri,
            expires_in: new_session.expires_in,
            interval: new_session.interval,
            message: new_session.message,
            created_at,
            expires_at,
        };

        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| session.expires_at > created_at);
        sessions.insert(session.login_id.clone(), session.clone());
        session
    }

    pub async fn get(&self, login_id: &str) -> Option<AuthLoginSession> {
        let now = Utc::now();
        self.sessions
            .read()
            .await
            .get(login_id)
            .filter(|session| session.expires_at > now)
            .cloned()
    }

    pub async fn complete_with_msa_token(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
    ) -> Option<AuthLoginMsaToken> {
        let now = Utc::now();
        let session = self.sessions.write().await.remove(login_id);
        if !session.is_some_and(|session| session.expires_at > now) {
            return None;
        }

        let token = AuthLoginMsaToken {
            login_id: login_id.to_string(),
            access_token: new_token.access_token,
            refresh_token: new_token.refresh_token,
            id_token: new_token.id_token,
            token_type: new_token.token_type,
            expires_in: new_token.expires_in,
            scope: new_token.scope,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_token.expires_in)),
        };

        *self.active_msa_token.write().await = Some(token.clone());
        Some(token)
    }

    pub async fn increase_interval(&self, login_id: &str, additional_seconds: u64) -> Option<u64> {
        let now = Utc::now();
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(login_id)
            .filter(|session| session.expires_at > now)?;
        session.interval = session.interval.saturating_add(additional_seconds);
        Some(session.interval)
    }

    pub async fn remove(&self, login_id: &str) -> bool {
        self.sessions.write().await.remove(login_id).is_some()
    }

    pub async fn has_active_msa_auth(&self) -> bool {
        self.active_msa_auth_remaining_seconds().await.is_some()
    }

    pub async fn active_msa_auth_remaining_seconds(&self) -> Option<u64> {
        let mut token = self.active_msa_token.write().await;
        let (expires_at, expires_in) = {
            let active = token.as_ref()?;
            (active.expires_at, active.expires_in)
        };
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            *token = None;
            return None;
        }

        Some((((remaining as u64) + 999) / 1000).min(expires_in))
    }

    pub async fn clear_all(&self) -> (usize, bool) {
        let cleared_pending_logins = {
            let mut sessions = self.sessions.write().await;
            let len = sessions.len();
            sessions.clear();
            len
        };
        let had_msa_auth = self.active_msa_token.write().await.take().is_some();

        (cleared_pending_logins, had_msa_auth)
    }

    #[cfg(test)]
    pub async fn active_msa_token(&self) -> Option<AuthLoginMsaToken> {
        self.active_msa_token.read().await.clone()
    }

    pub async fn remove_expired(&self, login_id: &str) -> bool {
        let now = Utc::now();
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(login_id).cloned()
        };

        match session {
            Some(session) if session.expires_at > now => false,
            Some(_) => {
                self.sessions.write().await.remove(login_id);
                true
            }
            None => false,
        }
    }

    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    fn next_login_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        format!("msa-{nanos:x}-{sequence:x}")
    }
}

impl Default for AuthLoginStore {
    fn default() -> Self {
        Self::new()
    }
}

fn saturating_u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auth_login_store_keeps_raw_device_code_server_side() {
        let store = AuthLoginStore::new();

        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: Some("Use this code.".to_string()),
            })
            .await;

        assert!(session.login_id.starts_with("msa-"));
        assert_eq!(session.device_code, "raw-device-code");
        assert_eq!(session.user_code, "ABCD-EFGH");
        assert_eq!(session.expires_in, 900);
        assert_eq!(session.interval, 5);
        assert!(session.expires_at > session.created_at);
        assert_eq!(store.get(&session.login_id).await, Some(session));
    }

    #[tokio::test]
    async fn auth_login_store_prunes_expired_sessions_on_insert() {
        let store = AuthLoginStore::new();

        let expired = store
            .insert(NewAuthLoginSession {
                device_code: "expired-device-code".to_string(),
                user_code: "OLD-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 0,
                interval: 5,
                message: None,
            })
            .await;
        let active = store
            .insert(NewAuthLoginSession {
                device_code: "active-device-code".to_string(),
                user_code: "NEW-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        assert_eq!(store.get(&expired.login_id).await, None);
        assert_eq!(store.get(&active.login_id).await, Some(active));
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn auth_login_store_removes_expired_known_session() {
        let store = AuthLoginStore::new();

        let expired = store
            .insert(NewAuthLoginSession {
                device_code: "expired-device-code".to_string(),
                user_code: "OLD-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 0,
                interval: 5,
                message: None,
            })
            .await;

        assert!(store.remove_expired(&expired.login_id).await);
        assert!(!store.remove_expired(&expired.login_id).await);
        assert_eq!(store.len().await, 0);
    }

    #[tokio::test]
    async fn auth_login_store_does_not_remove_pending_session() {
        let store = AuthLoginStore::new();

        let active = store
            .insert(NewAuthLoginSession {
                device_code: "active-device-code".to_string(),
                user_code: "NEW-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        assert!(!store.remove_expired(&active.login_id).await);
        assert_eq!(store.get(&active.login_id).await, Some(active));
    }

    #[tokio::test]
    async fn auth_login_store_keeps_only_one_active_msa_token() {
        let store = AuthLoginStore::new();
        let first = store
            .insert(NewAuthLoginSession {
                device_code: "first-device-code".to_string(),
                user_code: "FIRST".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        let second = store
            .insert(NewAuthLoginSession {
                device_code: "second-device-code".to_string(),
                user_code: "SECOND".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        store
            .complete_with_msa_token(
                &first.login_id,
                NewAuthLoginMsaToken {
                    access_token: "first-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: None,
                },
            )
            .await
            .expect("first token");
        store
            .complete_with_msa_token(
                &second.login_id,
                NewAuthLoginMsaToken {
                    access_token: "second-access-token".to_string(),
                    refresh_token: Some("second-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
            )
            .await
            .expect("second token");

        let active = store.active_msa_token().await.expect("active token");
        assert_eq!(active.login_id, second.login_id);
        assert_eq!(active.access_token, "second-access-token");
        assert_eq!(
            active.refresh_token,
            Some("second-refresh-token".to_string())
        );
        assert_eq!(store.get(&first.login_id).await, None);
        assert_eq!(store.get(&second.login_id).await, None);
    }

    #[tokio::test]
    async fn auth_login_store_clear_all_removes_pending_and_active_msa_auth() {
        let store = AuthLoginStore::new();
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: Some("msa-refresh-token".to_string()),
                    id_token: Some("msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: None,
                },
            )
            .await
            .expect("complete login");
        let pending = store
            .insert(NewAuthLoginSession {
                device_code: "pending-device-code".to_string(),
                user_code: "PENDING".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        let summary = store.clear_all().await;

        assert_eq!(summary, (1, true));
        assert_eq!(store.get(&pending.login_id).await, None);
        assert_eq!(store.active_msa_token().await, None);

        assert_eq!(store.clear_all().await, (0, false));
    }

    #[tokio::test]
    async fn auth_login_store_drops_expired_active_msa_auth() {
        let store = AuthLoginStore::new();
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 0,
                    scope: None,
                },
            )
            .await
            .expect("complete login");

        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert_eq!(store.active_msa_token().await, None);
        assert!(!store.has_active_msa_auth().await);
    }
}
