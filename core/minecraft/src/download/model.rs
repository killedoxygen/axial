use super::install::{
    ManagedInstallAcknowledgementRecoveryState, ManagedInstallDurableEvidenceState,
    ManagedInstallDurableRecoveryState, ManagedInstallPublicationSeed,
};
use crate::known_good::KnownGoodReconstructionReceipt;
use crate::managed_fs::ManagedLibraryOperation;
use crate::managed_publication::ManagedRootPublicationLease;
use crate::portable_path::PortableRelativePath;
use crate::runtime::RuntimeSourceFailure;
use crate::version_bundle_publication::VersionBundleTransactionRecovery;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::io;
use thiserror::Error;

const MANAGED_INSTALL_EVIDENCE_PREFIX: &str = "managed-install-v1";
const MANAGED_INSTALL_EVIDENCE_ENCODED_LEN: usize =
    MANAGED_INSTALL_EVIDENCE_PREFIX.len() + 5 + 43 + 22 + 22 + 43 + 43;
const MANAGED_INSTALL_ACTIVATION_CONTRACT_PREFIX: &str = "managed-install-activation-v1";
const MANAGED_INSTALL_ACTIVATION_CONTRACT_ENCODED_LEN: usize =
    MANAGED_INSTALL_ACTIVATION_CONTRACT_PREFIX.len() + 1 + 43;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub phase: String,
    pub current: i32,
    pub total: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub done: bool,
    /// Cumulative transfer-plan facts for the whole install: bytes of planned
    /// work completed vs. planned so far. Stamped by the installer entry
    /// points; absent on events emitted before the plan has any entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
}

#[must_use = "dropping recovery releases the exact managed install publication authority"]
pub struct ManagedInstallPublicationRecovery {
    pub(crate) state: ManagedInstallPublicationRecoveryState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedInstallRollbackEffect {
    Promotion,
    Postcheck,
    Rollback,
}

pub enum ManagedInstallDurableOutcome {
    NoEffect,
    Mismatch,
    Committed(ManagedInstallCommittedEvidence),
    RolledBack {
        evidence: ManagedInstallRolledBackEvidence,
        effect: ManagedInstallRollbackEffect,
    },
    Indeterminate(ManagedInstallDurableRecovery),
}

pub struct ManagedInstallCommittedEvidence {
    pub(crate) state: ManagedInstallDurableEvidenceState,
}

pub struct ManagedInstallRolledBackEvidence {
    pub(crate) state: ManagedInstallDurableEvidenceState,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ManagedInstallActivationContractId {
    value: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("managed install activation contract id is invalid")]
pub struct ManagedInstallActivationContractIdError;

#[must_use = "verified receipt authority must be consumed before activation or acknowledgement"]
pub struct VerifiedManagedInstallReceipt<R> {
    pub(crate) evidence: ManagedInstallCommittedEvidence,
    pub(crate) receipt: R,
    pub(crate) activation_contract_id: ManagedInstallActivationContractId,
}

impl<R> VerifiedManagedInstallReceipt<R> {
    pub fn activation_contract_id(&self) -> &ManagedInstallActivationContractId {
        &self.activation_contract_id
    }
}

impl<R> std::fmt::Debug for VerifiedManagedInstallReceipt<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedManagedInstallReceipt")
            .field("activation_contract_id", &self.activation_contract_id)
            .finish_non_exhaustive()
    }
}

#[must_use = "failed verification retains the publication evidence and untrusted receipt"]
pub struct ManagedInstallReceiptVerificationFailure<R> {
    pub(crate) evidence: ManagedInstallCommittedEvidence,
    pub(crate) receipt: R,
}

impl<R> ManagedInstallReceiptVerificationFailure<R> {
    pub fn into_parts(self) -> (ManagedInstallCommittedEvidence, R) {
        (self.evidence, self.receipt)
    }
}

impl<R> std::fmt::Debug for ManagedInstallReceiptVerificationFailure<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallReceiptVerificationFailure")
            .finish_non_exhaustive()
    }
}

impl<R> std::fmt::Display for ManagedInstallReceiptVerificationFailure<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("managed install receipt does not match durable activation contract")
    }
}

