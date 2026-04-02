package composition

import "github.com/mateoltd/croopor/internal/system"

type VersionFamily string

const (
	FamilyA VersionFamily = "A"
	FamilyB VersionFamily = "B"
	FamilyC VersionFamily = "C"
	FamilyD VersionFamily = "D"
	FamilyE VersionFamily = "E"
	FamilyF VersionFamily = "F"
)

// CompositionMode is the user-selected performance strategy for an instance.
type CompositionMode string

const (
	ModeManaged CompositionMode = "managed"
	ModeVanilla CompositionMode = "vanilla"
	ModeCustom  CompositionMode = "custom"
)

// CompositionTier is the quality level of the resolved composition.
type CompositionTier string

const (
	TierExtended        CompositionTier = "extended"
	TierCore            CompositionTier = "core"
	TierVanillaEnhanced CompositionTier = "vanilla_enhanced"
)

// ModCondition controls when a managed mod is included.
type ModCondition string

const (
	ConditionAlways       ModCondition = "always"
	ConditionHardware     ModCondition = "hardware"
	ConditionVersionRange ModCondition = "version_range"
	ConditionRecommend    ModCondition = "recommend"
)

// HardwareRequirement gates a hardware-conditional mod.
type HardwareRequirement struct {
	GPUVendor  system.GPUVendor  `json:"gpu_vendor,omitempty"`
	GPUArchMin system.NVIDIAArch `json:"gpu_arch_min,omitempty"`
	MinRAMMB   int               `json:"min_ram_mb,omitempty"`
	MinCores   int               `json:"min_cores,omitempty"`
}

// ManagedMod is a mod entry in the resolved composition.
type ManagedMod struct {
	ProjectID        string               `json:"project_id"`
	Slug             string               `json:"slug"`
	Name             string               `json:"name"`
	Condition        ModCondition         `json:"condition"`
	VersionRange     string               `json:"version_range,omitempty"`
	HardwareReq      *HardwareRequirement `json:"hardware_req,omitempty"`
	MutualExclusions []string             `json:"mutual_exclusions,omitempty"`
}

// CompositionPlan is the fully resolved output of the composition engine.
type CompositionPlan struct {
	CompositionID  string          `json:"composition_id"`
	Family         VersionFamily   `json:"family"`
	Loader         string          `json:"loader"`
	Mode           CompositionMode `json:"mode"`
	Tier           CompositionTier `json:"tier"`
	Mods           []ManagedMod    `json:"mods"`
	JVMPreset      string          `json:"jvm_preset,omitempty"`
	FallbackChain  []string        `json:"fallback_chain,omitempty"`
	Warnings       []string        `json:"warnings,omitempty"`
	FallbackReason string          `json:"fallback_reason,omitempty"`
}

// ResolutionRequest is the input to Resolve.
type ResolutionRequest struct {
	GameVersion   string
	Loader        string
	Mode          CompositionMode
	Hardware      system.HardwareProfile
	InstalledMods []string
}

type Manifest struct {
	SchemaVersion int              `json:"schema_version"`
	GeneratedAt   string           `json:"generated_at"`
	Compositions  []CompositionDef `json:"compositions"`
}

type CompositionDef struct {
	ID          string          `json:"id"`
	DisplayName string          `json:"display_name"`
	Description string          `json:"description"`
	Families    []VersionFamily `json:"families"`
	Loaders     []string        `json:"loaders"`
	Tier        CompositionTier `json:"tier"`
	Mods        []ManagedMod    `json:"mods"`
	FallbackTo  string          `json:"fallback_to,omitempty"`
	JVMPreset   string          `json:"jvm_preset,omitempty"`
}

// ClassifyVersion returns the VersionFamily for a Minecraft version string.
func ClassifyVersion(mcVersion string) VersionFamily {
	v, err := Parse(mcVersion)
	if err != nil {
		return FamilyF
	}
	if v.IsSnapshot {
		return FamilyF
	}

	switch {
	case compareReleaseVersion(v, 1, 6, 0) < 0:
		return FamilyA
	case compareReleaseVersion(v, 1, 7, 10) <= 0:
		return FamilyB
	case compareReleaseVersion(v, 1, 12, 2) <= 0:
		return FamilyC
	case compareReleaseVersion(v, 1, 15, 2) <= 0:
		return FamilyD
	case compareReleaseVersion(v, 1, 20, 1) <= 0:
		return FamilyE
	default:
		return FamilyF
	}
}

func compareReleaseVersion(v MCVersion, major, minor, patch int) int {
	target := MCVersion{Major: major, Minor: minor, Patch: patch}
	return v.Compare(target)
}
