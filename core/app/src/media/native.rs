use super::{MediaError, SKIN_PNG_MAX_BYTES, validate_skin_png};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use uuid::Uuid;

const HANDLE_TTL: Duration = Duration::from_secs(30);

/// Created by the native window owner. It is never deserialized from IPC.
#[derive(Clone, PartialEq, Eq)]
pub struct NativeSkinScope {
    window: String,
    incarnation: Uuid,
}

impl NativeSkinScope {
    pub fn for_window(window: &str) -> Result<Self, MediaError> {
        if window.is_empty() || window.len() > 128 || window.chars().any(char::is_control) {
            return Err(MediaError::InvalidSelection);
        }
        Ok(Self {
            window: window.into(),
            incarnation: Uuid::new_v4(),
        })
    }
}

#[derive(Serialize)]
pub struct NativeSkinHandle {
    pub token: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct AdmittedSkinFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Retains only validated bytes. A handle cannot reopen a path or pin a library root.
#[derive(Clone)]
pub struct NativeSkinAdmission {
    shared: Arc<Mutex<State>>,
    drained: Arc<Notify>,
}

struct State {
    open: bool,
    generation: u64,
    in_flight: bool,
    pending: Option<Pending>,
}

struct Pending {
    scope: NativeSkinScope,
    token: String,
    expires_at: Instant,
    file: AdmittedSkinFile,
}

/// Retain this permit across the native read. A stale or cancelled read cannot publish.
pub struct NativeSelection {
    owner: NativeSkinAdmission,
    scope: NativeSkinScope,
    generation: u64,
}

impl Default for NativeSkinAdmission {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeSkinAdmission {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(State {
                open: true,
                generation: 0,
                in_flight: false,
                pending: None,
            })),
            drained: Arc::new(Notify::new()),
        }
    }

    pub fn begin(&self, scope: &NativeSkinScope) -> Result<NativeSelection, MediaError> {
        let mut state = self.shared.lock().map_err(|_| MediaError::Closed)?;
        if !state.open {
            return Err(MediaError::Closed);
        }
        if state.in_flight {
            return Err(MediaError::Busy);
        }
        state.generation = state.generation.checked_add(1).ok_or(MediaError::Closed)?;
        // Starting any new selection revokes the previous one, even if the new file is invalid.
        state.pending = None;
        state.in_flight = true;
        Ok(NativeSelection {
            owner: self.clone(),
            scope: scope.clone(),
            generation: state.generation,
        })
    }

    pub fn consume(
        &self,
        scope: &NativeSkinScope,
        token: &str,
    ) -> Result<AdmittedSkinFile, MediaError> {
        self.consume_at(scope, token, Instant::now())
    }

    fn consume_at(
        &self,
        scope: &NativeSkinScope,
        token: &str,
        now: Instant,
    ) -> Result<AdmittedSkinFile, MediaError> {
        let mut state = self.shared.lock().map_err(|_| MediaError::Closed)?;
        if !state.open {
            return Err(MediaError::Closed);
        }
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.expires_at <= now)
        {
            state.pending = None;
        }
        if !state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.scope == *scope && pending.token == token)
        {
            return Err(MediaError::InvalidSelection);
        }
        Ok(state
            .pending
            .take()
            .ok_or(MediaError::InvalidSelection)?
            .file)
    }

    /// Closing revokes unconsumed bytes and prevents late publication from active reads.
    pub fn close(&self) {
        if let Ok(mut state) = self.shared.lock() {
            state.open = false;
            state.pending = None;
        }
    }

    pub async fn drain(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let active = self
                .shared
                .lock()
                .map(|state| state.in_flight)
                .unwrap_or(false);
            if !active {
                return;
            }
            notified.await;
        }
    }

    pub fn reopen(&self) -> Result<(), MediaError> {
        let mut state = self.shared.lock().map_err(|_| MediaError::Closed)?;
        if state.in_flight {
            return Err(MediaError::Busy);
        }
        state.open = true;
        Ok(())
    }
}