impl<R: 'static> std::error::Error for ManagedInstallReceiptVerificationFailure<R> {}

#[must_use = "verified checkpoint receipt authority must be consumed before activation"]
pub struct VerifiedManagedInstallCheckpointReceipt<R> {
    pub(crate) receipt: R,
    pub(crate) activation_contract_id: ManagedInstallActivationContractId,
}

impl<R> VerifiedManagedInstallCheckpointReceipt<R> {
    pub fn activation_contract_id(&self) -> &ManagedInstallActivationContractId {
        &self.activation_contract_id
    }
}

impl<R> std::fmt::Debug for VerifiedManagedInstallCheckpointReceipt<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedManagedInstallCheckpointReceipt")
            .field("activation_contract_id", &self.activation_contract_id)
            .finish_non_exhaustive()
    }
}

#[must_use = "failed checkpoint verification retains the untrusted receipt"]
pub struct ManagedInstallCheckpointVerificationFailure<R> {
    pub(crate) receipt: R,
}

impl<R> ManagedInstallCheckpointVerificationFailure<R> {
    pub fn into_receipt(self) -> R {
        self.receipt
    }
}

impl<R> std::fmt::Debug for ManagedInstallCheckpointVerificationFailure<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallCheckpointVerificationFailure")
            .finish_non_exhaustive()
    }
}

impl<R> std::fmt::Display for ManagedInstallCheckpointVerificationFailure<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("reconstructed receipt does not match checkpoint activation contract")
    }
}

impl<R: 'static> std::error::Error for ManagedInstallCheckpointVerificationFailure<R> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("known-good activation callback rejected the verified authority")]
pub struct KnownGoodActivationRejected;

/// Move-only authority for establishing the first registered known-good state.
///
/// This bootstrap authority must not replace an existing persisted known-good snapshot.
#[must_use = "verified registered bootstrap must be consumed by its activation transition"]
pub struct VerifiedRegisteredKnownGoodBootstrap {
    pub(crate) receipt: KnownGoodReconstructionReceipt,
    pub(crate) activation_contract_id: ManagedInstallActivationContractId,
    pub(crate) managed_root: ManagedLibraryOperation,
    pub(crate) publication_lease: ManagedRootPublicationLease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisteredKnownGoodBootstrapVerificationFailureKind {
    InvalidActivationContract,
    PhysicalProjectionMismatch,
    MismatchedPublication,
    CommittedPublication,
    RolledBackPublication,
    IndeterminatePublication,
}

#[must_use = "failed verification retains the reconstruction receipt and durable publication state"]
pub struct RegisteredKnownGoodBootstrapVerificationFailure {
    pub(crate) state: RegisteredKnownGoodBootstrapVerificationFailureState,
    pub(crate) kind: RegisteredKnownGoodBootstrapVerificationFailureKind,
}

pub(crate) enum RegisteredKnownGoodBootstrapVerificationFailureState {
    InvalidActivationContract(KnownGoodReconstructionReceipt),
    PhysicalProjectionMismatch(KnownGoodReconstructionReceipt),
    Publication {
        receipt: KnownGoodReconstructionReceipt,
        outcome: ManagedInstallDurableOutcome,
    },
}

#[must_use = "verification recovery must be reconciled or explicitly discarded"]
pub enum RegisteredKnownGoodBootstrapVerificationRecovery {
    InvalidActivationContract(KnownGoodReconstructionReceipt),
    PhysicalProjectionMismatch(KnownGoodReconstructionReceipt),
    Publication {
        receipt: KnownGoodReconstructionReceipt,
        outcome: ManagedInstallDurableOutcome,
    },
}

impl VerifiedRegisteredKnownGoodBootstrap {
    pub fn activation_contract_id(&self) -> &ManagedInstallActivationContractId {
        &self.activation_contract_id
    }
}

impl std::fmt::Debug for VerifiedRegisteredKnownGoodBootstrap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedRegisteredKnownGoodBootstrap")
            .field("activation_contract_id", &self.activation_contract_id)
            .finish_non_exhaustive()
    }
}

