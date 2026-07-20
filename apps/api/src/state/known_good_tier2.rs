use super::{
    AppState, IdleSweepAuthority, InstanceLifecycleLease, KnownGoodVerificationLease,
    KnownGoodVerificationOwner, KnownGoodVerificationUnavailable, RegisteredArtifactFindings,
    RegisteredArtifactObservation,
};
use crate::execution::integrity::{Tier2CleanSealRequest, Tier2RegisteredArtifactSealRequest};
use axial_config::is_canonical_instance_id;
use axial_minecraft::{ManagedRuntimeCache, known_good::KnownGoodInventory};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

/// Move-only snapshot of exact live known-good authority for a Tier 2 sweep.
pub(crate) struct KnownGoodTier2Ticket {
    sweep_authority: IdleSweepAuthority,
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    managed_runtime_cache: ManagedRuntimeCache,
    inventory: Arc<KnownGoodInventory>,
    managed_artifact_epoch: super::ManagedArtifactMutationEpoch,
}

/// Exact clean authority retained across durable Tier 2 terminalization.
pub(crate) struct KnownGoodTier2CleanSeal {
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    managed_runtime_cache: ManagedRuntimeCache,
    inventory: Arc<KnownGoodInventory>,
    managed_artifact_epoch: super::ManagedArtifactMutationEpoch,
}

pub(crate) enum KnownGoodTier2CleanClassification {
    Clean(KnownGoodTier2CleanSeal),
    NonClean(KnownGoodTier2Ticket),
}

/// Process-local proof that one exact live instance identity was clean.
pub(crate) struct KnownGoodTier2CleanReceipt {
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    managed_runtime_cache: ManagedRuntimeCache,
    inventory: Weak<KnownGoodInventory>,
    managed_artifact_epoch: super::ManagedArtifactMutationEpoch,
    verified_at: tokio::time::Instant,
}

struct KnownGoodTier2ExactIdentity<'a> {
    instance_id: &'a str,
    version_id: &'a str,
    created_at: &'a str,
    library_root: &'a Path,
    managed_runtime_cache: &'a ManagedRuntimeCache,
    inventory: &'a Arc<KnownGoodInventory>,
    managed_artifact_epoch: super::ManagedArtifactMutationEpoch,
}

