//! A retained settlement outlives its lane cleanup as the install record's witness. Its
//! owner classifies it after any prior transaction settles and acknowledges it once the
//! record no longer depends on it.

use super::*;
use crate::known_good::MAX_KNOWN_GOOD_VERSION_JSON_BYTES;
use crate::managed::component::table::{ManagedComponentArtifactKind, version_bundle_version_id};
use crate::managed::publication::valid_publication_nonce;
use std::hash::Hasher;

const ROOT_BINDING_DOMAIN: &[u8] = b"axial.managed_install_publication.root.v1";
const FINGERPRINT_DOMAIN: &[u8] = b"axial.managed_install_publication.evidence.v1";

/// A retained settlement as its owner observed it; acknowledgement must find it unchanged.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ComponentSettlementEvidence {
    settlement: ComponentSettlementRecord,
    identity: ManagedFileIdentity,
    version_id: String,
    root_binding: String,
    fingerprint: String,
}

pub(crate) enum ComponentSettlementClassification {
    NoEffect(ManagedRootPublicationLease),
    Mismatch(ManagedRootPublicationLease),
    Settled {
        lease: ManagedRootPublicationLease,
        evidence: ComponentSettlementEvidence,
    },
    Indeterminate(ManagedRootPublicationLease),
}

impl ComponentSettlementEvidence {
    pub(crate) fn version_id(&self) -> &str {
        &self.version_id
    }

    pub(crate) fn transaction_nonce(&self) -> String {
        hex::encode(self.settlement.intent.transaction_nonce)
    }

    pub(crate) fn settlement_generation(&self) -> String {
        hex::encode(self.settlement.generation)
    }