impl RegisteredKnownGoodBootstrapVerificationFailure {
    pub fn kind(&self) -> RegisteredKnownGoodBootstrapVerificationFailureKind {
        self.kind
    }

    pub fn into_recovery(self) -> RegisteredKnownGoodBootstrapVerificationRecovery {
        match self.state {
            RegisteredKnownGoodBootstrapVerificationFailureState::InvalidActivationContract(
                receipt,
            ) => {
                RegisteredKnownGoodBootstrapVerificationRecovery::InvalidActivationContract(receipt)
            }
            RegisteredKnownGoodBootstrapVerificationFailureState::PhysicalProjectionMismatch(
                receipt,
            ) => RegisteredKnownGoodBootstrapVerificationRecovery::PhysicalProjectionMismatch(
                receipt,
            ),
            RegisteredKnownGoodBootstrapVerificationFailureState::Publication {
                receipt,
                outcome,
            } => RegisteredKnownGoodBootstrapVerificationRecovery::Publication { receipt, outcome },
        }
    }
}

impl std::fmt::Debug for RegisteredKnownGoodBootstrapVerificationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredKnownGoodBootstrapVerificationFailure")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RegisteredKnownGoodBootstrapVerificationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.kind {
            RegisteredKnownGoodBootstrapVerificationFailureKind::InvalidActivationContract => {
                "registered reconstruction activation contract is invalid"
            }
            RegisteredKnownGoodBootstrapVerificationFailureKind::PhysicalProjectionMismatch => {
                "registered reconstruction does not match the managed VersionBundle"
            }
            RegisteredKnownGoodBootstrapVerificationFailureKind::MismatchedPublication => {
                "registered reconstruction found a publication owned by another operation"
            }
            RegisteredKnownGoodBootstrapVerificationFailureKind::CommittedPublication => {
                "registered reconstruction found an unacknowledged committed publication"
            }
            RegisteredKnownGoodBootstrapVerificationFailureKind::RolledBackPublication => {
                "registered reconstruction found an unacknowledged rolled-back publication"
            }
            RegisteredKnownGoodBootstrapVerificationFailureKind::IndeterminatePublication => {
                "registered reconstruction publication state is indeterminate"
            }
        })
    }
}

impl std::error::Error for RegisteredKnownGoodBootstrapVerificationFailure {}

#[must_use = "dropping recovery releases publication ownership but leaves durable evidence intact"]
pub struct ManagedInstallDurableRecovery {
    pub(crate) state: ManagedInstallDurableRecoveryState,
}

#[must_use = "durable publication acknowledgement is required after successful activation"]
pub struct ManagedInstallPostActivationAcknowledgement {
    pub(crate) state: ManagedInstallDurableEvidenceState,
}

pub enum ManagedInstallAcknowledgementOutcome {
    Acknowledged,
    Indeterminate(ManagedInstallAcknowledgementRecovery),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedInstallPublicationCandidates {
    primary: String,
    alternate: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("managed install publication candidates are invalid")]
pub struct ManagedInstallPublicationCandidatesError;

impl ManagedInstallPublicationCandidates {
    pub fn one(
        version_id: impl Into<String>,
    ) -> Result<Self, ManagedInstallPublicationCandidatesError> {
        let version_id = version_id.into();
        validate_managed_install_candidate(&version_id)?;
        Ok(Self {
            primary: version_id,
            alternate: None,
        })
    }

    pub fn pair(
        primary: impl Into<String>,
        alternate: impl Into<String>,
    ) -> Result<Self, ManagedInstallPublicationCandidatesError> {
        let primary = primary.into();
        let alternate = alternate.into();
        validate_managed_install_candidate(&primary)?;
        validate_managed_install_candidate(&alternate)?;
        if primary == alternate {
            return Err(ManagedInstallPublicationCandidatesError);
        }
        Ok(Self {
            primary,
            alternate: Some(alternate),
        })
    }

    pub(crate) fn one_unchecked(version_id: String) -> Self {
        Self {
            primary: version_id,
            alternate: None,
        }
    }