impl NativeSelection {
    pub fn publish(self, name: &str, bytes: Vec<u8>) -> Result<NativeSkinHandle, MediaError> {
        if bytes.len() > SKIN_PNG_MAX_BYTES {
            return Err(MediaError::TooLarge);
        }
        validate_skin_png(&bytes)?;
        let name = if name.trim().is_empty()
            || name.len() > 255
            || name
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            "skin.png".to_string()
        } else {
            name.to_string()
        };
        let mut state = self.owner.shared.lock().map_err(|_| MediaError::Closed)?;
        if !state.open {
            return Err(MediaError::Closed);
        }
        if state.generation != self.generation {
            return Err(MediaError::InvalidSelection);
        }
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        state.pending = Some(Pending {
            scope: self.scope.clone(),
            token: token.clone(),
            expires_at: Instant::now() + HANDLE_TTL,
            file: AdmittedSkinFile { name, bytes },
        });
        Ok(NativeSkinHandle {
            token,
            expires_in_seconds: HANDLE_TTL.as_secs(),
        })
    }
}

impl Drop for NativeSelection {
    fn drop(&mut self) {
        if let Ok(mut state) = self.owner.shared.lock() {
            state.in_flight = false;
        }
        self.owner.drained.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        super::super::tests::png(64, 64, &vec![128; 64 * 64 * 4])
    }

    #[test]
    fn exact_window_incarnation_single_use_and_expiry() {
        let owner = NativeSkinAdmission::new();
        let scope = NativeSkinScope::for_window("main").unwrap();
        let other = NativeSkinScope::for_window("main").unwrap();
        let handle = owner
            .begin(&scope)
            .unwrap()
            .publish("skin.png", png())
            .unwrap();
        assert_eq!(
            owner.consume(&other, &handle.token),
            Err(MediaError::InvalidSelection)
        );
        assert_eq!(
            owner.consume(&scope, "wrong"),
            Err(MediaError::InvalidSelection)
        );
        assert_eq!(
            owner.consume(&scope, &handle.token).unwrap().name,
            "skin.png"
        );
        assert_eq!(
            owner.consume(&scope, &handle.token),
            Err(MediaError::InvalidSelection)
        );
        let handle = owner
            .begin(&scope)
            .unwrap()
            .publish("skin.png", png())
            .unwrap();
        assert_eq!(
            owner.consume_at(&scope, &handle.token, Instant::now() + HANDLE_TTL),
            Err(MediaError::InvalidSelection)
        );
    }

    #[test]
    fn failed_replacement_revokes_previous_selection_and_releases_ingress() {
        let owner = NativeSkinAdmission::new();
        let scope = NativeSkinScope::for_window("main").unwrap();
        let handle = owner
            .begin(&scope)
            .unwrap()
            .publish("skin.png", png())
            .unwrap();
        assert!(
            owner
                .begin(&scope)
                .unwrap()
                .publish("bad.png", vec![0; 32])
                .is_err()
        );
        assert_eq!(
            owner.consume(&scope, &handle.token),
            Err(MediaError::InvalidSelection)
        );
        assert!(owner.begin(&scope).is_ok());
    }

    #[tokio::test]
    async fn close_rejects_completion_and_drain_waits_for_reader() {
        let owner = NativeSkinAdmission::new();
        let scope = NativeSkinScope::for_window("main").unwrap();
        let selection = owner.begin(&scope).unwrap();
        assert!(matches!(owner.begin(&scope), Err(MediaError::Busy)));
        owner.close();
        assert!(matches!(owner.begin(&scope), Err(MediaError::Closed)));
        assert_eq!(owner.reopen(), Err(MediaError::Busy));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), owner.drain())
                .await
                .is_err()
        );
        assert!(matches!(
            selection.publish("skin.png", png()),
            Err(MediaError::Closed)
        ));
        tokio::time::timeout(Duration::from_secs(1), owner.drain())
            .await
            .unwrap();
        owner.reopen().unwrap();
        assert!(owner.begin(&scope).is_ok());
    }
}
