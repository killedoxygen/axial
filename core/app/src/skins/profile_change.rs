//! Account-bound skin intent, accepted task ownership and provider settlement.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::{Notify, oneshot};

use crate::{
    accounts::{
        directory::AccountDirectory,
        microsoft::MinecraftProfile,
        model::{AccountError, AccountKind},
        selection::CapturedAccount,
        session::{AuthError, AuthService},
    },
    library::ApplicationRootPin,
    media::{SkinVariant, texture_key},
    tasks::TaskOwner,
};

use super::{
    delivery::{
        ProfileProvider, SkinProfileResponse, TextureDelivery, active_cape, offline_profile,
        online_profile, profile_texture, unverified_profile,
    },
    library::{
        CapeUpdate, ReplaceSkinOptions, SaveSkinOptions, SavedSkinDeleteResult, SavedSkinLibrary,
        SavedSkinRecord, SkinLibraryError, UpdateSavedSkinRequest, default_profile_skin_name,
        default_username_skin_name,
    },
    lookup::{ProfileLookup, ProfileMediaError},
    pending::{PendingSkin, PendingSkinStatus, PendingSkins},
};

#[derive(Debug, thiserror::Error)]
pub enum SkinError {
    #[error(transparent)]
    Library(#[from] SkinLibraryError),
    #[error(transparent)]
    Profile(#[from] ProfileMediaError),
}

#[derive(Serialize)]
pub struct SavedSkinsResponse {
    pub skins: Vec<SavedSkinRecord>,
    pub pending_apply_texture_key: Option<String>,
}

#[derive(Default)]
struct Admission {
    closing: bool,
    active: HashSet<u64>,
    gates: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    resets: usize,
    mutations: usize,
    unsettled: HashMap<String, ProfileMediaError>,
}

/// Constructed once per isolated application profile. The root pin and task
/// owner outlive a disconnected HTTP waiter and all accepted provider effects.
pub struct ProfileMedia {
    pub library: Arc<SavedSkinLibrary>,
    pub delivery: TextureDelivery,
    accounts: Arc<AccountDirectory>,
    auth: Arc<AuthService>,
    provider: ProfileProvider,
    pending: Arc<PendingSkins>,
    tasks: TaskOwner,
    root: ApplicationRootPin,
    admission: Mutex<Admission>,
    changed: Notify,
}

impl ProfileMedia {
    pub fn new(
        library: Arc<SavedSkinLibrary>,
        accounts: Arc<AccountDirectory>,
        auth: Arc<AuthService>,
        tasks: TaskOwner,
        root: ApplicationRootPin,
    ) -> Result<Arc<Self>, SkinError> {
        Self::with_clients(
            library,
            accounts,
            auth,
            tasks,
            root,
            TextureDelivery::new(ProfileLookup::new()?),
            ProfileProvider::new()?,
        )
    }

    fn with_clients(
        library: Arc<SavedSkinLibrary>,
        accounts: Arc<AccountDirectory>,
        auth: Arc<AuthService>,
        tasks: TaskOwner,
        root: ApplicationRootPin,
        delivery: TextureDelivery,
        provider: ProfileProvider,
    ) -> Result<Arc<Self>, SkinError> {
        let pending = Arc::new(PendingSkins::default());
        let observer = pending.clone();
        library.set_change_observer(Arc::new(move |change| observer.library_changed(change)))?;
        let service = Arc::new(Self {
            library,
            accounts,
            auth,
            tasks,
            root,
            delivery,
            provider,
            pending,
            admission: Mutex::new(Admission::default()),
            changed: Notify::new(),
        });
        let weak = Arc::downgrade(&service);
        service
            .auth
            .set_account_removed_observer(Arc::new(move |account_id| {
                if let Some(service) = weak.upgrade() {
                    service.account_removed(account_id);
                }
            }));
        Ok(service)
    }

    pub fn selected(&self) -> Result<CapturedAccount, ProfileMediaError> {
        self.accounts.capture_selected().map_err(account_error)
    }

    fn selected_online(&self) -> Result<CapturedAccount, ProfileMediaError> {
        let capture = self.selected()?;
        require_online_account(&capture)?;
        Ok(capture)
    }

    pub fn profile(&self) -> Result<SkinProfileResponse, ProfileMediaError> {
        let capture = self.selected()?;
        Ok(match capture.profile() {
            Some(profile) if capture.owns_minecraft_java() => online_profile(profile),
            Some(_) => unverified_profile(capture.display_name(), capture.minecraft_uuid()),
            None if capture.kind() == AccountKind::Microsoft => {
                unverified_profile(capture.display_name(), capture.minecraft_uuid())
            }
            None => offline_profile(capture.display_name(), capture.minecraft_uuid()),
        })
    }

    pub fn list(&self) -> Result<SavedSkinsResponse, SkinError> {
        let snapshot = self.accounts.snapshot().map_err(account_error)?;
        let Some(id) = snapshot.active_account_id else {
            // Historical application does not identify a currently equipped skin.
            let mut skins = self.library.list()?;
            for skin in &mut skins {
                skin.applied_at = None;
            }
            return Ok(SavedSkinsResponse {
                skins,
                pending_apply_texture_key: None,
            });
        };
        Ok(SavedSkinsResponse {
            skins: self.library.list_for_account(id.as_str())?,
            pending_apply_texture_key: self
                .pending
                .status(id.as_str())
                .and_then(|status| status.texture_key),
        })
    }

    pub fn pending_status(&self) -> Result<Option<PendingSkinStatus>, ProfileMediaError> {
        Ok(self.pending.status(self.selected()?.account_id()))
    }

    pub async fn profile_file(&self, texture: Option<&str>) -> Result<Vec<u8>, ProfileMediaError> {
        if let Some(texture) = texture {
            return self.delivery.skin(texture).await;
        }
        let capture = self.selected_online()?;
        let bytes = self
            .delivery
            .skin(profile_texture(
                capture
                    .profile()
                    .ok_or(ProfileMediaError::AccountRequired)?,
            )?)
            .await?;
        self.accounts
            .validate_capture(&capture)
            .map_err(account_error)?;
        Ok(bytes)
    }

    pub async fn profile_file_for_identity(
        &self,
        texture: Option<&str>,
        profile_id: Option<&str>,
        skin_id: Option<&str>,
    ) -> Result<Vec<u8>, ProfileMediaError> {
        if profile_id.is_none() && skin_id.is_none() {
            return self.profile_file(texture).await;
        }
        let (Some(texture), Some(profile_id)) = (texture, profile_id) else {
            return Err(ProfileMediaError::InvalidTexture);
        };
        let account_id = crate::accounts::model::microsoft_account_id(profile_id)
            .map_err(|_| ProfileMediaError::InvalidTexture)?;
        let capture = self
            .accounts
            .capture_account(&account_id)
            .map_err(account_error)?;
        let profile = capture
            .profile()
            .ok_or(ProfileMediaError::AccountRequired)?;
        let requested_id =
            uuid::Uuid::parse_str(profile_id).map_err(|_| ProfileMediaError::InvalidTexture)?;
        let current_id =
            uuid::Uuid::parse_str(&profile.id).map_err(|_| ProfileMediaError::InvalidResponse)?;
        if requested_id != current_id {
            return Err(ProfileMediaError::StaleIdentity);
        }
        let requested = self.delivery.lookup.texture_url(texture)?;
        if !profile.skins.iter().any(|skin| {
            skin_id.is_none_or(|id| skin.id == id)
                && self
                    .delivery
                    .lookup
                    .texture_url(&skin.url)
                    .is_ok_and(|url| url == requested)
        }) {
            return Err(ProfileMediaError::StaleIdentity);
        }
        let bytes = self.delivery.skin(requested.as_str()).await?;
        self.accounts
            .validate_account_capture(&capture)
            .map_err(account_error)?;
        Ok(bytes)
    }

    pub async fn cape_file(&self, id: &str) -> Result<Vec<u8>, ProfileMediaError> {
        let capture = self.selected_online()?;
        let cape = capture
            .profile()
            .and_then(|profile| profile.capes.iter().find(|cape| cape.id == id))
            .ok_or(ProfileMediaError::MissingCape)?;
        let bytes = self.delivery.cape(&cape.url).await?;
        self.accounts
            .validate_capture(&capture)
            .map_err(account_error)?;
        Ok(bytes)
    }

    pub async fn save_upload(
        self: &Arc<Self>,
        bytes: &[u8],
        options: SaveSkinOptions,
        source: Option<&str>,
    ) -> Result<SavedSkinRecord, SkinError> {
        let capture = options
            .cape_id
            .as_deref()
            .map(|id| self.capture_cape(id))
            .transpose()?;
        let bytes = bytes.to_vec();
        let source = source.map(str::to_owned);
        self.mutate(move |service| async move {
            service
                .save_upload_inner(&bytes, options, source.as_deref(), capture)
                .await
        })
        .await
    }

    async fn save_upload_inner(
        &self,
        bytes: &[u8],
        options: SaveSkinOptions,
        source: Option<&str>,
        capture: Option<CapturedAccount>,
    ) -> Result<SavedSkinRecord, SkinError> {
        if let Some(capture) = capture {
            return self
                .auth
                .with_account_capture(&capture, || {
                    Ok(self.library.save_upload(bytes, options, source))
                })
                .await
                .map_err(auth_error)?
                .map_err(SkinError::from);
        }
        Ok(self.library.save_upload(bytes, options, source)?)
    }

    pub async fn update_saved(
        self: &Arc<Self>,
        key: &str,
        update: UpdateSavedSkinRequest,
    ) -> Result<Option<SavedSkinRecord>, SkinError> {
        let capture = match &update.cape_id {
            CapeUpdate::Set(id) => Some(self.capture_cape(id)?),
            _ => None,
        };
        let key = key.to_owned();
        self.mutate(move |service| async move {
            service.update_saved_inner(&key, update, capture).await
        })
        .await
    }

    async fn update_saved_inner(
        &self,
        key: &str,
        update: UpdateSavedSkinRequest,
        capture: Option<CapturedAccount>,
    ) -> Result<Option<SavedSkinRecord>, SkinError> {
        if let Some(capture) = capture {
            return self
                .auth
                .with_account_capture(&capture, || Ok(self.library.update_metadata(key, update)))
                .await
                .map_err(auth_error)?
                .map_err(SkinError::from);
        }
        Ok(self.library.update_metadata(key, update)?)
    }

    pub async fn replace_saved(
        self: &Arc<Self>,
        key: &str,
        bytes: &[u8],
        options: ReplaceSkinOptions,
    ) -> Result<Option<SavedSkinRecord>, SkinError> {
        let capture = match &options.cape_id {
            CapeUpdate::Set(id) => Some(self.capture_cape(id)?),
            _ => None,
        };
        let key = key.to_owned();
        let bytes = bytes.to_vec();
        self.mutate(move |service| async move {
            service
                .replace_saved_inner(&key, &bytes, options, capture)
                .await
        })
        .await
    }

    async fn replace_saved_inner(
        &self,
        key: &str,
        bytes: &[u8],
        options: ReplaceSkinOptions,
        capture: Option<CapturedAccount>,
    ) -> Result<Option<SavedSkinRecord>, SkinError> {
        if let Some(capture) = capture {
            return self
                .auth
                .with_account_capture(&capture, || {
                    Ok(self.library.replace_texture(key, bytes, options))
                })
                .await
                .map_err(auth_error)?
                .map_err(SkinError::from);
        }
        Ok(self.library.replace_texture(key, bytes, options)?)
    }

    fn capture_cape(&self, id: &str) -> Result<CapturedAccount, ProfileMediaError> {
        let capture = self.selected_online()?;
        if !capture
            .profile()
            .is_some_and(|profile| profile.capes.iter().any(|cape| cape.id == id))
        {
            return Err(ProfileMediaError::MissingCape);
        }
        Ok(capture)
    }

    pub async fn save_profile(
        self: &Arc<Self>,
        name: Option<String>,
        variant: Option<SkinVariant>,
        mark_current: bool,
        expected_account_id: &str,
        expected_selection_revision: u64,
    ) -> Result<SavedSkinRecord, SkinError> {
        let capture = self
            .accounts
            .with_selected_account(
                expected_account_id,
                expected_selection_revision,
                |capture| -> Result<_, ProfileMediaError> {
                    require_online_account(capture)?;
                    Ok(capture.clone())
                },
            )
            .map_err(account_error)??;
        self.mutate(move |service| async move {
            service
                .save_profile_inner(name, variant, mark_current, capture)
                .await
        })
        .await
    }

    async fn save_profile_inner(
        &self,
        name: Option<String>,
        variant: Option<SkinVariant>,
        mark_current: bool,
        capture: CapturedAccount,
    ) -> Result<SavedSkinRecord, SkinError> {
        let profile = capture
            .profile()
            .ok_or(ProfileMediaError::AccountRequired)?;
        let bytes = self.delivery.skin(profile_texture(profile)?).await?;
        let actual_variant = profile_variant(profile);
        let options = SaveSkinOptions {
            name: name.unwrap_or_else(|| default_profile_skin_name(&profile.name)),
            variant: Some(variant.unwrap_or(actual_variant)),
            cape_id: active_cape(profile).map(str::to_owned),
        };
        self.auth
            .with_account_capture(&capture, || {
                Ok((|| {
                    if mark_current && options.variant == Some(actual_variant) {
                        self.library
                            .save_current_profile(capture.account_id(), &bytes, options)
                    } else {
                        self.library.save_from_profile(&bytes, options)
                    }
                })())
            })
            .await
            .map_err(auth_error)?
            .map_err(SkinError::from)
    }

    pub async fn save_username(
        self: &Arc<Self>,
        username: &str,
        name: Option<String>,
        variant: Option<SkinVariant>,
    ) -> Result<SavedSkinRecord, SkinError> {
        let username = username.to_owned();
        self.mutate(move |service| async move {
            service.save_username_inner(&username, name, variant).await
        })
        .await
    }

    async fn save_username_inner(
        &self,
        username: &str,
        name: Option<String>,
        variant: Option<SkinVariant>,
    ) -> Result<SavedSkinRecord, SkinError> {
        let profile = self.delivery.lookup.lookup(username).await?;
        let bytes = self.delivery.skin(&profile.texture_url).await?;
        Ok(self.library.save_from_username(
            &bytes,
            SaveSkinOptions {
                name: name.unwrap_or_else(|| default_username_skin_name(&profile.username)),
                variant: variant.or(Some(if profile.variant == "slim" {
                    SkinVariant::Slim
                } else {
                    SkinVariant::Classic
                })),
                cape_id: None,
            },
        )?)
    }

    /// Admission completes before returning. Dropping the caller only drops its
    /// waiter; the task owner retains this intent and its application root.
    pub fn queue(
        self: &Arc<Self>,
        key: &str,
        expected_account_id: &str,
        expected_selection_revision: u64,
    ) -> Result<PendingSkinStatus, SkinError> {
        let mut admission = self.admission.lock().expect("skin admission lock poisoned");
        if admission.closing {
            return Err(ProfileMediaError::ShuttingDown.into());
        }
        self.root
            .revalidate()
            .map_err(|_| ProfileMediaError::ShuttingDown)?;
        let (send, receive) = oneshot::channel::<Option<PendingSkin>>();
        let service = self.clone();
        // Reserve bounded execution before changing the user's previous choice.
        let handle = self
            .tasks
            .try_spawn(self.root.clone(), move |_| async move {
                if let Ok(Some(pending)) = receive.await {
                    service.run_scheduled(&pending).await;
                    service.finished_worker(&pending);
                    service.changed.notify_waiters();
                }
            })
            .map_err(|_| ProfileMediaError::Busy)?;
        let pending = self
            .library
            .with_skin(key, |snapshot| {
                Ok(snapshot.map(|snapshot| {
                    self.accounts
                        .with_selected_account(
                            expected_account_id,
                            expected_selection_revision,
                            |capture| -> Result<_, ProfileMediaError> {
                                require_online_account(capture)?;
                                Ok(self.pending.queue(
                                    capture,
                                    snapshot.record.texture_key,
                                    snapshot.revision,
                                ))
                            },
                        )
                        .map_err(account_error)?
                }))
            })?
            .ok_or(ProfileMediaError::SavedSkinMissing)??;
        admission.active.insert(pending.generation);
        let status = self
            .pending
            .status(&pending.account_id)
            .expect("just queued skin");
        let _ = send.send(Some(pending));
        drop(handle);
        Ok(status)
    }

    pub fn cancel_selected(
        &self,
        expected_account_id: &str,
        expected_selection_revision: u64,
        expected_generation: u64,
    ) -> Result<bool, ProfileMediaError> {
        let result = self
            .accounts
            .with_selected_account(
                expected_account_id,
                expected_selection_revision,
                |capture| {
                    self.pending
                        .cancel_generation(capture.account_id(), expected_generation)
                },
            )
            .map_err(account_error)?;
        self.changed.notify_waiters();
        result
    }

    /// Expedite only the intent accepted by this request. A later choice cannot
    /// be applied by an older request whose response is still in flight.
    pub async fn flush_generation(
        self: &Arc<Self>,
        account_id: &str,
        generation: u64,
    ) -> Result<(), ProfileMediaError> {
        if !self.pending.expedite_generation(account_id, generation) {
            return self
                .pending
                .outcome(account_id, generation)
                .unwrap_or(Err(ProfileMediaError::StaleIntent));
        }
        self.wait_for_generation(account_id, generation).await
    }

    pub async fn flush_account(
        self: &Arc<Self>,
        account_id: &str,
    ) -> Result<(), ProfileMediaError> {
        let Some(generation) = self.pending.expedite(account_id) else {
            return Ok(());
        };
        self.wait_for_generation(account_id, generation).await
    }

    async fn wait_for_generation(
        self: &Arc<Self>,
        account_id: &str,
        generation: u64,
    ) -> Result<(), ProfileMediaError> {
        {
            let mut admission = self.admission.lock().expect("skin admission lock poisoned");
            if !admission.active.contains(&generation) {
                if let Some(pending) = self.pending.queued(account_id) {
                    self.spawn_worker(pending, &mut admission)?;
                }
            }
        }
        self.changed.notify_waiters();
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.pending.outcome(account_id, generation) {
                return outcome;
            }
            notified.await;
        }
    }

    pub async fn flush_selected(
        self: &Arc<Self>,
        expected_account_id: &str,
        expected_selection_revision: u64,
        expected_generation: u64,
    ) -> Result<usize, ProfileMediaError> {
        let expedited = self
            .accounts
            .with_selected_account(
                expected_account_id,
                expected_selection_revision,
                |capture| {
                    self.pending
                        .expedite_generation(capture.account_id(), expected_generation)
                },
            )
            .map_err(account_error)?;
        if !expedited {
            return Err(ProfileMediaError::StaleIntent);
        }
        self.wait_for_generation(expected_account_id, expected_generation)
            .await?;
        Ok(1)
    }

    /// Native close flushes accepted intent without closing command admission.
    /// Its following atomic task-owner fence detects any newly accepted work.
    pub async fn flush_pending(self: &Arc<Self>) -> Result<usize, ProfileMediaError> {
        let intents = self.pending.active_intents();
        tokio::time::timeout(Duration::from_secs(60), async {
            let mut applied = 0;
            let mut failure = None;
            for (account, generation) in intents {
                if !self.pending.expedite_generation(&account, generation) {
                    failure.get_or_insert(ProfileMediaError::Cancelled);
                    continue;
                }
                match self.wait_for_generation(&account, generation).await {
                    Ok(()) => applied += 1,
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            let admission = self.admission.lock().expect("skin admission lock poisoned");
            if let Some(error) = failure.or_else(|| admission.unsettled.values().next().copied()) {
                return Err(error);
            }
            Ok(applied)
        })
        .await
        .map_err(|_| ProfileMediaError::Busy)?
    }

    /// Flush first, then let composition close its shared task owner and release
    /// the library/service references before resetting the application root.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), ProfileMediaError> {
        self.admission
            .lock()
            .expect("skin admission lock poisoned")
            .closing = true;
        for account in self.pending.queued_accounts() {
            self.pending.expedite(&account);
        }
        self.changed.notify_waiters();
        let joined = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let idle = {
                    let admission = self.admission.lock().expect("skin admission lock poisoned");
                    admission.active.is_empty() && admission.resets == 0 && admission.mutations == 0
                };
                if idle {
                    break;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| ProfileMediaError::Busy);
        let mut admission = self.admission.lock().expect("skin admission lock poisoned");
        let result = joined.and_then(|()| {
            if !self.pending.queued_accounts().is_empty() {
                return Err(ProfileMediaError::Unavailable);
            }
            if let Some(error) = admission.unsettled.values().next() {
                return Err(*error);
            }
            Ok(())
        });
        if result.is_err() {
            admission.closing = false;
            for account in self.pending.queued_accounts() {
                if let Some(pending) = self.pending.queued(&account) {
                    if !admission.active.contains(&pending.generation) {
                        // Failure to reserve another worker leaves the exact
                        // intent available for an explicit flush or cancellation.
                        let _ = self.spawn_worker(pending, &mut admission);
                    }
                }
            }
        }
        result
    }

    fn spawn_worker(
        self: &Arc<Self>,
        pending: PendingSkin,
        admission: &mut Admission,
    ) -> Result<(), ProfileMediaError> {
        let service = self.clone();
        let generation = pending.generation;
        let work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(async move {
                service.run_scheduled(&pending).await;
                service.finished_worker(&pending);
            });
        self.tasks
            .try_spawn(self.root.clone(), move |_| work)
            .map_err(|_| ProfileMediaError::Busy)?;
        admission.active.insert(generation);
        Ok(())
    }

    fn finished_worker(self: &Arc<Self>, pending: &PendingSkin) {
        let mut admission = self.admission.lock().expect("skin admission lock poisoned");
        admission.active.remove(&pending.generation);
        if !admission.closing {
            if let Some(next) = self.pending.queued(&pending.account_id) {
                if !admission.active.contains(&next.generation) {
                    let _ = self.spawn_worker(next, &mut admission);
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// Composition calls this after the account owner's acknowledged removal.
    /// Revocation cancels local intent; it does not promise to undo a provider
    /// request that was already admitted.
    pub fn account_removed(&self, account_id: &str) {
        self.pending.invalidate_account(account_id);
        self.admission
            .lock()
            .expect("skin admission lock poisoned")
            .unsettled
            .remove(account_id);
        self.changed.notify_waiters();
    }

    pub async fn delete_saved(
        self: &Arc<Self>,
        key: &str,
    ) -> Result<SavedSkinDeleteResult, SkinError> {
        let key = key.to_owned();
        self.mutate(move |service| async move { Ok(service.library.delete_unapplied(&key)?) })
            .await
    }

    async fn mutate<T, F, Fut>(self: &Arc<Self>, apply: F) -> Result<T, SkinError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, SkinError>> + Send + 'static,
    {
        let service = self.clone();
        let handle = {
            let mut admission = self.admission.lock().expect("skin admission lock poisoned");
            if admission.closing {
                return Err(ProfileMediaError::ShuttingDown.into());
            }
            self.root
                .revalidate()
                .map_err(|_| ProfileMediaError::ShuttingDown)?;
            let handle = self
                .tasks
                .try_spawn(self.root.clone(), move |_| async move {
                    let outcome = apply(service.clone()).await;
                    service
                        .admission
                        .lock()
                        .expect("skin admission lock poisoned")
                        .mutations -= 1;
                    service.changed.notify_waiters();
                    outcome
                })
                .map_err(|_| ProfileMediaError::Busy)?;
            admission.mutations += 1;
            handle
        };
        handle
            .join()
            .await
            .map_err(|_| ProfileMediaError::SettlementFailed)?
    }

    fn gate(&self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.admission
            .lock()
            .expect("skin admission lock poisoned")
            .gates
            .entry(account.to_owned())
            .or_default()
            .clone()
    }

    async fn run_scheduled(&self, scheduled: &PendingSkin) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let Some(due) = self
                .pending
                .due_generation(&scheduled.account_id, scheduled.generation)
            else {
                return;
            };
            if due > Instant::now() {
                tokio::select! { _ = tokio::time::sleep_until(due.into()) => {}, _ = &mut notified => {} }
                continue;
            }
            let gate = self.gate(&scheduled.account_id);
            let _guard = gate.lock().await;
            let pending = match self
                .pending
                .claim(&scheduled.account_id, Some(scheduled.generation))
            {
                Ok(Some(pending)) => pending,
                _ => return,
            };
            let outcome = self.apply(&pending).await;
            self.pending.finish(&pending, outcome);
            self.changed.notify_waiters();
            if self
                .admission
                .lock()
                .expect("skin admission lock poisoned")
                .closing
            {
                return;
            }
            // Only known pre-effect failures are put back in the queue. An
            // ambiguous or partial provider write is never automatically retried.
        }
    }

    async fn apply(&self, pending: &PendingSkin) -> Result<(), ProfileMediaError> {
        let capture = self
            .accounts
            .capture_account(&pending.account_id)
            .map_err(account_error)?;
        if !pending.matches_account(&capture) {
            return Err(ProfileMediaError::StaleIdentity);
        }
        let credentials = self.auth.credentials(&capture).await.map_err(auth_error)?;
        let token = credentials.minecraft_access_token();
        let profile = self.provider.sync(token, capture.minecraft_uuid()).await?;
        let skin = self
            .library
            .get(&pending.texture_key)
            .map_err(|_| ProfileMediaError::SavedSkinMissing)?
            .filter(|skin| skin.revision == pending.skin_revision)
            .ok_or(ProfileMediaError::SavedSkinMissing)?;
        if skin
            .record
            .cape_id
            .as_ref()
            .is_some_and(|id| !profile.capes.iter().any(|cape| &cape.id == id))
        {
            return Err(ProfileMediaError::MissingCape);
        }
        let png = self
            .library
            .read_png(&pending.texture_key)
            .map_err(|_| ProfileMediaError::SavedSkinMissing)?
            .ok_or(ProfileMediaError::SavedSkinMissing)?;
        self.preserve(&capture, &profile).await?;
        self.auth
            .with_account_capture(&capture, || {
                Ok(self.library.with_skin(&pending.texture_key, |skin| {
                    Ok(
                        if skin.is_none_or(|skin| skin.revision != pending.skin_revision) {
                            Err(ProfileMediaError::SavedSkinMissing)
                        } else {
                            self.pending.begin_effect(pending)
                        },
                    )
                }))
            })
            .await
            .map_err(auth_error)?
            .map_err(|_| ProfileMediaError::SavedSkinMissing)??;
        self.admission
            .lock()
            .expect("skin admission lock poisoned")
            .unsettled
            .insert(
                capture.account_id().to_owned(),
                ProfileMediaError::UncertainChange,
            );
        let uploaded = match self
            .provider
            .upload(token, skin.record.variant.as_str(), png, &profile.id)
            .await
        {
            Ok(profile) => profile,
            Err(error) => return self.failed_effect(&capture, token, error).await,
        };
        let uploaded = match uploaded {
            Some(profile) => profile,
            None => match self.provider.sync(token, &profile.id).await {
                Ok(profile) => profile,
                Err(_) => {
                    return self
                        .failed_effect(&capture, token, ProfileMediaError::SettlementFailed)
                        .await;
                }
            },
        };
        // A superseding intent cannot stop a write already admitted, but it can
        // prevent this old intent from issuing a second, unnecessary cape write.
        if !self.pending.is_current(pending) {
            self.commit(&capture, uploaded, None).await?;
            return Err(ProfileMediaError::Cancelled);
        }
        let admitted = self
            .auth
            .with_account_capture(&capture, || Ok(self.pending.begin_effect(pending)))
            .await
            .map_err(auth_error)?;
        if let Err(error) = admitted {
            self.commit(&capture, uploaded, None).await?;
            return Err(error);
        }
        let final_profile = match self
            .provider
            .cape(token, &uploaded, skin.record.cape_id.as_deref())
            .await
        {
            Ok(Some(profile)) => profile,
            Ok(None) => match self.provider.sync(token, &profile.id).await {
                Ok(profile) => profile,
                Err(_) => {
                    return self
                        .failed_effect(&capture, token, ProfileMediaError::SettlementFailed)
                        .await;
                }
            },
            Err(_) => {
                return self
                    .failed_effect(&capture, token, ProfileMediaError::PartialChange)
                    .await;
            }
        };
        self.commit(&capture, final_profile, Some(pending)).await
    }

    async fn preserve(
        &self,
        capture: &CapturedAccount,
        profile: &MinecraftProfile,
    ) -> Result<(), ProfileMediaError> {
        let url = match profile_texture(profile) {
            Ok(url) => url,
            Err(ProfileMediaError::MissingSkin) => return Ok(()),
            Err(error) => return Err(error),
        };
        let bytes = self
            .delivery
            .skin(url)
            .await
            .map_err(|_| ProfileMediaError::PreservationFailed)?;
        self.auth
            .with_account_capture(capture, || {
                Ok((|| {
                    if self.library.get(&texture_key(&bytes))?.is_none() {
                        self.library.save_from_profile(
                            &bytes,
                            SaveSkinOptions {
                                name: default_profile_skin_name(&profile.name),
                                variant: Some(profile_variant(profile)),
                                cape_id: active_cape(profile).map(str::to_owned),
                            },
                        )?;
                    }
                    Ok::<_, SkinLibraryError>(())
                })())
            })
            .await
            .map_err(auth_error)?
            .map_err(|_| ProfileMediaError::PreservationFailed)
    }

    async fn commit(
        &self,
        before: &CapturedAccount,
        profile: MinecraftProfile,
        pending: Option<&PendingSkin>,
    ) -> Result<(), ProfileMediaError> {
        let current = self
            .auth
            .commit_profile(before, profile)
            .await
            .map_err(auth_error)?;
        self.pending.rebase_account(before, &current);
        self.auth
            .with_account_capture(&current, || {
                Ok((|| {
                    self.library.clear_applied(current.account_id())?;
                    if let Some(pending) =
                        pending.filter(|pending| self.pending.is_current(pending))
                    {
                        if !self.library.mark_applied_if_current(
                            current.account_id(),
                            &pending.texture_key,
                            pending.skin_revision,
                        )? {
                            return Err(SkinLibraryError::Conflict);
                        }
                    }
                    Ok::<_, SkinLibraryError>(())
                })())
            })
            .await
            .map_err(auth_error)?
            .map_err(|_| ProfileMediaError::SettlementFailed)?;
        self.admission
            .lock()
            .expect("skin admission lock poisoned")
            .unsettled
            .remove(current.account_id());
        if pending.is_some_and(|pending| !self.pending.is_current(pending)) {
            return Err(ProfileMediaError::Cancelled);
        }
        Ok(())
    }

    async fn failed_effect(
        &self,
        capture: &CapturedAccount,
        token: &str,
        error: ProfileMediaError,
    ) -> Result<(), ProfileMediaError> {
        if matches!(
            error,
            ProfileMediaError::UncertainChange
                | ProfileMediaError::PartialChange
                | ProfileMediaError::SettlementFailed
        ) {
            if let Ok(profile) = self.provider.sync(token, capture.minecraft_uuid()).await {
                self.commit(capture, profile, None).await?;
            } else {
                self.auth
                    .with_account_capture(capture, || {
                        Ok(self.library.clear_applied(capture.account_id()))
                    })
                    .await
                    .map_err(auth_error)?
                    .map_err(|_| ProfileMediaError::SettlementFailed)?;
            }
        } else {
            self.admission
                .lock()
                .expect("skin admission lock poisoned")
                .unsettled
                .remove(capture.account_id());
        }
        Err(error)
    }

    pub async fn reset(
        self: &Arc<Self>,
        skin: bool,
        expected_account_id: &str,
        expected_selection_revision: u64,
    ) -> Result<(), ProfileMediaError> {
        let service = self.clone();
        let handle = {
            let mut admission = self.admission.lock().expect("skin admission lock poisoned");
            if admission.closing {
                return Err(ProfileMediaError::ShuttingDown);
            }
            let (send, receive) = oneshot::channel::<CapturedAccount>();
            let handle = self
                .tasks
                .try_spawn(self.root.clone(), move |_| async move {
                    let capture = receive.await.map_err(|_| ProfileMediaError::Cancelled)?;
                    let outcome = service.reset_account(&capture, skin).await;
                    {
                        let mut admission = service
                            .admission
                            .lock()
                            .expect("skin admission lock poisoned");
                        admission.resets -= 1;
                    }
                    service.changed.notify_waiters();
                    outcome
                })
                .map_err(|_| ProfileMediaError::Busy)?;
            let capture = self
                .accounts
                .with_selected_account(
                    expected_account_id,
                    expected_selection_revision,
                    |capture| -> Result<_, ProfileMediaError> {
                        require_online_account(capture)?;
                        self.pending.cancel_account(capture.account_id())?;
                        Ok(capture.clone())
                    },
                )
                .map_err(account_error)??;
            admission.resets += 1;
            let _ = send.send(capture);
            self.changed.notify_waiters();
            handle
        };
        handle
            .join()
            .await
            .map_err(|_| ProfileMediaError::SettlementFailed)?
    }

    async fn reset_account(
        &self,
        capture: &CapturedAccount,
        skin: bool,
    ) -> Result<(), ProfileMediaError> {
        let service = self;
        let gate = service.gate(capture.account_id());
        let _guard = gate.lock().await;
        let credentials = service
            .auth
            .credentials(&capture)
            .await
            .map_err(auth_error)?;
        let token = credentials.minecraft_access_token();
        let profile = service
            .provider
            .sync(token, capture.minecraft_uuid())
            .await?;
        service.preserve(&capture, &profile).await?;
        service
            .auth
            .with_account_capture(&capture, || Ok(()))
            .await
            .map_err(auth_error)?;
        service
            .admission
            .lock()
            .expect("skin admission lock poisoned")
            .unsettled
            .insert(
                capture.account_id().to_owned(),
                ProfileMediaError::UncertainChange,
            );
        let changed = if skin {
            service.provider.reset_skin(token, &profile.id).await
        } else {
            service.provider.cape(token, &profile, None).await
        };
        let final_profile = match changed {
            Ok(Some(profile)) => profile,
            Ok(None) => match service.provider.sync(token, &profile.id).await {
                Ok(profile) => profile,
                Err(_) => {
                    return service
                        .failed_effect(&capture, token, ProfileMediaError::SettlementFailed)
                        .await;
                }
            },
            Err(error) => return service.failed_effect(&capture, token, error).await,
        };
        service.commit(&capture, final_profile, None).await
    }
}

fn require_online_account(capture: &CapturedAccount) -> Result<(), ProfileMediaError> {
    if capture.kind() != AccountKind::Microsoft || capture.profile().is_none() {
        return Err(ProfileMediaError::AccountRequired);
    }
    if !capture.owns_minecraft_java() {
        return Err(ProfileMediaError::OwnershipMissing);
    }
    Ok(())
}

fn profile_variant(profile: &MinecraftProfile) -> SkinVariant {
    let selected = profile_texture(profile)
        .ok()
        .and_then(|url| profile.skins.iter().find(|skin| skin.url == url));
    if selected.is_some_and(|skin| skin.variant.eq_ignore_ascii_case("slim")) {
        SkinVariant::Slim
    } else {
        SkinVariant::Classic
    }
}

fn account_error(error: AccountError) -> ProfileMediaError {
    match error {
        AccountError::StaleCapture | AccountError::NotFound => ProfileMediaError::StaleIdentity,
        AccountError::NoSelection | AccountError::NotMicrosoft => {
            ProfileMediaError::AccountRequired
        }
        _ => ProfileMediaError::Unavailable,
    }
}

fn auth_error(error: AuthError) -> ProfileMediaError {
    match error {
        AuthError::Account(error) => account_error(error),
        AuthError::SignInRequired | AuthError::LoginExpired => ProfileMediaError::AccountRequired,
        AuthError::OwnershipMissing => ProfileMediaError::OwnershipMissing,
        AuthError::Credentials(crate::accounts::credential_store::CredentialError::Stale) => {
            ProfileMediaError::StaleIdentity
        }
        AuthError::Credentials(crate::accounts::credential_store::CredentialError::Unresolved) => {
            ProfileMediaError::AccountRequired
        }
        _ => ProfileMediaError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skins::{
        store::{MIGRATION, SavedSkinStore},
        tests::{Reply, png, server},
    };
    use crate::{
        accounts::{
            credential_store::CredentialStore,
            credentials::Credentials,
            microsoft::{MinecraftCape, MinecraftSkin},
            model::{MicrosoftIdentity, microsoft_account_id},
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        storage::MetadataStore,
    };

    const PROFILE_ID: &str = "12345678123442348234123456789abc";
    const FIRST_URL: &str = "https://textures.minecraft.net/texture/1111111111111111111111111111111111111111111111111111111111111111";
    const SECOND_URL: &str = "https://textures.minecraft.net/texture/2222222222222222222222222222222222222222222222222222222222222222";

    fn queue(service: &Arc<ProfileMedia>, key: &str) {
        let capture = service.selected().unwrap();
        service
            .queue(key, capture.account_id(), capture.selection_revision())
            .unwrap();
    }

    fn profile(url: Option<&str>, cape: bool) -> MinecraftProfile {
        MinecraftProfile {
            id: PROFILE_ID.into(),
            name: "PlayerOne".into(),
            skins: url
                .into_iter()
                .map(|url| MinecraftSkin {
                    id: "skin".into(),
                    state: "ACTIVE".into(),
                    url: url.into(),
                    variant: "CLASSIC".into(),
                })
                .collect(),
            capes: if cape {
                vec![MinecraftCape {
                    id: "cape-one".into(),
                    state: "ACTIVE".into(),
                    url: "https://textures.minecraft.net/texture/cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
                }]
            } else {
                vec![]
            },
        }
    }

    async fn fixture(
        base: &str,
        initial: MinecraftProfile,
    ) -> (
        tempfile::TempDir,
        Arc<ProfileMedia>,
        CapturedAccount,
        String,
        String,
    ) {
        let (directory, service, capture, first, second, _) =
            fixture_with_credentials(base, initial).await;
        (directory, service, capture, first, second)
    }

    async fn fixture_with_credentials(
        base: &str,
        initial: MinecraftProfile,
    ) -> (
        tempfile::TempDir,
        Arc<ProfileMedia>,
        CapturedAccount,
        String,
        String,
        Arc<CredentialStore>,
    ) {
        assert!(crate::accounts::microsoft::validate_profile(&initial).is_ok());
        let directory = tempfile::tempdir().unwrap();
        let roots = match LibraryLifecycle::open(&directory.path().canonicalize().unwrap()) {
            LibraryOpenOutcome::Ready(roots) => roots,
            _ => panic!("isolated root"),
        };
        let root = roots.admit_application_root().unwrap();
        let metadata = Arc::new(MetadataStore::in_memory().unwrap());
        let accounts = Arc::new(AccountDirectory::new(metadata.clone()).unwrap());
        metadata.migrate(&[MIGRATION]).unwrap();
        let credentials = Arc::new(CredentialStore::isolated_for_tests());
        let id = microsoft_account_id(PROFILE_ID).unwrap();
        let fence = credentials.begin_change(&id, 0).await.unwrap();
        let receipt = credentials
            .save(
                &fence,
                Credentials::new(
                    "fixture-microsoft".into(),
                    Some("fixture-refresh".into()),
                    u64::MAX / 2,
                    "fixture-minecraft".into(),
                    u64::MAX / 2,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let capture = accounts
            .commit_microsoft(
                accounts.selection_revision().unwrap(),
                MicrosoftIdentity {
                    login_id: uuid::Uuid::new_v4().to_string(),
                    profile_id: PROFILE_ID.into(),
                    display_name: initial.name.clone(),
                    credential_revision: receipt.revision(),
                    owns_minecraft_java: true,
                    profile: initial,
                },
            )
            .unwrap();
        let tasks = TaskOwner::new(32).unwrap();
        let auth = Arc::new(AuthService::new(
            accounts.clone(),
            credentials.clone(),
            tasks.clone(),
        ));
        let library = Arc::new(SavedSkinLibrary::new(
            SavedSkinStore::new(metadata),
            root.clone(),
        ));
        let first = library
            .save_upload(
                &png(11),
                SaveSkinOptions {
                    name: "First".into(),
                    variant: Some(SkinVariant::Classic),
                    cape_id: None,
                },
                None,
            )
            .unwrap();
        let second = library
            .save_upload(
                &png(22),
                SaveSkinOptions {
                    name: "Second".into(),
                    variant: Some(SkinVariant::Classic),
                    cape_id: None,
                },
                None,
            )
            .unwrap();
        let delivery = TextureDelivery::new(ProfileLookup::new().unwrap());
        delivery.fixture_skin(FIRST_URL, &png(11)).await;
        delivery.fixture_skin(SECOND_URL, &png(22)).await;
        let service = ProfileMedia::with_clients(
            library,
            accounts,
            auth,
            tasks,
            root,
            delivery,
            ProfileProvider::fixture(base),
        )
        .unwrap();
        (
            directory,
            service,
            capture,
            first.texture_key,
            second.texture_key,
            credentials,
        )
    }

    async fn assert_stale_profile_save_preserves_publication(reselected: bool) {
        let (_directory, service, confirmed, first, second, credentials) =
            fixture_with_credentials("http://127.0.0.1:9", profile(Some(FIRST_URL), false)).await;
        let mut other_profile = profile(Some(SECOND_URL), false);
        other_profile.id = "22345678123442348234123456789abc".into();
        other_profile.name = "PlayerTwo".into();
        other_profile.skins[0].variant = "SLIM".into();
        let other_id = microsoft_account_id(&other_profile.id).unwrap();
        let fence = credentials.begin_change(&other_id, 0).await.unwrap();
        let receipt = credentials
            .save(
                &fence,
                Credentials::new(
                    "fixture-second-microsoft".into(),
                    Some("fixture-second-refresh".into()),
                    u64::MAX / 2,
                    "fixture-second-minecraft".into(),
                    u64::MAX / 2,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let other = service
            .accounts
            .commit_microsoft(
                service.accounts.selection_revision().unwrap(),
                MicrosoftIdentity {
                    login_id: uuid::Uuid::new_v4().to_string(),
                    profile_id: other_profile.id.clone(),
                    display_name: other_profile.name.clone(),
                    credential_revision: receipt.revision(),
                    owns_minecraft_java: true,
                    profile: other_profile,
                },
            )
            .unwrap();
        if reselected {
            service.accounts.select(confirmed.account_id()).unwrap();
            let mut changed = profile(Some(FIRST_URL), false);
            changed.skins[0].variant = "SLIM".into();
            service
                .auth
                .commit_profile(&confirmed, changed)
                .await
                .unwrap();
        }
        for key in std::iter::once(&second).chain(reselected.then_some(&first)) {
            service
                .library
                .update_metadata(
                    key,
                    UpdateSavedSkinRequest {
                        variant: Some("slim".into()),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
        }
        service
            .library
            .mark_applied(confirmed.account_id(), &first)
            .unwrap();
        service
            .library
            .mark_applied(other.account_id(), &second)
            .unwrap();
        let current = service.selected().unwrap();
        assert_eq!(
            current.account_id(),
            if reselected {
                confirmed.account_id()
            } else {
                other.account_id()
            }
        );
        assert!(service.auth.credentials(&current).await.is_ok());
        let before = [
            service.library.get(&first).unwrap(),
            service.library.get(&second).unwrap(),
        ];
        let pngs = [
            service.library.read_png(&first).unwrap(),
            service.library.read_png(&second).unwrap(),
        ];
        let markers = [
            service
                .library
                .list_for_account(confirmed.account_id())
                .unwrap(),
            service
                .library
                .list_for_account(other.account_id())
                .unwrap(),
        ];

        // The delayed caller derived Classic and mark_current from the original A profile.
        let result = service
            .save_profile(
                None,
                Some(SkinVariant::Classic),
                true,
                confirmed.account_id(),
                confirmed.selection_revision(),
            )
            .await;
        let after = [
            service.library.get(&first).unwrap(),
            service.library.get(&second).unwrap(),
        ];
        let after_pngs = [
            service.library.read_png(&first).unwrap(),
            service.library.read_png(&second).unwrap(),
        ];
        let after_markers = [
            service
                .library
                .list_for_account(confirmed.account_id())
                .unwrap(),
            service
                .library
                .list_for_account(other.account_id())
                .unwrap(),
        ];
        service.shutdown().await.unwrap();
        assert_eq!(
            after, before,
            "stale profile save must preserve exact saved metadata and revisions"
        );
        assert_eq!(after_pngs, pngs);
        assert_eq!(after_markers, markers);
        assert!(matches!(
            result,
            Err(SkinError::Profile(ProfileMediaError::StaleIdentity))
        ));
    }

    #[tokio::test]
    async fn stale_profile_save_cannot_publish_another_accounts_texture_or_clear_its_marker() {
        assert_stale_profile_save_preserves_publication(false).await;
    }

    #[tokio::test]
    async fn stale_profile_save_cannot_publish_after_reselection() {
        assert_stale_profile_save_preserves_publication(true).await;
    }

    #[tokio::test]
    async fn current_profile_save_publishes_its_texture_and_account_marker() {
        let (_directory, service, capture, first, second) =
            fixture("http://127.0.0.1:9", profile(Some(FIRST_URL), false)).await;
        let png = service.library.read_png(&first).unwrap();
        let untouched = service.library.get(&second).unwrap();
        let saved = service
            .save_profile(
                None,
                Some(SkinVariant::Classic),
                true,
                capture.account_id(),
                capture.selection_revision(),
            )
            .await
            .unwrap();
        assert_eq!(saved.texture_key, first);
        assert_eq!(saved.variant, SkinVariant::Classic);
        assert_eq!(service.library.read_png(&first).unwrap(), png);
        assert_eq!(service.library.get(&second).unwrap(), untouched);
        let marked = service
            .library
            .list_for_account(capture.account_id())
            .unwrap();
        assert!(
            marked
                .iter()
                .any(|skin| skin.texture_key == first && skin.applied_at.is_some())
        );
        assert!(
            marked
                .iter()
                .filter(|skin| skin.texture_key != first)
                .all(|skin| skin.applied_at.is_none())
        );
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn no_account_wardrobe_retains_saved_skins_and_allows_deletion() {
        let (_directory, service, capture, first, _) =
            fixture("http://127.0.0.1:9", profile(None, false)).await;
        let saved = service.library.get(&first).unwrap().unwrap();
        assert!(
            service
                .library
                .mark_applied_if_current(capture.account_id(), &first, saved.revision,)
                .unwrap()
        );
        service.auth.logout().await.unwrap();
        assert!(
            service
                .accounts
                .snapshot()
                .unwrap()
                .active_account_id
                .is_none()
        );
        let public = service.list().unwrap();
        assert_eq!(public.skins.len(), 2);
        assert!(public.skins.iter().all(|skin| skin.applied_at.is_none()));
        assert!(public.pending_apply_texture_key.is_none());
        assert!(matches!(
            service.delete_saved(&first).await.unwrap(),
            SavedSkinDeleteResult::Deleted(record) if record.texture_key == first
        ));
        assert_eq!(service.list().unwrap().skins.len(), 1);
        service.shutdown().await.unwrap();
    }

    fn change_ownership(
        service: &ProfileMedia,
        capture: &CapturedAccount,
        owns_minecraft_java: bool,
    ) -> CapturedAccount {
        service
            .accounts
            .refresh_microsoft(
                capture,
                MicrosoftIdentity {
                    login_id: capture.login_id().unwrap().into(),
                    profile_id: capture.minecraft_uuid().into(),
                    display_name: capture.display_name().into(),
                    credential_revision: capture.credential_revision(),
                    profile: capture.profile().unwrap().clone(),
                    owns_minecraft_java,
                },
            )
            .unwrap()
    }

    #[tokio::test]
    async fn negative_ownership_refuses_new_skin_work_and_fences_a_queued_apply() {
        let (_directory, service, original, first, _) =
            fixture("http://127.0.0.1:9", profile(None, false)).await;
        queue(&service, &first);
        let unowned = change_ownership(&service, &original, false);
        assert!(service.auth.credentials(&unowned).await.is_ok());
        assert!(matches!(
            service.queue(&first, unowned.account_id(), unowned.selection_revision()),
            Err(SkinError::Profile(ProfileMediaError::OwnershipMissing))
        ));
        assert_eq!(
            service
                .reset(true, unowned.account_id(), unowned.selection_revision())
                .await,
            Err(ProfileMediaError::OwnershipMissing)
        );
        assert_eq!(
            service
                .reset(false, unowned.account_id(), unowned.selection_revision())
                .await,
            Err(ProfileMediaError::OwnershipMissing)
        );
        assert!(matches!(
            service.profile_file(None).await,
            Err(ProfileMediaError::OwnershipMissing)
        ));
        assert!(matches!(
            service
                .save_profile(
                    None,
                    None,
                    false,
                    unowned.account_id(),
                    unowned.selection_revision()
                )
                .await,
            Err(SkinError::Profile(ProfileMediaError::OwnershipMissing))
        ));
        assert_eq!(
            service.flush_account(unowned.account_id()).await,
            Err(ProfileMediaError::StaleIdentity)
        );
        assert!(
            service
                .list()
                .unwrap()
                .skins
                .iter()
                .all(|skin| skin.applied_at.is_none())
        );
        assert!(
            !service
                .accounts
                .capture_selected()
                .unwrap()
                .owns_minecraft_java()
        );
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ownership_loss_during_upload_preserves_the_unsettled_effect() {
        let (resume, paused) = oneshot::channel();
        let mut upload = Reply::json(profile(Some(FIRST_URL), true));
        upload.resume = Some(paused);
        let (base, mut requests, server) =
            server(vec![Reply::json(profile(None, true)), upload]).await;
        let (_directory, service, capture, first, _) = fixture(&base, profile(None, true)).await;
        queue(&service, &first);
        let waiter_service = service.clone();
        let account = capture.account_id().to_owned();
        let waiter = tokio::spawn(async move { waiter_service.flush_account(&account).await });
        assert!(requests.recv().await.unwrap().starts_with("GET"));
        assert!(requests.recv().await.unwrap().starts_with("POST"));
        let unowned = change_ownership(&service, &capture, false);
        resume.send(()).unwrap();
        assert_eq!(waiter.await.unwrap(), Err(ProfileMediaError::StaleIdentity));
        assert_eq!(
            service.shutdown().await,
            Err(ProfileMediaError::UncertainChange)
        );
        assert!(
            !service
                .accounts
                .capture_selected()
                .unwrap()
                .owns_minecraft_java()
        );
        assert!(service.auth.credentials(&unowned).await.is_ok());
        server.await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn accepted_apply_commits_the_profile_and_only_its_saved_skin() {
        let (base, mut requests, server) = server(vec![
            Reply::json(profile(None, false)),
            Reply::json(profile(Some(FIRST_URL), false)),
            Reply::json(profile(Some(FIRST_URL), false)),
        ])
        .await;
        let (_directory, service, capture, first, second) =
            fixture(&base, profile(None, false)).await;
        queue(&service, &first);
        assert_eq!(service.flush_pending().await.unwrap(), 1);
        let records = service
            .library
            .list_for_account(capture.account_id())
            .unwrap();
        assert!(
            records
                .iter()
                .find(|record| record.texture_key == first)
                .unwrap()
                .applied_at
                .is_some()
        );
        assert!(
            records
                .iter()
                .find(|record| record.texture_key == second)
                .unwrap()
                .applied_at
                .is_none()
        );
        assert_eq!(service.list().unwrap().skins, records);
        assert_eq!(
            service.profile().unwrap().texture_url.as_deref(),
            Some(FIRST_URL)
        );
        service.shutdown().await.unwrap();
        server.await.unwrap();
        assert!(requests.recv().await.unwrap().starts_with("GET /profile "));
        assert!(requests.recv().await.unwrap().starts_with("POST /skins "));
        assert!(requests.recv().await.unwrap().starts_with("GET /profile "));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn partial_cape_failure_is_reconciled_and_never_reported_or_retried_as_success() {
        let (base, mut requests, server) = server(vec![
            Reply::json(profile(None, true)),
            Reply::json(profile(Some(FIRST_URL), true)),
            Reply {
                status: 503,
                body: Vec::new(),
                resume: None,
            },
            Reply::json(profile(Some(FIRST_URL), true)),
        ])
        .await;
        let (_directory, service, capture, first, _) = fixture(&base, profile(None, true)).await;
        queue(&service, &first);
        assert_eq!(
            service.flush_account(capture.account_id()).await,
            Err(ProfileMediaError::PartialChange)
        );
        assert_eq!(
            service.profile().unwrap().texture_url.as_deref(),
            Some(FIRST_URL)
        );
        assert!(
            service
                .library
                .list_for_account(capture.account_id())
                .unwrap()
                .iter()
                .all(|record| record.applied_at.is_none())
        );
        assert_eq!(
            service.pending.status(capture.account_id()).unwrap().phase,
            "failed"
        );
        // The failed operation stays visible, but its fetched remote profile
        // has settled locally, so it does not strand application shutdown.
        service.shutdown().await.unwrap();
        server.await.unwrap();
        let mut sent = Vec::new();
        while let Ok(request) = requests.try_recv() {
            sent.push(request);
        }
        assert_eq!(
            sent.iter()
                .filter(|request| request.starts_with("POST"))
                .count(),
            1
        );
        assert_eq!(
            sent.iter()
                .filter(|request| request.starts_with("DELETE"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn logout_while_provider_write_finishes_cannot_restore_profile_or_applied_marker() {
        let (resume, paused) = oneshot::channel();
        let mut upload = Reply::json(profile(Some(FIRST_URL), false));
        upload.resume = Some(paused);
        let (base, mut requests, server) =
            server(vec![Reply::json(profile(None, false)), upload]).await;
        let (_directory, service, capture, first, _) = fixture(&base, profile(None, false)).await;
        queue(&service, &first);
        let waiter_service = service.clone();
        let account = capture.account_id().to_owned();
        let waiter = tokio::spawn(async move { waiter_service.flush_account(&account).await });
        assert!(requests.recv().await.unwrap().starts_with("GET"));
        assert!(requests.recv().await.unwrap().starts_with("POST"));
        service.auth.logout().await.unwrap();
        resume.send(()).unwrap();
        assert!(waiter.await.unwrap().is_err());
        assert!(
            service
                .accounts
                .capture_account(capture.account_id())
                .is_err()
        );
        assert!(
            service
                .library
                .list()
                .unwrap()
                .iter()
                .all(|record| record.applied_at.is_none())
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_shutdown_reopens_commands_and_explicit_retry_settles_the_exact_intent() {
        let (base, _, server) = server(vec![
            Reply::json(profile(None, false)),
            Reply {
                status: 200,
                body: b"not a profile".to_vec(),
                resume: None,
            },
            Reply {
                status: 503,
                body: Vec::new(),
                resume: None,
            },
            Reply::json(profile(None, false)),
            Reply::json(profile(Some(FIRST_URL), false)),
            Reply::json(profile(Some(FIRST_URL), false)),
        ])
        .await;
        let (_directory, service, capture, first, _) = fixture(&base, profile(None, false)).await;
        queue(&service, &first);
        assert_eq!(
            service.flush_account(capture.account_id()).await,
            Err(ProfileMediaError::UncertainChange)
        );
        assert_eq!(
            service.shutdown().await,
            Err(ProfileMediaError::UncertainChange)
        );
        assert_eq!(
            service.flush_pending().await,
            Err(ProfileMediaError::UncertainChange)
        );
        // Admission is reopened on refusal, and a new explicit apply can first
        // read the actual provider state and finish settlement.
        queue(&service, &first);
        service.flush_account(capture.account_id()).await.unwrap();
        service.shutdown().await.unwrap();
        assert!(matches!(
            service.delete_saved(&first).await,
            Err(SkinError::Profile(ProfileMediaError::ShuttingDown))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn profile_texture_queries_validate_the_complete_identity_tuple() {
        let (_directory, service, _, _, _) =
            fixture("http://127.0.0.1:1", profile(Some(FIRST_URL), false)).await;
        assert!(
            service
                .profile_file_for_identity(Some(FIRST_URL), Some(PROFILE_ID), Some("skin"))
                .await
                .is_ok()
        );
        assert_eq!(
            service
                .profile_file_for_identity(Some(SECOND_URL), Some(PROFILE_ID), Some("skin"))
                .await,
            Err(ProfileMediaError::StaleIdentity)
        );
        assert_eq!(
            service
                .profile_file_for_identity(
                    Some(FIRST_URL),
                    Some("22345678123442348234123456789abc"),
                    Some("skin")
                )
                .await,
            Err(ProfileMediaError::StaleIdentity)
        );
        assert_eq!(
            service
                .profile_file_for_identity(Some(FIRST_URL), None, Some("skin"))
                .await,
            Err(ProfileMediaError::InvalidTexture)
        );
        service.accounts.create_offline_account("Steve").unwrap();
        assert!(
            service
                .profile_file_for_identity(Some(FIRST_URL), Some(PROFILE_ID), None)
                .await
                .is_ok()
        );
        service.shutdown().await.unwrap();
        assert!(matches!(
            service
                .save_upload(
                    &png(33),
                    SaveSkinOptions {
                        name: "Blocked".into(),
                        ..Default::default()
                    },
                    None
                )
                .await,
            Err(SkinError::Profile(ProfileMediaError::ShuttingDown))
        ));
    }

    #[tokio::test]
    async fn newer_intent_survives_old_profile_completion_and_disconnected_waiter() {
        let (resume, paused) = oneshot::channel();
        let mut upload = Reply::json(profile(Some(FIRST_URL), false));
        upload.resume = Some(paused);
        let (base, mut requests, server) = server(vec![
            Reply::json(profile(None, false)),
            upload,
            Reply::json(profile(Some(FIRST_URL), false)),
            Reply::json(profile(Some(SECOND_URL), false)),
            Reply::json(profile(Some(SECOND_URL), false)),
        ])
        .await;
        let (_directory, service, capture, first, second) =
            fixture(&base, profile(None, false)).await;
        queue(&service, &first);
        let waiter_service = service.clone();
        let waiter = tokio::spawn(async move { waiter_service.flush_pending().await });
        assert!(requests.recv().await.unwrap().starts_with("GET"));
        assert!(requests.recv().await.unwrap().starts_with("POST"));
        waiter.abort();
        queue(&service, &second);
        resume.send(()).unwrap();
        service.flush_account(capture.account_id()).await.unwrap();
        let records = service
            .library
            .list_for_account(capture.account_id())
            .unwrap();
        assert!(
            records
                .iter()
                .find(|record| record.texture_key == first)
                .unwrap()
                .applied_at
                .is_none()
        );
        assert!(
            records
                .iter()
                .find(|record| record.texture_key == second)
                .unwrap()
                .applied_at
                .is_some()
        );
        assert_eq!(
            service.profile().unwrap().texture_url.as_deref(),
            Some(SECOND_URL)
        );
        service.shutdown().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_profile_revision_change_during_upload_prevents_the_obsolete_cape_write() {
        let (resume, paused) = oneshot::channel();
        let mut upload = Reply::json(profile(Some(FIRST_URL), true));
        upload.resume = Some(paused);
        let (base, mut requests, server) =
            server(vec![Reply::json(profile(None, true)), upload]).await;
        let (_directory, service, capture, first, _) = fixture(&base, profile(None, true)).await;
        queue(&service, &first);
        let waiter_service = service.clone();
        let account = capture.account_id().to_owned();
        let waiter = tokio::spawn(async move { waiter_service.flush_account(&account).await });
        assert!(requests.recv().await.unwrap().starts_with("GET"));
        assert!(requests.recv().await.unwrap().starts_with("POST"));
        let mut updated = profile(None, true);
        updated.name = "NewPlayer".into();
        service
            .auth
            .commit_profile(&capture, updated)
            .await
            .unwrap();
        resume.send(()).unwrap();
        assert_eq!(waiter.await.unwrap(), Err(ProfileMediaError::StaleIdentity));
        assert_eq!(service.profile().unwrap().username, "NewPlayer");
        server.await.unwrap();
        assert!(requests.try_recv().is_err());
    }
}