    pub(crate) fn contains(&self, version_id: &str) -> bool {
        self.primary == version_id || self.alternate.as_deref() == Some(version_id)
    }
}

fn validate_managed_install_candidate(
    version_id: &str,
) -> Result<(), ManagedInstallPublicationCandidatesError> {
    if crate::portable_path::PortableFileName::new_exact(version_id).is_err() {
        return Err(ManagedInstallPublicationCandidatesError);
    }
    Ok(())
}

#[must_use = "dropping acknowledgement recovery leaves the durable witness intact"]
pub struct ManagedInstallAcknowledgementRecovery {
    pub(crate) state: ManagedInstallAcknowledgementRecoveryState,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ManagedInstallPublicationEvidenceId {
    value: String,
    version_binding: [u8; 32],
    transaction_nonce: String,
    settlement_generation: String,
    root_binding: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("managed install publication evidence id is invalid")]
pub struct ManagedInstallPublicationEvidenceIdError;

impl ManagedInstallPublicationEvidenceId {
    pub fn parse(value: &str) -> Result<Self, ManagedInstallPublicationEvidenceIdError> {
        if value.len() != MANAGED_INSTALL_EVIDENCE_ENCODED_LEN {
            return Err(ManagedInstallPublicationEvidenceIdError);
        }
        let mut parts = value.split('.');
        let prefix = parts.next();
        let encoded_version_binding = parts.next();
        let encoded_transaction_nonce = parts.next();
        let encoded_settlement_generation = parts.next();
        let encoded_root_binding = parts.next();
        let encoded_fingerprint = parts.next();
        if prefix != Some(MANAGED_INSTALL_EVIDENCE_PREFIX) || parts.next().is_some() {
            return Err(ManagedInstallPublicationEvidenceIdError);
        }
        let version_binding = decode_compact_evidence_field::<32>(
            encoded_version_binding.ok_or(ManagedInstallPublicationEvidenceIdError)?,
        )?;
        let transaction_nonce = decode_compact_evidence_field::<16>(
            encoded_transaction_nonce.ok_or(ManagedInstallPublicationEvidenceIdError)?,
        )?;
        let settlement_generation = decode_compact_evidence_field::<16>(
            encoded_settlement_generation.ok_or(ManagedInstallPublicationEvidenceIdError)?,
        )?;
        let root_binding = decode_compact_evidence_field::<32>(
            encoded_root_binding.ok_or(ManagedInstallPublicationEvidenceIdError)?,
        )?;
        decode_compact_evidence_field::<32>(
            encoded_fingerprint.ok_or(ManagedInstallPublicationEvidenceIdError)?,
        )?;
        Ok(Self {
            value: value.to_string(),
            version_binding,
            transaction_nonce: encode_evidence_hex(&transaction_nonce),
            settlement_generation: encode_evidence_hex(&settlement_generation),
            root_binding: encode_evidence_hex(&root_binding),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn matches_version_id(&self, version_id: &str) -> bool {
        let observed: [u8; 32] = Sha256::digest(version_id.as_bytes()).into();
        self.version_binding == observed
    }

    pub(crate) fn from_parts(
        version_id: &str,
        transaction_nonce: &str,
        settlement_generation: &str,
        root_binding: &str,
        fingerprint: &str,
    ) -> Self {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let transaction_nonce =
            decode_evidence_hex::<16>(transaction_nonce).expect("validated transaction nonce");
        let settlement_generation = decode_evidence_hex::<16>(settlement_generation)
            .expect("validated settlement generation");
        let root_binding = decode_evidence_hex::<32>(root_binding).expect("validated root binding");
        let fingerprint =
            decode_evidence_hex::<32>(fingerprint).expect("validated settlement fingerprint");
        let value = format!(
            "{MANAGED_INSTALL_EVIDENCE_PREFIX}.{}.{}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(version_id.as_bytes())),
            URL_SAFE_NO_PAD.encode(transaction_nonce),
            URL_SAFE_NO_PAD.encode(settlement_generation),
            URL_SAFE_NO_PAD.encode(root_binding),
            URL_SAFE_NO_PAD.encode(fingerprint)
        );
        Self::parse(&value).expect("validated durable settlement produces canonical evidence")
    }

    pub(crate) fn binding_parts(&self) -> (&str, &str, &str) {
        (
            &self.transaction_nonce,
            &self.settlement_generation,
            &self.root_binding,
        )
    }
}

impl ManagedInstallActivationContractId {
    pub fn parse(value: &str) -> Result<Self, ManagedInstallActivationContractIdError> {
        if value.len() != MANAGED_INSTALL_ACTIVATION_CONTRACT_ENCODED_LEN {
            return Err(ManagedInstallActivationContractIdError);
        }
        let mut parts = value.split('.');
        let prefix = parts.next();
        let encoded_digest = parts.next();
        if prefix != Some(MANAGED_INSTALL_ACTIVATION_CONTRACT_PREFIX) || parts.next().is_some() {
            return Err(ManagedInstallActivationContractIdError);
        }
        decode_compact_evidence_field::<32>(
            encoded_digest.ok_or(ManagedInstallActivationContractIdError)?,
        )
        .map_err(|_| ManagedInstallActivationContractIdError)?;
        Ok(Self {
            value: value.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub(crate) fn from_digest(digest: [u8; 32]) -> Self {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let value = format!(
            "{MANAGED_INSTALL_ACTIVATION_CONTRACT_PREFIX}.{}",
            URL_SAFE_NO_PAD.encode(digest)
        );
        Self::parse(&value).expect("activation contract digest produces canonical evidence")
    }
}

impl std::fmt::Display for ManagedInstallActivationContractId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ManagedInstallActivationContractId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedInstallActivationContractId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for ManagedInstallPublicationEvidenceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ManagedInstallPublicationEvidenceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ManagedInstallPublicationEvidenceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

fn decode_compact_evidence_field<const N: usize>(
    value: &str,
) -> Result<[u8; N], ManagedInstallPublicationEvidenceIdError> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ManagedInstallPublicationEvidenceIdError)?;
    let decoded: [u8; N] = decoded
        .try_into()
        .map_err(|_| ManagedInstallPublicationEvidenceIdError)?;
    if URL_SAFE_NO_PAD.encode(decoded) != value {
        return Err(ManagedInstallPublicationEvidenceIdError);
    }
    Ok(decoded)
}

fn decode_evidence_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (evidence_hex_nibble(pair[0]) << 4) | evidence_hex_nibble(pair[1]);
    }
    Some(decoded)
}

fn evidence_hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("evidence hex was validated"),
    }
}

fn encode_evidence_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod evidence_id_tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const GENERATION: &str = "fedcba9876543210fedcba9876543210";
    const BINDING: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const FINGERPRINT: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    #[test]
    fn activation_contract_id_is_canonical_and_round_trips() {
        let id = ManagedInstallActivationContractId::from_digest([0x5a; 32]);

        assert_eq!(
            ManagedInstallActivationContractId::parse(id.as_str()),
            Ok(id.clone())
        );
        assert_eq!(id.as_str().split('.').count(), 2);
        assert_eq!(
            serde_json::from_str::<ManagedInstallActivationContractId>(
                &serde_json::to_string(&id).expect("serialize activation contract")
            )
            .expect("deserialize activation contract"),
            id
        );
    }

