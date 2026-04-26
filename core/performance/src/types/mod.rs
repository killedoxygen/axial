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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModCondition {
    Always,
    Hardware,
    VersionRange,
    Recommend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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
pub struct ManagedMod {
    pub project_id: String,
    pub slug: String,
    pub name: String,
    pub condition: ModCondition,
    #[serde(default)]
    pub version_range: String,
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
    pub fallback_chain: Vec<String>,
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
pub struct Manifest {
    pub schema_version: i32,
    pub generated_at: String,
    pub compositions: Vec<CompositionDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fallback_to: String,
    #[serde(default)]
    pub jvm_preset: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HardwareProfile {
    pub total_ram_mb: i32,
    pub logical_cores: i32,
    pub gpu_vendor: String,
    pub gpu_arch: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledMod {
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub sha512: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionState {
    pub composition_id: String,
    pub tier: CompositionTier,
    pub installed_mods: Vec<InstalledMod>,
    pub installed_at: String,
    #[serde(default)]
    pub failure_count: i32,
    #[serde(default)]
    pub last_failure: String,
}
