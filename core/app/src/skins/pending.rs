//! Per-account last-intent queue. All locks are short and never cross I/O.

use super::{library::SkinLibraryChange, lookup::ProfileMediaError};
use crate::accounts::selection::CapturedAccount;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const SKIN_CHANGE_DEBOUNCE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub(crate) struct PendingSkin {
    pub account_id: String,
    pub account_revision: u64,
    pub credential_revision: u64,
    pub profile_revision: u64,
    pub texture_key: String,
    pub skin_revision: u64,
    pub generation: u64,
    pub due: Instant,
}

impl PendingSkin {
    pub fn matches_account(&self, account: &CapturedAccount) -> bool {
        self.account_id == account.account_id()
            && self.account_revision == account.account_revision()
            && self.credential_revision == account.credential_revision()
            && self.profile_revision == account.profile_revision()
    }
}

#[derive(Clone, Debug, Serialize, ts_rs::TS)]
pub struct PendingSkinStatus {
    pub account_id: String,
    pub texture_key: Option<String>,
    #[ts(type = "number")]
    pub generation: u64,
    pub phase: &'static str,
    pub error: Option<String>,
}

#[derive(Default)]
struct Slot {
    queued: Option<PendingSkin>,
    running: Option<PendingSkin>,
    generation: u64,
    effect_started: bool,
    cancelled: bool,
    error: Option<ProfileMediaError>,
    completed: Option<(u64, Result<(), ProfileMediaError>)>,
}

#[derive(Default)]
struct State {
    next_generation: u64,
    accounts: HashMap<String, Slot>,
}

#[derive(Default)]
pub struct PendingSkins {
    state: Mutex<State>,
}

impl PendingSkins {
    pub(crate) fn queue(
        &self,
        capture: &CapturedAccount,
        texture_key: String,
        skin_revision: u64,
    ) -> PendingSkin {
        self.insert(PendingSkin {
            account_id: capture.account_id().to_owned(),
            account_revision: capture.account_revision(),
            credential_revision: capture.credential_revision(),
            profile_revision: capture.profile_revision(),
            texture_key,
            skin_revision,
            generation: 0,
            due: Instant::now() + SKIN_CHANGE_DEBOUNCE,
        })
    }

    fn insert(&self, mut pending: PendingSkin) -> PendingSkin {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .expect("pending skin generations exhausted");
        pending.generation = state.next_generation;
        let slot = state
            .accounts
            .entry(pending.account_id.clone())
            .or_default();
        slot.generation = pending.generation;
        slot.queued = Some(pending.clone());
        // A new explicit intent supersedes a previous terminal failure. An
        // already running operation still owns its completion and cannot retry.
        slot.error = None;
        pending
    }

    pub(crate) fn claim(
        &self,
        account_id: &str,
        generation: Option<u64>,
    ) -> Result<Option<PendingSkin>, ProfileMediaError> {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        let Some(slot) = state.accounts.get_mut(account_id) else {
            return Ok(None);
        };
        if slot.running.is_some() {
            return Err(ProfileMediaError::Busy);
        }
        if slot
            .queued
            .as_ref()
            .is_some_and(|entry| generation.is_some_and(|id| id != entry.generation))
        {
            return Ok(None);
        }
        let Some(pending) = slot.queued.take() else {
            return Ok(None);
        };
        slot.running = Some(pending.clone());
        slot.cancelled = false;
        slot.effect_started = false;
        Ok(Some(pending))
    }

    /// Linearization point for cancellation versus a provider write.
    pub(crate) fn begin_effect(&self, pending: &PendingSkin) -> Result<(), ProfileMediaError> {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        let slot = state
            .accounts
            .get_mut(&pending.account_id)
            .ok_or(ProfileMediaError::Cancelled)?;
        if slot.cancelled || slot.generation != pending.generation {
            return Err(ProfileMediaError::Cancelled);
        }
        if slot
            .running
            .as_ref()
            .is_none_or(|running| running.generation != pending.generation)
        {
            return Err(ProfileMediaError::Cancelled);
        }
        slot.effect_started = true;
        Ok(())
    }

    pub(crate) fn is_current(&self, pending: &PendingSkin) -> bool {
        self.state
            .lock()
            .expect("pending skin lock poisoned")
            .accounts
            .get(&pending.account_id)
            .is_some_and(|slot| !slot.cancelled && slot.generation == pending.generation)
    }

    pub(crate) fn finish(&self, pending: &PendingSkin, outcome: Result<(), ProfileMediaError>) {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        let Some(slot) = state.accounts.get_mut(&pending.account_id) else {
            return;
        };
        if slot
            .running
            .as_ref()
            .is_none_or(|entry| entry.generation != pending.generation)
        {
            return;
        }
        slot.running = None;
        slot.completed = Some((pending.generation, outcome));
        if slot.generation != pending.generation {
            return;
        }
        slot.error = outcome.err();
        if matches!(
            slot.error,
            Some(
                ProfileMediaError::RateLimited
                    | ProfileMediaError::Unavailable
                    | ProfileMediaError::PreservationFailed
            )
        ) && !slot.cancelled
        {
            let mut retry = pending.clone();
            retry.due = Instant::now() + SKIN_CHANGE_DEBOUNCE;
            slot.queued = Some(retry);
        }
    }