    #[test]
    fn activation_contract_id_parser_rejects_noncanonical_values() {
        let canonical = ManagedInstallActivationContractId::from_digest([0x5a; 32]);
        let mut padded = canonical.as_str().to_string();
        padded.push('=');
        let mut invalid_alphabet = canonical.as_str().to_string();
        invalid_alphabet.replace_range(invalid_alphabet.len() - 1.., "*");

        for invalid in [
            canonical.as_str().replacen(
                MANAGED_INSTALL_ACTIVATION_CONTRACT_PREFIX,
                "activation-v2",
                1,
            ),
            padded,
            invalid_alphabet,
            format!("{}.extra", canonical.as_str()),
            "x".repeat(MANAGED_INSTALL_ACTIVATION_CONTRACT_ENCODED_LEN * 1024),
        ] {
            assert!(
                ManagedInstallActivationContractId::parse(&invalid).is_err(),
                "accepted invalid activation contract id: {invalid}"
            );
        }
    }

    #[test]
    fn evidence_id_is_a_canonical_fixed_shape_string() {
        let id = ManagedInstallPublicationEvidenceId::from_parts(
            "1.21.5",
            NONCE,
            GENERATION,
            BINDING,
            FINGERPRINT,
        );

        assert_eq!(
            ManagedInstallPublicationEvidenceId::parse(id.as_str()),
            Ok(id.clone())
        );
        assert!(id.matches_version_id("1.21.5"));
        assert!(!id.matches_version_id("1.21.6"));
        let parts = id.as_str().split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 6);
        assert_eq!(parts[1].len(), 43);
        assert_eq!(parts[2].len(), 22);
        assert_eq!(parts[3].len(), 22);
        assert_eq!(parts[4].len(), 43);
        assert_eq!(parts[5].len(), 43);
        let encoded = serde_json::to_string(&id).expect("serialize evidence id");
        assert_eq!(
            serde_json::from_str::<ManagedInstallPublicationEvidenceId>(&encoded)
                .expect("deserialize evidence id"),
            id
        );
    }