impl KnownGoodTier2CleanReceipt {
    pub(crate) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(crate) fn verified_at(&self) -> tokio::time::Instant {
        self.verified_at
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

impl KnownGoodTier2Ticket {
    pub(crate) fn matches_settlement(&self, settlement: &super::IdleSweepSettlementOwner) -> bool {
        settlement.matches_authority(&self.sweep_authority)
    }

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
        sweep_authority: &IdleSweepAuthority,
        instance_id: &str,
    ) -> Result<KnownGoodTier2Ticket, KnownGoodVerificationUnavailable> {
        if !is_canonical_instance_id(instance_id) {
            return Err(KnownGoodVerificationUnavailable::InstanceNotRegistered);
        }
        if !self.idle_sweep_authority_is_current(sweep_authority) {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let lifecycle = InstanceLifecycleLease::bind(
            instance_id,
            self.instance_lifecycle_gates.clone(),
            self.instance_lifecycle_gates.acquire(instance_id).await,
        );
        if !self.idle_sweep_authority_is_current(sweep_authority) {
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
        let managed_artifact_epoch = self
            .capture_managed_artifact_mutation_epoch()
            .map_err(|_| KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?;
        let ticket = KnownGoodTier2Ticket {
            sweep_authority: sweep_authority.clone(),
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root,
            managed_runtime_cache: self.managed_runtime_cache.clone(),
            inventory,
            managed_artifact_epoch,
        };
        if !self.idle_sweep_authority_is_current(sweep_authority) {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        drop(lifecycle);
        Ok(ticket)
    }

    pub(crate) fn known_good_tier2_ticket_is_current(&self, ticket: &KnownGoodTier2Ticket) -> bool {
        self.known_good_tier2_ticket_identity_is_current(ticket)
            && self.idle_sweep_authority_is_current(&ticket.sweep_authority)
    }

    pub(crate) async fn seal_known_good_tier2_clean_request(
        &self,
        request: Tier2CleanSealRequest,
    ) -> Result<KnownGoodTier2CleanClassification, KnownGoodVerificationUnavailable> {
        self.seal_known_good_tier2_clean_request_with_observer(request, || {})
            .await
    }

    async fn seal_known_good_tier2_clean_request_with_observer<BeforeSeal>(
        &self,
        request: Tier2CleanSealRequest,
        before_seal: BeforeSeal,
    ) -> Result<KnownGoodTier2CleanClassification, KnownGoodVerificationUnavailable>
    where
        BeforeSeal: FnOnce(),
    {
        if !self.known_good_tier2_ticket_is_current(request.ticket()) {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let lifecycle = InstanceLifecycleLease::bind(
            &request.ticket().instance_id,
            self.instance_lifecycle_gates.clone(),
            self.instance_lifecycle_gates
                .acquire(&request.ticket().instance_id)
                .await,
        );
        before_seal();
        if !self.known_good_tier2_ticket_is_current(request.ticket()) {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let exact_clean = request.is_exact_clean();
        let ticket = request.into_ticket();
        if !exact_clean {
            drop(lifecycle);
            return Ok(KnownGoodTier2CleanClassification::NonClean(ticket));
        }
        let KnownGoodTier2Ticket {
            sweep_authority: _,
            instance_id,
            version_id,
            created_at,
            library_root,
            managed_runtime_cache,
            inventory,
            managed_artifact_epoch,
        } = ticket;
        drop(lifecycle);
        Ok(KnownGoodTier2CleanClassification::Clean(
            KnownGoodTier2CleanSeal {
                instance_id,
                version_id,
                created_at,
                library_root,
                managed_runtime_cache,
                inventory,
                managed_artifact_epoch,
            },
        ))
    }

    pub(crate) fn accept_known_good_tier2_clean_seal(
        &self,
        seal: KnownGoodTier2CleanSeal,
        verified_at: tokio::time::Instant,
    ) -> Option<KnownGoodTier2CleanReceipt> {
        if !self.known_good_tier2_exact_identity_is_current(KnownGoodTier2ExactIdentity {
            instance_id: &seal.instance_id,
            version_id: &seal.version_id,
            created_at: &seal.created_at,
            library_root: &seal.library_root,
            managed_runtime_cache: &seal.managed_runtime_cache,
            inventory: &seal.inventory,
            managed_artifact_epoch: seal.managed_artifact_epoch,
        }) {
            return None;
        }
        let inventory = Arc::downgrade(&seal.inventory);
        Some(KnownGoodTier2CleanReceipt {
            instance_id: seal.instance_id,
            version_id: seal.version_id,
            created_at: seal.created_at,
            library_root: seal.library_root,
            managed_runtime_cache: seal.managed_runtime_cache,
            inventory,
            managed_artifact_epoch: seal.managed_artifact_epoch,
            verified_at,
        })
    }

    pub(crate) fn known_good_tier2_clean_receipt_is_current(
        &self,
        receipt: &KnownGoodTier2CleanReceipt,
    ) -> bool {
        let Some(inventory) = receipt.inventory.upgrade() else {
            return false;
        };
        self.known_good_tier2_exact_identity_is_current(KnownGoodTier2ExactIdentity {
            instance_id: &receipt.instance_id,
            version_id: &receipt.version_id,
            created_at: &receipt.created_at,
            library_root: &receipt.library_root,
            managed_runtime_cache: &receipt.managed_runtime_cache,
            inventory: &inventory,
            managed_artifact_epoch: receipt.managed_artifact_epoch,
        })
    }

    pub(crate) async fn seal_tier2_registered_artifact_request(
        &self,
        request: Tier2RegisteredArtifactSealRequest,
    ) -> Result<RegisteredArtifactFindings, KnownGoodVerificationUnavailable> {
        let (ticket, observations) = request.into_parts();
        self.seal_tier2_registered_artifact_findings_with_observer(ticket, observations, || {})
            .await
    }

    async fn seal_tier2_registered_artifact_findings_with_observer<BeforeSeal>(
        &self,
        ticket: KnownGoodTier2Ticket,
        observations: Vec<RegisteredArtifactObservation>,
        before_seal: BeforeSeal,
    ) -> Result<RegisteredArtifactFindings, KnownGoodVerificationUnavailable>
    where
        BeforeSeal: FnOnce(),
    {
        if !self.known_good_tier2_ticket_is_current(&ticket) {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let lifecycle = InstanceLifecycleLease::bind(
            &ticket.instance_id,
            self.instance_lifecycle_gates.clone(),
            self.instance_lifecycle_gates
                .acquire(&ticket.instance_id)
                .await,
        );
        if !self.known_good_tier2_ticket_is_current(&ticket) {
            return Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable);
        }
        let KnownGoodTier2Ticket {
            sweep_authority,
            instance_id,
            version_id,
            created_at,
            library_root,
            managed_runtime_cache,
            inventory,
            managed_artifact_epoch,
        } = ticket;
        let audit_authority = sweep_authority.clone();
        let authority = KnownGoodVerificationLease {
            owner: KnownGoodVerificationOwner::IdleSweep(sweep_authority),
            _lifecycle: lifecycle,
            instance_id,
            version_id,
            created_at,
            library_root,
            managed_runtime_cache,
            inventory,
            managed_artifact_epoch: Some(Arc::new(std::sync::atomic::AtomicU64::new(
                managed_artifact_epoch.value(),
            ))),
        };
        before_seal();
        match self.seal_registered_artifact_findings(authority, observations) {
            Err(_) if !self.idle_sweep_authority_is_current(&audit_authority) => {
                Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable)
            }
            result => result,
        }
    }

    fn known_good_tier2_ticket_identity_is_current(&self, ticket: &KnownGoodTier2Ticket) -> bool {
        self.known_good_tier2_exact_identity_is_current(KnownGoodTier2ExactIdentity {
            instance_id: &ticket.instance_id,
            version_id: &ticket.version_id,
            created_at: &ticket.created_at,
            library_root: &ticket.library_root,
            managed_runtime_cache: &ticket.managed_runtime_cache,
            inventory: &ticket.inventory,
            managed_artifact_epoch: ticket.managed_artifact_epoch,
        })
    }

    fn known_good_tier2_exact_identity_is_current(
        &self,
        identity: KnownGoodTier2ExactIdentity<'_>,
    ) -> bool {
        let KnownGoodTier2ExactIdentity {
            instance_id,
            version_id,
            created_at,
            library_root,
            managed_runtime_cache,
            inventory,
            managed_artifact_epoch,
        } = identity;
        self.managed_artifact_mutation_epoch()
            .is_ok_and(|epoch| epoch == managed_artifact_epoch)
            && self
                .managed_runtime_cache
                .shares_identity_with(managed_runtime_cache)
            && self.known_good_authority_is_current(
                instance_id,
                version_id,
                created_at,
                library_root,
                managed_runtime_cache,
                inventory,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, IdleSweepReservation, InstallStore, SessionStore};
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
        let _ = <KnownGoodTier2CleanSeal as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <KnownGoodTier2CleanReceipt as AmbiguousIfClone<_>>::assert_not_clone;
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
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
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
    async fn shared_root_mutation_invalidates_an_existing_ticket() {
        let fixture = Fixture::new("shared-root-mutation");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let shared_root_writer = fixture.state.clone();

        let _mutation = shared_root_writer
            .admit_managed_artifact_mutation()
            .expect("shared-root mutation");

        assert!(!fixture.state.known_good_tier2_ticket_is_current(&ticket));
        drop(ticket);
        reservation.settle(super::super::IdleSweepTerminal::Cancelled);
        fixture.close().await;
    }

    #[tokio::test]
    async fn sealed_sweep_findings_distinguish_admission_from_live_effect_authority() {
        let fixture = Fixture::new("sealed-findings-authority");
        let inventory = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: TestKnownGoodRoot::Assets,
            path: "indexes/repairable.json".to_string(),
            kind: KnownGoodArtifactKind::AssetIndex,
            integrity: TestKnownGoodIntegrity::Sha1 {
                digest: "0000000000000000000000000000000000000000".to_string(),
                size: 1,
            },
        }])
        .expect("repairable Assets inventory")
        .with_test_standalone_leaf_repair_source(
            0,
            "https://example.invalid/indexes/repairable.json",
        )
        .expect("repairable Assets source");
        fixture
            .state
            .activate_known_good_inventory_for_test(&fixture.instance.id, inventory);
        let reservation = fixture.reserve_sweep();
        let cancellation = reservation.cancellation();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let findings = fixture
            .state
            .seal_tier2_registered_artifact_findings_with_observer(
                ticket,
                vec![RegisteredArtifactObservation::new(
                    0,
                    super::super::RegisteredArtifactCondition::Missing,
                )],
                || {},
            )
            .await
            .expect("sealed sweep findings");

        assert!(
            fixture
                .state
                .registered_artifact_findings_can_admit(&findings)
        );
        assert!(
            fixture
                .state
                .registered_artifact_findings_are_live(&findings)
        );
        cancellation.cancel();
        assert!(
            !fixture
                .state
                .registered_artifact_findings_can_admit(&findings)
        );
        assert!(
            fixture
                .state
                .registered_artifact_findings_are_live(&findings)
        );
        reservation.settle(super::super::IdleSweepTerminal::Cancelled);
        assert!(
            !fixture
                .state
                .registered_artifact_findings_are_live(&findings)
        );

        drop(findings);
        let lifecycle = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fixture
                .state
                .acquire_instance_lifecycle(&fixture.instance.id),
        )
        .await
        .expect("findings release lifecycle");
        drop(lifecycle);
        fixture.close().await;
    }

    #[tokio::test]
    async fn cancellation_during_sweep_finding_seal_is_classified_as_sweep_supersession() {
        let fixture = Fixture::new("cancel-during-seal");
        let inventory = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: TestKnownGoodRoot::Assets,
            path: "indexes/cancelled.json".to_string(),
            kind: KnownGoodArtifactKind::AssetIndex,
            integrity: TestKnownGoodIntegrity::Sha1 {
                digest: "0000000000000000000000000000000000000000".to_string(),
                size: 1,
            },
        }])
        .expect("cancelled Assets inventory")
        .with_test_standalone_leaf_repair_source(
            0,
            "https://example.invalid/indexes/cancelled.json",
        )
        .expect("cancelled Assets source");
        fixture
            .state
            .activate_known_good_inventory_for_test(&fixture.instance.id, inventory);
        let reservation = fixture.reserve_sweep();
        let cancellation = reservation.cancellation();
        let cancel_during_seal = cancellation.clone();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");

        let result = fixture
            .state
            .seal_tier2_registered_artifact_findings_with_observer(
                ticket,
                vec![RegisteredArtifactObservation::new(
                    0,
                    super::super::RegisteredArtifactCondition::Missing,
                )],
                move || cancel_during_seal.cancel(),
            )
            .await;

        assert!(matches!(
            result,
            Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable)
        ));
        assert!(cancellation.is_cancelled());
        reservation.settle(super::super::IdleSweepTerminal::Cancelled);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_revalidation_rejects_replaced_inventory() {
        let fixture = Fixture::new("inventory-drift");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
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
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
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
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
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
    async fn cancelled_ticket_is_active_for_settlement_but_not_current_for_admission() {
        let fixture = Fixture::new("cancelled-authority");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");

        reservation.cancellation().cancel();

        assert!(!fixture.state.known_good_tier2_ticket_is_current(&ticket));
        assert!(
            fixture
                .state
                .idle_sweep_authority_is_active(&ticket.sweep_authority)
        );
        reservation.settle(crate::state::IdleSweepTerminal::Cancelled);
        assert!(
            !fixture
                .state
                .idle_sweep_authority_is_active(&ticket.sweep_authority)
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_mint_accepts_only_a_canonical_registered_identity() {
        let fixture = Fixture::new("identity-only-api");
        let reservation = fixture.reserve_sweep();
        assert_eq!(
            fixture
                .state
                .mint_known_good_tier2_ticket(&reservation.authority(), "../caller/path")
                .await
                .err(),
            Some(KnownGoodVerificationUnavailable::InstanceNotRegistered)
        );
        assert_eq!(
            fixture
                .state
                .mint_known_good_tier2_ticket(&reservation.authority(), "missing-instance")
                .await
                .err(),
            Some(KnownGoodVerificationUnavailable::InstanceNotRegistered)
        );
        drop(reservation);
        fixture.close().await;
    }

    #[tokio::test]
    async fn ticket_mint_rejects_foreign_and_stale_sweep_authority() {
        let fixture = Fixture::new("authority-owner");
        let foreign = Fixture::new("authority-foreign");
        let foreign_reservation = foreign.reserve_sweep();

        assert_eq!(
            fixture
                .state
                .mint_known_good_tier2_ticket(
                    &foreign_reservation.authority(),
                    &fixture.instance.id,
                )
                .await
                .err(),
            Some(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable)
        );

        let reservation = fixture.reserve_sweep();
        let stale_authority = reservation.authority();
        reservation.settle(crate::state::IdleSweepTerminal::Cancelled);
        assert_eq!(
            fixture
                .state
                .mint_known_good_tier2_ticket(&stale_authority, &fixture.instance.id)
                .await
                .err(),
            Some(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable)
        );

        drop(foreign_reservation);
        foreign.close().await;
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
                .mint_known_good_tier2_ticket(&reservation.authority(), &instance_id)
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

    #[tokio::test]
    async fn exact_clean_seal_mints_a_process_local_exact_receipt() {
        let fixture = Fixture::new("clean-receipt");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let classification = fixture
            .state
            .seal_known_good_tier2_clean_request(Tier2CleanSealRequest::exact_clean_for_test(
                ticket,
            ))
            .await
            .expect("clean seal");
        let KnownGoodTier2CleanClassification::Clean(seal) = classification else {
            panic!("exact clean report must seal clean authority")
        };
        reservation.settle(super::super::IdleSweepTerminal::Complete);
        let verified_at = tokio::time::Instant::now();
        let receipt = fixture
            .state
            .accept_known_good_tier2_clean_seal(seal, verified_at)
            .expect("exact receipt");

        assert_eq!(receipt.verified_at(), verified_at);
        assert_eq!(
            receipt.exact_identity_for_test(),
            (
                fixture.instance.id.as_str(),
                fixture.instance.version_id.as_str(),
                fixture.instance.created_at.as_str(),
                fixture.library_root.as_path(),
            )
        );
        assert!(
            fixture
                .state
                .known_good_tier2_clean_receipt_is_current(&receipt)
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn dirty_report_is_returned_for_findings_sealing_without_clean_authority() {
        let fixture = Fixture::new("dirty-classification");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let classification = fixture
            .state
            .seal_known_good_tier2_clean_request(
                Tier2CleanSealRequest::exact_clean_for_test(ticket).with_fact_for_test(),
            )
            .await
            .expect("dirty classification");
        let KnownGoodTier2CleanClassification::NonClean(ticket) = classification else {
            panic!("dirty report must not produce clean authority")
        };
        assert!(fixture.state.known_good_tier2_ticket_is_current(&ticket));
        drop(ticket);
        reservation.settle(super::super::IdleSweepTerminal::Cancelled);
        fixture.close().await;
    }

    #[tokio::test]
    async fn managed_artifact_epoch_race_before_clean_seal_is_rejected() {
        let fixture = Fixture::new("clean-before-seal-race");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let mutating_state = fixture.state.clone();
        let result = fixture
            .state
            .seal_known_good_tier2_clean_request_with_observer(
                Tier2CleanSealRequest::exact_clean_for_test(ticket),
                move || {
                    drop(
                        mutating_state
                            .admit_managed_artifact_mutation()
                            .expect("racing managed mutation"),
                    );
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable)
        ));
        reservation.settle(super::super::IdleSweepTerminal::Cancelled);
        fixture.close().await;
    }

    #[tokio::test]
    async fn mutation_after_clean_seal_prevents_final_receipt_acceptance() {
        let fixture = Fixture::new("clean-after-seal-race");
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let classification = fixture
            .state
            .seal_known_good_tier2_clean_request(Tier2CleanSealRequest::exact_clean_for_test(
                ticket,
            ))
            .await
            .expect("clean seal");
        let KnownGoodTier2CleanClassification::Clean(seal) = classification else {
            panic!("exact clean report must seal clean authority")
        };
        reservation.settle(super::super::IdleSweepTerminal::Complete);
        drop(
            fixture
                .state
                .admit_managed_artifact_mutation()
                .expect("post-seal managed mutation"),
        );

        assert!(
            fixture
                .state
                .accept_known_good_tier2_clean_seal(seal, tokio::time::Instant::now())
                .is_none()
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn clean_receipt_does_not_retain_a_replaced_inventory() {
        let fixture = Fixture::new("receipt-weak-inventory");
        let retained_inventory = fixture
            .state
            .activate_known_good_inventory_for_test_with_identity(
                &fixture.instance.id,
                inventory("retained.jar"),
            );
        let inventory_identity = Arc::downgrade(&retained_inventory);
        let reservation = fixture.reserve_sweep();
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance.id)
            .await
            .expect("ticket");
        let classification = fixture
            .state
            .seal_known_good_tier2_clean_request(Tier2CleanSealRequest::exact_clean_for_test(
                ticket,
            ))
            .await
            .expect("clean seal");
        let KnownGoodTier2CleanClassification::Clean(seal) = classification else {
            panic!("exact clean report must seal clean authority")
        };
        reservation.settle(super::super::IdleSweepTerminal::Complete);
        let receipt = fixture
            .state
            .accept_known_good_tier2_clean_seal(seal, tokio::time::Instant::now())
            .expect("clean receipt");
        drop(retained_inventory);

        fixture.state.activate_known_good_inventory_for_test(
            &fixture.instance.id,
            inventory("replacement.jar"),
        );

        assert!(inventory_identity.upgrade().is_none());
        assert!(receipt.inventory.upgrade().is_none());
        assert!(
            !fixture
                .state
                .known_good_tier2_clean_receipt_is_current(&receipt)
        );
        fixture.close().await;
    }
}