    /// `Busy` means a provider effect already started, so cancellation cannot be
    /// promised. A newer queued choice can still be removed independently.
    pub fn cancel_account(&self, account_id: &str) -> Result<bool, ProfileMediaError> {
        self.cancel(account_id, None)
    }

    pub(crate) fn cancel_generation(
        &self,
        account_id: &str,
        generation: u64,
    ) -> Result<bool, ProfileMediaError> {
        self.cancel(account_id, Some(generation))
    }

    fn cancel(&self, account_id: &str, generation: Option<u64>) -> Result<bool, ProfileMediaError> {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        let Some(slot) = state.accounts.get_mut(account_id) else {
            return if generation.is_some() {
                Err(ProfileMediaError::StaleIntent)
            } else {
                Ok(false)
            };
        };
        if generation.is_some_and(|generation| slot.generation != generation) {
            return Err(ProfileMediaError::StaleIntent);
        }
        let queued = slot.queued.take().is_some();
        if queued {
            slot.error = None;
            slot.completed = None;
        }
        if slot.running.as_ref().is_some_and(|running| {
            generation.is_none_or(|generation| running.generation == generation)
        }) {
            if slot.effect_started {
                return if queued {
                    Ok(true)
                } else {
                    Err(ProfileMediaError::Busy)
                };
            }
            slot.cancelled = true;
            return Ok(true);
        }
        Ok(queued)
    }

    pub fn library_changed(&self, change: &SkinLibraryChange) {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        for slot in state.accounts.values_mut() {
            match change {
                SkinLibraryChange::Removed { texture_key } => {
                    if slot
                        .queued
                        .as_ref()
                        .is_some_and(|entry| &entry.texture_key == texture_key)
                    {
                        slot.queued = None;
                    }
                    if slot
                        .running
                        .as_ref()
                        .is_some_and(|entry| &entry.texture_key == texture_key)
                    {
                        slot.cancelled = true;
                    }
                }
                SkinLibraryChange::Replaced {
                    old_texture_key,
                    new_texture_key,
                    revision,
                } => {
                    if let Some(entry) = slot
                        .queued
                        .as_mut()
                        .filter(|entry| &entry.texture_key == old_texture_key)
                    {
                        entry.texture_key = new_texture_key.clone();
                        entry.skin_revision = *revision;
                        // The scheduler owns this intent incarnation. Retargeting
                        // its payload must not orphan it by changing its identity.
                    }
                    if slot
                        .running
                        .as_ref()
                        .is_some_and(|entry| &entry.texture_key == old_texture_key)
                    {
                        slot.cancelled = true;
                    }
                }
            }
        }
    }