    #[test]
    fn evidence_id_parser_rejects_noncanonical_and_mixed_contracts() {
        let canonical = ManagedInstallPublicationEvidenceId::from_parts(
            "1.21.5",
            NONCE,
            GENERATION,
            BINDING,
            FINGERPRINT,
        );
        let mut parts = canonical
            .as_str()
            .split('.')
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut invalid = vec![
            canonical
                .as_str()
                .replacen("managed-install-v1", "managed-install-v2", 1),
            format!("{}.extra", canonical.as_str()),
            "x".repeat(MANAGED_INSTALL_EVIDENCE_ENCODED_LEN * 1024),
        ];
        parts[1].push('=');
        invalid.push(parts.join("."));
        parts = canonical.as_str().split('.').map(str::to_string).collect();
        parts[1] = "YS9i".to_string();
        invalid.push(parts.join("."));
        parts = canonical.as_str().split('.').map(str::to_string).collect();
        parts[2].push('=');
        invalid.push(parts.join("."));
        parts = canonical.as_str().split('.').map(str::to_string).collect();
        parts[4].pop();
        invalid.push(parts.join("."));
        parts = canonical.as_str().split('.').map(str::to_string).collect();
        parts[5].replace_range(..1, "*");
        invalid.push(parts.join("."));

        for value in invalid {
            assert!(
                ManagedInstallPublicationEvidenceId::parse(&value).is_err(),
                "accepted invalid evidence id: {value}"
            );
        }
    }

    #[test]
    fn maximum_portable_version_evidence_fits_the_operation_fact_contract() {
        let version_id = "v".repeat(255);
        let id = ManagedInstallPublicationEvidenceId::from_parts(
            &version_id,
            NONCE,
            GENERATION,
            BINDING,
            FINGERPRINT,
        );

        assert_eq!(
            ManagedInstallPublicationEvidenceId::parse(id.as_str()),
            Ok(id.clone())
        );
        assert!(id.matches_version_id(&version_id));
        assert!(!id.matches_version_id(&format!("{version_id}x")));
        let fact = format!("install_publication_evidence:{id}");
        assert!(
            fact.len() <= 320,
            "evidence fact exceeded journal contract: {} bytes",
            fact.len()
        );
    }

    #[test]
    fn publication_candidates_are_exact_distinct_and_preserve_portable_id_capacity() {
        let maximum = "v".repeat(255);
        let candidates = ManagedInstallPublicationCandidates::pair(&maximum, "loader-child")
            .expect("maximum portable version candidate");
        assert!(candidates.contains(&maximum));
        assert!(candidates.contains("loader-child"));
        assert!(ManagedInstallPublicationCandidates::pair("same", "same").is_err());
        assert!(ManagedInstallPublicationCandidates::one("../unsafe").is_err());
    }
}

