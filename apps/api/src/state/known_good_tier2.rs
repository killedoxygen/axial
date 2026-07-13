use super::{
    AppState, IdleSweepReservation, InstanceLifecycleLease, KnownGoodVerificationUnavailable,
};
use axial_config::is_canonical_instance_id;
use axial_minecraft::{ManagedRuntimeCache, known_good::KnownGoodInventory};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Move-only snapshot of exact live known-good authority for a Tier 2 sweep.
pub(crate) struct KnownGoodTier2Ticket {
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    managed_runtime_cache: ManagedRuntimeCache,
    inventory: Arc<KnownGoodInventory>,
}

impl KnownGoodTier2Ticket {
    pub(crate) fn execution_parts(&self) -> (&Path, &ManagedRuntimeCache, &KnownGoodInventory) {
        (
            &self.library_root,
            &self.managed_runtime_cache,
            &self.inventory,
        )
    }

    #[cfg(test)]
    fn exact_identity_for_test(&self) -> (&str, &str, &str, &Path) {
        (
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
        )
    }
}

impl AppState {
    pub(crate) async fn mint_known_good_tier2_ticket(
        &self,
        reservation: &IdleSweepReservation,
        instance_id: &str,
    ) -> Result<KnownGoodTier2Ticket, KnownGoodVerificationUnavailable> {
        if !is_canonical_instance_id(instance_id) {
            return Err(KnownGoodVerificationUnavailable::InstanceNotRegistered);
        }
        if !reservation.is_current() {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let lifecycle = InstanceLifecycleLease::bind(
            instance_id,
            self.instance_lifecycle_gates.acquire(instance_id).await,
        );
        if !reservation.is_current() {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let instance = self
            .instances
            .get(instance_id)
            .filter(|instance| instance.id == instance_id)
            .ok_or(KnownGoodVerificationUnavailable::InstanceNotRegistered)?;
        let library_root = self
            .library_dir()
            .map(PathBuf::from)
            .and_then(|root| super::known_good::normalize_library_root(&root).ok())
            .ok_or(KnownGoodVerificationUnavailable::LibraryRootUnavailable)?;
        let inventory = self
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .ok_or(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?;
        let ticket = KnownGoodTier2Ticket {
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root,
            managed_runtime_cache: self.managed_runtime_cache.clone(),
            inventory,
        };
        if !reservation.is_current() {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        drop(lifecycle);
        Ok(ticket)
    }

    pub(crate) fn known_good_tier2_ticket_is_current(&self, ticket: &KnownGoodTier2Ticket) -> bool {
        self.known_good_authority_is_current(
            &ticket.instance_id,
            &ticket.version_id,
            &ticket.created_at,
            &ticket.library_root,
            &ticket.managed_runtime_cache,
            &ticket.inventory,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, TestKnownGoodEntry, TestKnownGoodIntegrity, TestKnownGoodRoot,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    trait AmbiguousIfClone<Marker> {
        fn assert_not_clone() {}
    }

    struct CloneMarker;

    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<CloneMarker> for T {}

    const _: fn() = || {
        let _ = <KnownGoodTier2Ticket as AmbiguousIfClone<_>>::assert_not_clone;
    };

    struct Fixture {
        root: PathBuf,
        state: AppState,
        instance: axial_config::Instance,
        library_root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "axial-tier2-ticket-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            let config_dir = root.join("config");
            let library_root = root.join("library");
            std::fs::create_dir_all(&library_root).expect("library root");
            let paths = AppPaths {
                config_file: config_dir.join("config.json"),
                instances_file: config_dir.join("instances.json"),
                instances_dir: root.join("instances"),
                music_dir: root.join("music"),
                library_dir: library_root.clone(),
                config_dir,
            };
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("config"));
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance"),
                ),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });
            state.set_library_dir_for_test(library_root.to_string_lossy().into_owned());
            let instance = state
                .instances()
                .insert_for_test("Tier 2 ticket", "1.21.5")
                .expect("instance");
            state.activate_known_good_inventory_for_test(&instance.id, inventory("client.jar"));
            Self {
                root,
                state,
                instance,
                library_root,
            }
        }

        async fn close(self) {
            self.state
                .close_known_good_inventories()
                .await
                .expect("close known-good store");
            drop(self.state);
            let _ = std::fs::remove_dir_all(self.root);
        }

        fn reserve_sweep(&self) -> IdleSweepReservation {
            let epoch = self.state.subscribe_integrity_idle().borrow().epoch();
            let producer = self
                .state
                .try_claim_producer()
                .expect("claim idle sweep producer");
            self.state
                .try_reserve_idle_sweep(epoch, producer)
                .expect("idle sweep reservation")
        }
    }

    fn inventory(path: &str) -> KnownGoodInventory {
        KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: TestKnownGoodRoot::Versions,
            path: path.to_string(),
            kind: KnownGoodArtifactKind::ClientJar,
            integrity: TestKnownGoodIntegrity::File { size: 1 },
        }])
        .expect("inventory")
    }

    #[tokio::test]
    async fn ticket_snapshots_exact_identity_without_retaining_instance_lifecycle() {
        let fixture = Fixture::new("identity");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation, &fixture.instance.id)
            .await
            .expect("ticket");
        let (instance_id, version_id, created_at, library_root) = ticket.exact_identity_for_test();
        let (_, _, inventory) = ticket.execution_parts();
        assert_eq!(instance_id, fixture.instance.id);
        assert_eq!(version_id, fixture.instance.version_id);
        assert_eq!(created_at, fixture.instance.created_at);
        assert_eq!(library_root, fixture.library_root);
        assert_eq!(inventory.entries().len(), 1);
        let lifecycle = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fixture
                .state
                .acquire_instance_lifecycle(&fixture.instance.id),
        )
        .await
        .expect("ticket must not retain instance lifecycle");
        drop(lifecycle);
        assert!(fixture.state.known_good_tier2_ticket_is_current(&ticket));
        drop(reservation);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_revalidation_rejects_replaced_inventory() {
        let fixture = Fixture::new("inventory-drift");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation, &fixture.instance.id)
            .await
            .expect("ticket");
        fixture
            .state
            .activate_known_good_inventory_for_test(&fixture.instance.id, inventory("other.jar"));
        assert!(!fixture.state.known_good_tier2_ticket_is_current(&ticket));
        drop(reservation);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_revalidation_rejects_replaced_incarnation() {
        let fixture = Fixture::new("incarnation-drift");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation, &fixture.instance.id)
            .await
            .expect("ticket");
        let mut replacement = fixture.instance.clone();
        replacement.version_id = "1.21.6".to_string();
        fixture
            .state
            .instances()
            .replace_for_test(replacement)
            .expect("replace instance");
        assert!(!fixture.state.known_good_tier2_ticket_is_current(&ticket));
        drop(reservation);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_revalidation_rejects_library_root_drift() {
        let fixture = Fixture::new("root-drift");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation, &fixture.instance.id)
            .await
            .expect("ticket");
        let changed_root = fixture.root.join("changed-library");
        std::fs::create_dir_all(&changed_root).expect("changed library root");
        fixture
            .state
            .set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        assert!(!fixture.state.known_good_tier2_ticket_is_current(&ticket));
        drop(reservation);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_mint_accepts_only_a_canonical_registered_identity() {
        let fixture = Fixture::new("identity-only-api");
        let reservation = fixture.reserve_sweep();
        assert_eq!(
            fixture
                .state
                .mint_known_good_tier2_ticket(&reservation, "../caller/path")
                .await
                .err(),
            Some(KnownGoodVerificationUnavailable::InstanceNotRegistered)
        );
        assert_eq!(
            fixture
                .state
                .mint_known_good_tier2_ticket(&reservation, "missing-instance")
                .await
                .err(),
            Some(KnownGoodVerificationUnavailable::InstanceNotRegistered)
        );
        drop(reservation);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_mint_revalidates_sweep_authority_after_lifecycle_wait() {
        let fixture = Fixture::new("sweep-cancelled-during-lifecycle-wait");
        let lifecycle = fixture
            .state
            .acquire_instance_lifecycle(&fixture.instance.id)
            .await;
        let reservation = fixture.reserve_sweep();
        let cancellation = reservation.cancellation();
        let state = fixture.state.clone();
        let instance_id = fixture.instance.id.clone();
        let mint = tokio::spawn(async move {
            let result = state
                .mint_known_good_tier2_ticket(&reservation, &instance_id)
                .await;
            (reservation, result)
        });
        tokio::task::yield_now().await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("foreground registration");
        assert!(cancellation.is_cancelled());

        drop(lifecycle);
        let (reservation, result) =
            tokio::time::timeout(std::time::Duration::from_millis(100), mint)
                .await
                .expect("ticket mint completion")
                .expect("ticket mint owner");
        assert_eq!(
            result.err(),
            Some(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable)
        );
        drop(reservation);
        drop(foreground);
        fixture.close().await;
    }
}