    pub(crate) fn queued_accounts(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("pending skin lock poisoned")
            .accounts
            .iter()
            .filter(|(_, slot)| slot.queued.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(crate) fn active_intents(&self) -> Vec<(String, u64)> {
        self.state
            .lock()
            .expect("pending skin lock poisoned")
            .accounts
            .iter()
            .filter_map(|(id, slot)| {
                slot.queued
                    .as_ref()
                    .or(slot.running.as_ref())
                    .map(|intent| (id.clone(), intent.generation))
            })
            .collect()
    }

    pub(crate) fn queued(&self, account_id: &str) -> Option<PendingSkin> {
        self.state
            .lock()
            .expect("pending skin lock poisoned")
            .accounts
            .get(account_id)?
            .queued
            .clone()
    }

    pub(crate) fn invalidate_account(&self, account_id: &str) {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        if let Some(slot) = state.accounts.get_mut(account_id) {
            slot.queued = None;
            slot.cancelled = true;
            slot.error = None;
        }
    }

    pub(crate) fn due_generation(&self, account_id: &str, generation: u64) -> Option<Instant> {
        self.state
            .lock()
            .expect("pending skin lock poisoned")
            .accounts
            .get(account_id)?
            .queued
            .as_ref()
            .filter(|entry| entry.generation == generation)
            .map(|entry| entry.due)
    }

    pub(crate) fn expedite(&self, account_id: &str) -> Option<u64> {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        let slot = state.accounts.get_mut(account_id)?;
        slot.completed = None;
        if let Some(entry) = slot.queued.as_mut() {
            entry.due = Instant::now();
            Some(entry.generation)
        } else {
            slot.running.as_ref().map(|entry| entry.generation)
        }
    }

    pub(crate) fn expedite_generation(&self, account_id: &str, generation: u64) -> bool {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        let Some(slot) = state.accounts.get_mut(account_id) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        if let Some(entry) = slot
            .queued
            .as_mut()
            .filter(|entry| entry.generation == generation)
        {
            entry.due = Instant::now();
            slot.completed = None;
            true
        } else {
            slot.running
                .as_ref()
                .is_some_and(|entry| entry.generation == generation)
        }
    }

    /// A provider completion may advance this account's profile revision while
    /// a newer choice waits. Only choices captured from that exact account
    /// incarnation are rebased; logout, refresh and unrelated accounts are not.
    pub(crate) fn rebase_account(&self, before: &CapturedAccount, after: &CapturedAccount) {
        let mut state = self.state.lock().expect("pending skin lock poisoned");
        if let Some(entry) = state
            .accounts
            .get_mut(before.account_id())
            .and_then(|slot| slot.queued.as_mut())
        {
            if entry.matches_account(before) {
                entry.account_revision = after.account_revision();
                entry.credential_revision = after.credential_revision();
                entry.profile_revision = after.profile_revision();
            }
        }
    }

    pub(crate) fn outcome(
        &self,
        account_id: &str,
        generation: u64,
    ) -> Option<Result<(), ProfileMediaError>> {
        let state = self.state.lock().expect("pending skin lock poisoned");
        let slot = state.accounts.get(account_id)?;
        if let Some((completed, outcome)) = slot.completed {
            if completed == generation {
                return Some(outcome);
            }
        }
        if slot.generation != generation {
            return Some(Err(ProfileMediaError::Cancelled));
        }
        if slot.queued.is_none() && slot.running.is_none() {
            return Some(Err(ProfileMediaError::Cancelled));
        }
        None
    }

    pub fn status(&self, account_id: &str) -> Option<PendingSkinStatus> {
        let state = self.state.lock().expect("pending skin lock poisoned");
        let slot = state.accounts.get(account_id)?;
        Some(PendingSkinStatus {
            account_id: account_id.to_owned(),
            texture_key: slot
                .queued
                .as_ref()
                .or(slot.running.as_ref())
                .map(|entry| entry.texture_key.clone()),
            generation: slot.generation,
            phase: if slot.running.is_some() {
                "applying"
            } else if slot.queued.is_some() {
                "queued"
            } else if slot.error.is_some() {
                "failed"
            } else {
                "idle"
            },
            error: slot.error.map(|error| error.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(account: &str, texture: &str) -> PendingSkin {
        PendingSkin {
            account_id: account.into(),
            account_revision: 1,
            credential_revision: 1,
            profile_revision: 1,
            texture_key: texture.into(),
            skin_revision: 1,
            generation: 0,
            due: Instant::now(),
        }
    }

    #[test]
    fn retry_cannot_replace_a_newer_choice_or_another_account() {
        let queue = PendingSkins::default();
        queue.insert(entry("alice", "first"));
        queue.insert(entry("bob", "bob"));
        let running = queue.claim("alice", None).unwrap().unwrap();
        queue.begin_effect(&running).unwrap();
        queue.insert(entry("alice", "new"));
        queue.finish(&running, Err(ProfileMediaError::RateLimited));
        assert_eq!(
            queue.claim("alice", None).unwrap().unwrap().texture_key,
            "new"
        );
        assert_eq!(
            queue.claim("bob", None).unwrap().unwrap().texture_key,
            "bob"
        );
    }

    #[test]
    fn cancellation_before_effect_wins_and_after_effect_is_refused() {
        let queue = PendingSkins::default();
        queue.insert(entry("alice", "first"));
        let running = queue.claim("alice", None).unwrap().unwrap();
        assert_eq!(queue.cancel_account("alice"), Ok(true));
        assert_eq!(
            queue.begin_effect(&running),
            Err(ProfileMediaError::Cancelled)
        );
        queue.finish(&running, Err(ProfileMediaError::Cancelled));
        queue.insert(entry("alice", "next"));
        let running = queue.claim("alice", None).unwrap().unwrap();
        queue.begin_effect(&running).unwrap();
        assert_eq!(queue.cancel_account("alice"), Err(ProfileMediaError::Busy));
    }

    #[test]
    fn replacement_retargets_matching_queued_accounts_and_invalidates_inflight() {
        let queue = PendingSkins::default();
        queue.insert(entry("alice", "first"));
        queue.insert(entry("bob", "other"));
        let running = queue.claim("alice", None).unwrap().unwrap();
        queue.insert(entry("carol", "first"));
        queue.library_changed(&SkinLibraryChange::Replaced {
            old_texture_key: "first".into(),
            new_texture_key: "replacement".into(),
            revision: 2,
        });
        assert!(!queue.is_current(&running));
        assert_eq!(
            queue.claim("bob", None).unwrap().unwrap().texture_key,
            "other"
        );
        assert_eq!(
            queue.claim("carol", None).unwrap().unwrap().texture_key,
            "replacement"
        );
    }

    #[test]
    fn possible_remote_effect_is_not_retried() {
        let queue = PendingSkins::default();
        queue.insert(entry("alice", "first"));
        let running = queue.claim("alice", None).unwrap().unwrap();
        queue.begin_effect(&running).unwrap();
        queue.finish(&running, Err(ProfileMediaError::PartialChange));
        assert!(queue.claim("alice", None).unwrap().is_none());
        assert_eq!(queue.status("alice").unwrap().phase, "failed");
    }
}