impl std::fmt::Debug for ManagedInstallCommittedEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallCommittedEvidence")
            .field("version_id", &self.version_id())
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedInstallRolledBackEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallRolledBackEvidence")
            .field("version_id", &self.version_id())
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedInstallPostActivationAcknowledgement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallPostActivationAcknowledgement")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedInstallDurableRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallDurableRecovery")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedInstallAcknowledgementRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedInstallAcknowledgementRecovery")
            .finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ManagedInstallDurableOutcome {
    pub fn indeterminate_fixture_with_retries_for_test(
        remaining_indeterminate_retries: usize,
    ) -> Self {
        Self::Indeterminate(ManagedInstallDurableRecovery {
            state: ManagedInstallDurableRecoveryState::Fixture {
                remaining_indeterminate_retries: Some(remaining_indeterminate_retries),
            },
        })
    }

    pub fn permanently_indeterminate_fixture_for_test() -> Self {
        Self::Indeterminate(ManagedInstallDurableRecovery {
            state: ManagedInstallDurableRecoveryState::Fixture {
                remaining_indeterminate_retries: None,
            },
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ManagedInstallAcknowledgementOutcome {
    pub fn indeterminate_fixture_with_retries_for_test(
        remaining_indeterminate_retries: usize,
    ) -> Self {
        Self::Indeterminate(ManagedInstallAcknowledgementRecovery {
            state: ManagedInstallAcknowledgementRecoveryState::Fixture {
                remaining_indeterminate_retries: Some(remaining_indeterminate_retries),
            },
        })
    }

    pub fn permanently_indeterminate_fixture_for_test() -> Self {
        Self::Indeterminate(ManagedInstallAcknowledgementRecovery {
            state: ManagedInstallAcknowledgementRecoveryState::Fixture {
                remaining_indeterminate_retries: None,
            },
        })
    }
}

pub(crate) enum ManagedInstallPublicationRecoveryState {
    Active {
        seed: ManagedInstallPublicationSeed,
        publication: VersionBundleTransactionRecovery,
    },
    Recover {
        seed: ManagedInstallPublicationSeed,
        classification: Option<ManagedInstallDurableRecovery>,
    },
    #[cfg(any(test, feature = "test-support"))]
    Fixture {
        remaining_indeterminate_retries: usize,
    },
}

impl std::fmt::Debug for ManagedInstallPublicationRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            ManagedInstallPublicationRecoveryState::Active { .. } => "active",
            ManagedInstallPublicationRecoveryState::Recover { .. } => "recover",
            #[cfg(any(test, feature = "test-support"))]
            ManagedInstallPublicationRecoveryState::Fixture { .. } => "fixture",
        };
        formatter
            .debug_struct("ManagedInstallPublicationRecovery")
            .field("state", &state)
            .finish_non_exhaustive()
    }
}

impl ManagedInstallPublicationRecovery {
    pub(crate) fn new(
        seed: ManagedInstallPublicationSeed,
        publication: VersionBundleTransactionRecovery,
    ) -> Self {
        Self {
            state: ManagedInstallPublicationRecoveryState::Active { seed, publication },
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_for_test() -> Self {
        Self::fixture_with_indeterminate_retries_for_test(0)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_with_indeterminate_retries_for_test(
        remaining_indeterminate_retries: usize,
    ) -> Self {
        Self {
            state: ManagedInstallPublicationRecoveryState::Fixture {
                remaining_indeterminate_retries,
            },
        }
    }
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("file operation failed: {0}")]
    FileOperation(#[from] io::Error),
    #[error("resolve manifest url: {0}")]
    ResolveManifest(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parse version json: {0}")]
    ParseVersion(#[from] serde_json::Error),
    #[error("prepare java runtime: {0}")]
    PrepareRuntime(String),
    #[error("acquire java runtime source: {0}")]
    RuntimeSource(RuntimeSourceFailure),
    #[error("java runtime {component} is not available for {platform}")]
    RuntimeUnavailableForPlatform { component: String, platform: String },
    #[error(
        "java runtime {component} needs Rosetta 2 on this Mac: run `softwareupdate --install-rosetta --agree-to-license` in Terminal"
    )]
    RuntimeRosettaRequired { component: String },
    #[error("download integrity: {0}")]
    Integrity(String),
    #[error("managed install publication remains indeterminate")]
    PublicationIndeterminate(ManagedInstallPublicationRecovery),
    #[error(transparent)]
    LibraryPlan(#[from] LibraryPlanError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LibraryPlanError {
    #[error("library metadata contains an unsafe artifact path")]
    InvalidArtifactPath,
    #[error("library metadata contains an invalid checksum")]
    InvalidChecksum,
    #[error("library artifact has no download source")]
    MissingDownloadSource,
    #[error("library artifacts have conflicting contracts for the same path")]
    ConflictingArtifactPath,
    #[error("library artifact integrity metadata conflicts across representations")]
    ConflictingArtifactIntegrity,
}

pub(crate) struct ExactLibraryDownloadProof {
    path: PortableRelativePath,
    is_native: bool,
    provider_url: String,
    expected: ExpectedIntegrity,
    size: u64,
    sha1: [u8; 20],
}

impl ExactLibraryDownloadProof {
    pub(super) fn new(
        path: PortableRelativePath,
        is_native: bool,
        provider_url: String,
        expected: ExpectedIntegrity,
        size: u64,
        sha1: [u8; 20],
    ) -> Self {
        Self {
            path,
            is_native,
            provider_url,
            expected,
            size,
            sha1,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        PortableRelativePath,
        bool,
        String,
        ExpectedIntegrity,
        u64,
        [u8; 20],
    ) {
        (
            self.path,
            self.is_native,
            self.provider_url,
            self.expected,
            self.size,
            self.sha1,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_bound_for_test(
        path: PortableRelativePath,
        is_native: bool,
        provider_url: String,
        expected: ExpectedIntegrity,
        size: u64,
        sha1: [u8; 20],
    ) -> Self {
        Self::new(path, is_native, provider_url, expected, size, sha1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExpectedIntegrity {
    pub size: Option<u64>,
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DownloadIntegrityError {
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    Sha1Mismatch {
        file: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for DownloadIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "{file} size mismatch: expected {expected}, got {actual}"
            ),
            Self::Sha1Mismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "{file} sha1 mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl ExpectedIntegrity {
    pub fn from_mojang(size: i64, sha1: &str) -> Self {
        Self {
            size: u64::try_from(size).ok().filter(|value| *value > 0),
            sha1: non_empty_sha1(sha1),
        }
    }

    pub fn from_sha1(sha1: &str) -> Self {
        Self {
            size: None,
            sha1: non_empty_sha1(sha1),
        }
    }

    pub fn has_evidence(&self) -> bool {
        self.size.is_some() || self.sha1.is_some()
    }

    pub fn has_checksum(&self) -> bool {
        self.sha1
            .as_deref()
            .is_some_and(super::integrity::is_sha1_hex)
    }
}

fn non_empty_sha1(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionDownloadFactKind {
    ChecksumMismatch,
    MetadataInvalid,
    MetadataMissing,
    Interrupted,
    NetworkFailure,
    PermissionFailure,
    PromoteFailed,
    ProviderFailure,
    SizeMismatch,
    TempDiscarded,
    TempWriteFailed,
    WrittenToTemp,
    Promoted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionDownloadFact {
    pub kind: ExecutionDownloadFactKind,
    pub target: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectedDownloadArtifactKind {
    VersionJson,
    ClientJar,
    Library,
    AssetIndex,
    AssetObject,
    LogConfig,
}

#[derive(Debug)]
pub struct ExecutionDownloadError {
    pub kind: ExecutionDownloadFactKind,
    pub facts: Vec<ExecutionDownloadFact>,
    pub(super) error: DownloadError,
}

impl std::fmt::Display for ExecutionDownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "download execution failed for {} ({:?})",
            self.facts
                .last()
                .map(|fact| fact.target.as_str())
                .unwrap_or("artifact"),
            self.kind
        )
    }
}

impl std::error::Error for ExecutionDownloadError {}

impl ExecutionDownloadError {
    pub fn io_error_kind(&self) -> Option<io::ErrorKind> {
        match &self.error {
            DownloadError::FileOperation(error) => Some(error.kind()),
            _ => None,
        }
    }

    pub fn into_download_error(self) -> DownloadError {
        let Self { kind, facts, error } = self;
        let _fact_report = (kind, facts);
        error
    }
}

pub(super) fn progress(
    phase: &str,
    current: i32,
    total: i32,
    file: Option<String>,
) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}