    pub(crate) fn root_binding(&self) -> &str {
        &self.root_binding
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn committed_activation_contract(&self) -> Option<[u8; 32]> {
        (self.settlement.outcome.terminal == ComponentTerminalOutcome::Committed)
            .then_some(self.settlement.intent.activation_contract)
            .flatten()
    }

    pub(crate) fn rollback_effect(&self) -> Option<ComponentRollbackEffect> {
        (self.settlement.outcome.terminal == ComponentTerminalOutcome::RolledBack)
            .then_some(self.settlement.outcome.effect)
    }
}

/// Whether `component`'s lane holds a settlement awaiting acknowledgement.
pub(crate) fn has_retained_settlement(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<bool, ComponentEffectsError> {
    lease.revalidate()?;
    let publication = lease.publication_directory();
    let lane_name = component_lane_name(component);
    let retained = publication.has_portably_exact_child_name(lane_name)?
        && publication
            .open_child(lane_name)?
            .has_portably_exact_child_name(COMPONENT_SETTLEMENT_FILE)?;
    lease.revalidate()?;
    Ok(retained)
}

/// Binds evidence to the managed root's filesystem identity and one settlement write.
pub(crate) fn settlement_root_binding(
    root: &ManagedDir,
    transaction_nonce: &str,
    settlement_generation: &str,
) -> Option<String> {
    if !valid_publication_nonce(transaction_nonce)
        || !valid_publication_nonce(settlement_generation)
    {
        return None;
    }
    root.settle().ok()?;
    let root_identity = root.identity().ok()?;
    let mut digest = EvidenceHasher::new(ROOT_BINDING_DOMAIN);
    root_identity.hash_filesystem_binding(&mut digest);
    digest.write(transaction_nonce.as_bytes());
    digest.write(settlement_generation.as_bytes());
    root.revalidate().ok()?;
    Some(digest.finish_hex())
}

/// Classifies `component`'s retained settlement. Run it after any prior transaction settled.
pub(crate) async fn classify_retained_settlement(
    lease: ManagedRootPublicationLease,
    component: ManagedComponentKind,
    is_candidate: impl Fn(&str) -> bool + Send + 'static,
) -> ComponentSettlementClassification {
    enum Classified {
        NoEffect,
        Mismatch,
        Settled(ComponentSettlementEvidence),
    }
    let (lease, classified) = run_retaining_lease(lease, move |lease| {
        lease.root().settle()?;
        lease.revalidate()?;
        let Some((lane, settlement, guard)) = open_retained_settlement(lease, component)? else {
            lease.revalidate()?;
            return Ok(Classified::NoEffect);
        };
        let version_id =
            version_bundle_version_id(&settlement.rows).ok_or(ComponentEffectsError::Topology)?;
        if !is_candidate(version_id) {
            lease.revalidate()?;
            return Ok(Classified::Mismatch);
        }
        retained_evidence(lease, &lane, settlement, &guard).map(Classified::Settled)
    })
    .await;
    match classified {
        Some(Classified::NoEffect) => ComponentSettlementClassification::NoEffect(lease),
        Some(Classified::Mismatch) => ComponentSettlementClassification::Mismatch(lease),
        Some(Classified::Settled(evidence)) => {
            ComponentSettlementClassification::Settled { lease, evidence }
        }
        None => ComponentSettlementClassification::Indeterminate(lease),
    }
}

/// Removes the retained settlement `evidence` names once its terminal shape still holds.
/// A settlement already gone is acknowledged when its lane and canonical files still match.
pub(crate) async fn acknowledge_retained_settlement(
    lease: ManagedRootPublicationLease,
    evidence: ComponentSettlementEvidence,
) -> Result<ManagedRootPublicationLease, (ManagedRootPublicationLease, ComponentSettlementEvidence)>
{
    let retained = evidence.clone();
    let (lease, acknowledged) = run_retaining_lease(lease, move |lease| {
        let component = retained.settlement.intent.component;
        lease.root().settle()?;
        lease.revalidate()?;
        let lane = open_component_settlement_lane(lease, component).map_err(effects_error)?;
        if let Some((settlement, guard)) =
            read_component_settlement(&lane).map_err(effects_error)?
        {
            if retained_evidence(lease, &lane, settlement, &guard)? != retained {
                return Err(ComponentEffectsError::Topology);
            }
            lane.lane
                .remove_guarded_file(COMPONENT_SETTLEMENT_FILE, &guard)?;
            lease.revalidate()?;
        } else {
            validate_retained_canonical_rows(lease, &retained.settlement)?;
        }
        validate_marker_free_settlement_shape(&lane, lease, &retained.settlement.outcome)
            .map_err(effects_error)
    })
    .await;
    match acknowledged {
        Some(()) => Ok(lease),
        None => Err((lease, evidence)),
    }
}

/// Reads the committed version metadata `evidence` proves while its settlement is retained.
pub(crate) async fn read_committed_version_metadata(
    lease: &ManagedRootPublicationLease,
    evidence: &ComponentSettlementEvidence,
) -> Result<Vec<u8>, ComponentEffectsError> {
    lease.revalidate()?;
    let retained = lease.retain_recovery();
    let evidence = evidence.clone();
    let bytes = run_publication_blocking(move || {
        let lease = retained.restore();
        let component = ManagedComponentKind::VersionBundle;
        if evidence.settlement.intent.component != component
            || evidence.committed_activation_contract().is_none()
        {
            return Err(ComponentEffectsError::Topology);
        }
        let lane = open_component_settlement_lane(&lease, component).map_err(effects_error)?;
        let (settlement, guard) = read_component_settlement(&lane)
            .map_err(effects_error)?
            .ok_or(ComponentEffectsError::Topology)?;
        if settlement != evidence.settlement
            || guard.identity() != evidence.identity
            || settlement_root_binding(
                lease.root(),
                &evidence.transaction_nonce(),
                &evidence.settlement_generation(),
            )
            .as_deref()
                != Some(evidence.root_binding())
        {
            return Err(ComponentEffectsError::Topology);
        }
        let row = settlement
            .rows
            .iter()
            .find(|row| row.kind == ManagedComponentArtifactKind::VersionMetadata)
            .ok_or(ComponentEffectsError::Topology)?;
        if row.final_size > MAX_KNOWN_GOOD_VERSION_JSON_BYTES as u64 {
            return Err(ComponentEffectsError::Topology);
        }
        let plan = plan_component_canonical_path(lease.root(), component, &row.path)?;
        let parent = plan.parent().ok_or(ComponentEffectsError::Topology)?;
        let file = parent
            .inspect_regular_file(plan.file_name())?
            .ok_or(ComponentEffectsError::Topology)?;
        let bytes = parent.read_guarded_file_bounded(plan.file_name(), &file, row.final_size)?;
        if bytes.len() as u64 != row.final_size
            || <[u8; 20]>::from(sha1::Sha1::digest(&bytes)) != row.final_sha1
            || !parent.file_guard_matches(plan.file_name(), &file)?
            || !lane
                .lane
                .file_guard_matches(COMPONENT_SETTLEMENT_FILE, &guard)?
        {
            return Err(ComponentEffectsError::Topology);
        }
        lease.revalidate()?;
        Ok(bytes)
    })
    .await??;
    lease.revalidate()?;
    Ok(bytes)
}

/// Opens a lane's retained settlement, refusing unsettled markers beside a missing one.
fn open_retained_settlement(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<
    Option<(ComponentLane, ComponentSettlementRecord, ManagedFileGuard)>,
    ComponentEffectsError,
> {
    let publication = lease.publication_directory();
    let lane_name = component_lane_name(component);
    if !publication.has_portably_exact_child_name(lane_name)? {
        return Ok(None);
    }
    let marker_lane = publication.open_child(lane_name)?;
    if !marker_lane.has_portably_exact_child_name(COMPONENT_SETTLEMENT_FILE)? {
        if marker_lane.has_portably_exact_child_name(COMPONENT_INTENT_FILE)?
            || marker_lane.has_portably_exact_child_name(COMPONENT_OUTCOME_FILE)?
        {
            return Err(ComponentEffectsError::Topology);
        }
        return Ok(None);
    }
    drop(marker_lane);
    let lane = open_component_settlement_lane(lease, component).map_err(effects_error)?;
    let (settlement, guard) = read_component_settlement(&lane)
        .map_err(effects_error)?
        .ok_or(ComponentEffectsError::Topology)?;
    if settlement.intent.component != component || !component_retains_settlement(component) {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(Some((lane, settlement, guard)))
}

/// Proves a retained settlement's whole persisted shape and its canonical terminal state.
fn retained_evidence(
    lease: &ManagedRootPublicationLease,
    lane: &ComponentLane,
    settlement: ComponentSettlementRecord,
    guard: &ManagedFileGuard,
) -> Result<ComponentSettlementEvidence, ComponentEffectsError> {
    validate_retained_settlement_shape(lane, lease, &settlement, guard).map_err(effects_error)?;
    validate_retained_canonical_rows(lease, &settlement)?;
    let version_id = version_bundle_version_id(&settlement.rows)
        .ok_or(ComponentEffectsError::Topology)?
        .to_string();
    let encoded = encode_component_settlement(
        &settlement.outcome,
        &settlement.encoded_intent,
        settlement.generation,
        &settlement.encoded_table,
    )
    .map_err(effects_error)?;
    let root_binding = settlement_root_binding(
        lease.root(),
        &hex::encode(settlement.intent.transaction_nonce),
        &hex::encode(settlement.generation),
    )
    .ok_or(ComponentEffectsError::Topology)?;
    let mut digest = EvidenceHasher::new(FINGERPRINT_DOMAIN);
    digest.write(root_binding.as_bytes());
    digest.write(&encoded);
    let fingerprint = digest.finish_hex();
    if !lane
        .lane
        .file_guard_matches(COMPONENT_SETTLEMENT_FILE, guard)?
    {
        return Err(ComponentEffectsError::Topology);
    }
    lease.revalidate()?;
    Ok(ComponentSettlementEvidence {
        settlement,
        identity: guard.identity(),
        version_id,
        root_binding,
        fingerprint,
    })
}

fn validate_retained_canonical_rows(
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
) -> Result<(), ComponentEffectsError> {
    let mut parents = ComponentCanonicalParents::default();
    for row in &settlement.rows {
        validate_settlement_canonical_row(
            lease,
            settlement.intent.component,
            row,
            settlement.outcome.terminal,
            &mut parents,
            false,
        )
        .map_err(effects_error)?;
    }
    Ok(())
}

/// Runs `work` with the lease held in a slot it cannot take, so a panic or a stopped worker
/// still returns exclusive authority to the caller.
async fn run_retaining_lease<T, F>(
    lease: ManagedRootPublicationLease,
    work: F,
) -> (ManagedRootPublicationLease, Option<T>)
where
    F: FnOnce(&ManagedRootPublicationLease) -> Result<T, ComponentEffectsError> + Send + 'static,
    T: Send + 'static,
{
    let holder = Arc::new(Mutex::new(Some(lease)));
    let worker = Arc::clone(&holder);
    let result = run_publication_blocking(move || {
        let slot = worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lease = slot.as_ref()?;
        catch_unwind(AssertUnwindSafe(|| work(lease))).ok()?.ok()
    })
    .await
    .ok()
    .flatten();
    let lease = holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("retained lease slot is never emptied by its worker");
    (lease, result)
}

fn effects_error(_: impl Sized) -> ComponentEffectsError {
    ComponentEffectsError::Topology
}

struct EvidenceHasher(Sha256);

impl EvidenceHasher {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update(b"\0");
        Self(digest)
    }

    fn finish_hex(self) -> String {
        hex::encode(self.0.finalize())
    }
}

impl Hasher for EvidenceHasher {
    fn finish(&self) -> u64 {
        0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }
}
