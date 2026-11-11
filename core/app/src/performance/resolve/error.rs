use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("failed to parse builtin performance manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported schema_version")]
    UnsupportedSchema,
    #[error("performance manifest exceeds the current {0} bound")]
    ManifestBound(&'static str),
    #[error("minimum_app_version is required")]
    MissingMinimumAppVersion,
    #[error("minimum_app_version is invalid: {0}")]
    InvalidMinimumAppVersion(String),
    #[error("running app version is invalid: {0}")]
    InvalidRunningAppVersion(String),
    #[error("manifest requires app version {required}, but running app version is {running}")]
    UnsupportedAppVersion { required: String, running: String },
    #[error("rule_channel is required")]
    MissingRuleChannel,
    #[error("unsupported rule_channel: {0}")]
    UnsupportedRuleChannel(String),
    #[error("artifact id is required")]
    MissingArtifactId,
    #[error("duplicate artifact id: {0}")]
    DuplicateArtifactId(String),
    #[error("artifact {0} source project_id is required")]
    MissingArtifactProjectId(String),
    #[error("artifact {0} source slug is required")]
    MissingArtifactSlug(String),
    #[error("artifact {0} must be composition_managed")]
    InvalidArtifactOwnership(String),
    #[error("managed mod artifact_id is required")]
    MissingManagedModArtifactId,
    #[error("managed mod references unknown artifact: {0}")]
    UnknownManagedModArtifact(String),
    #[error("managed mod {artifact_id} project_id mismatch: expected {expected}, found {actual}")]
    ManagedModProjectMismatch {
        artifact_id: String,
        expected: String,
        actual: String,
    },
    #[error("managed mod {artifact_id} slug mismatch: expected {expected}, found {actual}")]
    ManagedModSlugMismatch {
        artifact_id: String,
        expected: String,
        actual: String,
    },
    #[error("managed mod {artifact_id} has invalid version_range: {version_range}")]
    InvalidManagedModVersionRange {
        artifact_id: String,
        version_range: String,
    },
    #[error("managed mod {artifact_id} cannot declare both version_range and exact_game_versions")]
    ConflictingManagedModVersionSelectors { artifact_id: String },
    #[error("managed mod {artifact_id} has invalid exact_game_versions entry: {game_version}")]
    InvalidManagedModExactGameVersion {
        artifact_id: String,
        game_version: String,
    },
    #[error("managed mod {artifact_id} has duplicate exact_game_versions entry: {game_version}")]
    DuplicateManagedModExactGameVersion {
        artifact_id: String,
        game_version: String,
    },
    #[error("managed mod {artifact_id} has invalid hardware_req.{field}: {value}")]
    InvalidManagedModHardwareRequirement {
        artifact_id: String,
        field: &'static str,
        value: i32,
    },
    #[error("managed mod {artifact_id} has invalid mutual_exclusions.{field}: {value}")]
    InvalidManagedModMutualExclusion {
        artifact_id: String,
        field: &'static str,
        value: String,
    },
    #[error("composition id is required")]
    MissingCompositionId,
    #[error("duplicate composition id: {0}")]
    DuplicateCompositionId(String),
    #[error("emergency disable id is required")]
    MissingEmergencyDisableId,
    #[error("emergency disable target_id is required")]
    MissingEmergencyDisableTargetId,
    #[error("emergency disable reason is required")]
    MissingEmergencyDisableReason,
    #[error("duplicate emergency disable id: {0}")]
    DuplicateEmergencyDisableId(String),
    #[error("emergency composition disable references unknown composition: {0}")]
    UnknownEmergencyDisableComposition(String),
    #[error("emergency artifact disable references unknown managed artifact: {0}")]
    UnknownEmergencyDisableArtifact(String),
}
