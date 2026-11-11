//! Performance wire data and canonical leaf models. Filesystem authorities stay private.

use super::effective::EffectivePerformancePlan;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionFamily {
    #[serde(rename = "A")]
    A,
    #[serde(rename = "B")]
    B,
    #[serde(rename = "C")]
    C,
    #[serde(rename = "D")]
    D,
    #[serde(rename = "E")]
    E,
    #[serde(rename = "F")]
    F,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceMode {
    Managed,
    Vanilla,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionTier {
    Extended,
    Core,
    VanillaEnhanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedArtifactRole {
    Root,
    RequiredDependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModCondition {
    Always,
    Hardware,
    VersionRange,
    Recommend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmergencyDisableTarget {
    Composition,
    Artifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipClass {
    CompositionManaged,
    UserManaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedArtifactType {
    #[serde(rename = "mod")]
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedArtifactProvider {
    Modrinth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedArtifactChecksumPolicy {
    ProviderSha512,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedArtifactSource {
    pub provider: ManagedArtifactProvider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedArtifactDefinitionSource {
    pub provider: ManagedArtifactProvider,
    pub project_id: String,
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedArtifactDefinition {
    pub id: String,
    #[serde(rename = "type")]
    pub artifact_type: ManagedArtifactType,
    pub source: ManagedArtifactDefinitionSource,
    pub checksum_policy: ManagedArtifactChecksumPolicy,
    pub ownership_class: OwnershipClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedArtifactIntegrity {
    pub sha512: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HardwareRequirement {
    #[serde(default)]
    pub gpu_vendor: String,
    #[serde(default)]
    pub gpu_arch_min: i32,
    #[serde(default)]
    pub min_ram_mb: i32,
    #[serde(default)]
    pub min_cores: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedMod {
    pub artifact_id: String,
    pub project_id: String,
    pub slug: String,
    pub name: String,
    pub condition: ModCondition,
    #[serde(default)]
    pub version_range: String,
    #[serde(default)]
    pub exact_game_versions: Vec<String>,
    #[serde(default)]
    pub hardware_req: Option<HardwareRequirement>,
    #[serde(default)]
    pub mutual_exclusions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionPlan {
    pub composition_id: String,
    pub family: VersionFamily,
    pub loader: String,
    pub mode: PerformanceMode,
    pub tier: CompositionTier,
    #[serde(default)]
    pub mods: Vec<ManagedMod>,
    #[serde(default)]
    pub jvm_preset: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub fallback_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionRequest {
    pub game_version: String,
    pub loader: String,
    pub mode: PerformanceMode,
    pub hardware: HardwareProfile,
    pub installed_mods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: i32,
    pub generated_at: String,
    pub minimum_app_version: String,
    pub rule_channel: String,
    pub artifacts: Vec<ManagedArtifactDefinition>,
    pub compositions: Vec<CompositionDef>,
    pub emergency_disables: Vec<EmergencyDisable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionDef {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub families: Vec<VersionFamily>,
    pub loaders: Vec<String>,
    pub tier: CompositionTier,
    #[serde(default)]
    pub mods: Vec<ManagedMod>,
    #[serde(default)]
    pub jvm_preset: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmergencyDisable {
    pub id: String,
    pub target: EmergencyDisableTarget,
    pub target_id: String,
    pub reason: String,
    #[serde(default)]
    pub families: Vec<VersionFamily>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub tiers: Vec<CompositionTier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HardwareProfile {
    pub total_ram_mb: i32,
    pub logical_cores: i32,
    pub gpu_vendor: String,
    pub gpu_arch: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledMod {
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub role: ManagedArtifactRole,
    pub size: u64,
    pub ownership_class: OwnershipClass,
    pub source: ManagedArtifactSource,
    pub integrity: ManagedArtifactIntegrity,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedDependencyStateEdge {
    pub parent_project_id: String,
    pub child_project_id: String,
    pub child_version_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionState {
    pub composition_id: String,
    pub family: VersionFamily,
    pub tier: CompositionTier,
    pub game_version: String,
    pub loader: String,
    pub graph_sha512: String,
    pub dependency_edges: Vec<ManagedDependencyStateEdge>,
    pub installed_mods: Vec<InstalledMod>,
    pub installed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleHealth {
    Healthy,
    Disabled,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    BuiltIn,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleChannel {
    Bundled,
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulesValidation {
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulesCacheState {
    Recorded,
    Invalid,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesCacheStatus {
    pub recorded: bool,
    pub state: RulesCacheState,
    pub updated_at: Option<String>,
    pub loaded_at: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceRulesStatus {
    pub rule_source: RuleSource,
    pub rule_channel: RuleChannel,
    pub rules_cache: RulesCacheStatus,
    pub schema_version: i32,
    pub generated_at: String,
    pub composition_count: usize,
    pub family_coverage: Vec<FamilyCoverage>,
    pub remote_refresh: bool,
    pub last_refresh_at: Option<String>,
    pub validation: RulesValidation,
    pub health_states: Vec<BundleHealth>,
    pub ownership_classes: Vec<OwnershipClass>,
    pub emergency_disable_count: usize,
    pub emergency_disables: Vec<EmergencyDisableDiagnostic>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyCoverage {
    pub family: VersionFamily,
    pub composition_count: usize,
    pub loaders: Vec<String>,
    pub tiers: Vec<CompositionTier>,
    pub managed_mod_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmergencyDisableDiagnostic {
    pub id: String,
    pub target: EmergencyDisableTarget,
    pub target_id: String,
    pub reason: String,
    pub families: Vec<VersionFamily>,
    pub loaders: Vec<String>,
    pub tiers: Vec<CompositionTier>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformancePlanRequest {
    pub game_version: Option<String>,
    pub loader: Option<String>,
    pub mode: Option<String>,
    pub instance_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformancePlanResponse {
    pub active: bool,
    pub effective: EffectivePerformancePlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewModelTone {
    Ok,
    Warn,
    Err,
    Mute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewModelAction {
    pub command: PerformanceCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PerformanceCommand {
    ApplyPerformancePlan,
    RefreshPerformanceRules,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerformancePlanSummaryViewModel {
    pub state_id: String,
    pub title: String,
    pub detail: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition_id: Option<String>,
    #[serde(default)]
    pub managed_artifact_count: usize,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceRulesStatusResponse {
    #[serde(flatten)]
    pub status: PerformanceRulesStatus,
    pub view_model: PerformanceRulesStatusViewModel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceRulesStatusViewModel {
    pub source_label: String,
    pub channel_label: String,
    pub validation_label: String,
    pub validation_tone: ViewModelTone,
    pub validation_icon: String,
    pub summary: String,
    pub refresh_label: String,
    pub generated_label: String,
    pub cache_label: String,
    pub emergency_disable_label: String,
    pub details_label: String,
    pub health_states_label: String,
    pub ownership_label: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceManagedArtifactSummary {
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub ownership_class: OwnershipClass,
    pub source_provider: ManagedArtifactProvider,
    pub role: ManagedArtifactRole,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeSource {
    Request,
    Instance,
    Global,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedPerformanceMode {
    pub mode: PerformanceMode,
    pub source: ModeSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlanInputError {
    #[error("invalid performance mode")]
    InvalidMode,
    #[error(
        "instance version metadata is unavailable; install the version before resolving performance files"
    )]
    VersionUnavailable,
    #[error("game_version query parameter is required")]
    GameVersionRequired,
}
