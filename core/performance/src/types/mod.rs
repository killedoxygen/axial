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
