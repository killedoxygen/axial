package composition

import (
	"fmt"
	"strings"

	"github.com/mateoltd/croopor/internal/system"
)

// Resolve is a pure function. It has no side effects, performs no I/O.
func Resolve(m *Manifest, req ResolutionRequest) *CompositionPlan {
	family := ClassifyVersion(req.GameVersion)
	mode := req.Mode
	if mode == "" {
		mode = ModeManaged
	}
	loader := strings.ToLower(strings.TrimSpace(req.Loader))
	if loader == "" {
		loader = "vanilla"
	}

	if mode == ModeVanilla || mode == ModeCustom {
		return &CompositionPlan{
			Family: family,
			Loader: loader,
			Mode:   mode,
			Tier:   TierVanillaEnhanced,
		}
	}

	if m == nil {
		return &CompositionPlan{
			Family: family,
			Loader: loader,
			Mode:   mode,
			Tier:   TierVanillaEnhanced,
		}
	}

	installedSet := make(map[string]struct{}, len(req.InstalledMods))
	for _, id := range req.InstalledMods {
		installedSet[strings.ToLower(id)] = struct{}{}
	}

	for _, tier := range []CompositionTier{TierExtended, TierCore, TierVanillaEnhanced} {
		def := findComposition(m, family, loader, tier)
		if def == nil {
			continue
		}

		activeMods := make([]ManagedMod, 0, len(def.Mods))
		warnings := make([]string, 0)
		for _, mod := range def.Mods {
			include, warning := shouldIncludeMod(mod, req.GameVersion, req.Hardware, installedSet)
			if warning != "" {
				warnings = append(warnings, warning)
			}
			if include {
				activeMods = append(activeMods, mod)
			}
		}

		if len(activeMods) >= 2 || tier == TierVanillaEnhanced {
			plan := &CompositionPlan{
				CompositionID: def.ID,
				Family:        family,
				Loader:        loader,
				Mode:          mode,
				Tier:          def.Tier,
				Mods:          activeMods,
				JVMPreset:     def.JVMPreset,
				FallbackChain: fallbackChain(m, def.ID),
				Warnings:      warnings,
			}
			if tier != TierExtended {
				plan.FallbackReason = "higher-tier managed composition is unavailable for this combination"
			}
			return plan
		}
	}

	return &CompositionPlan{
		Family: family,
		Loader: loader,
		Mode:   mode,
		Tier:   TierVanillaEnhanced,
	}
}

func findComposition(m *Manifest, family VersionFamily, loader string, tier CompositionTier) *CompositionDef {
	for i := range m.Compositions {
		def := &m.Compositions[i]
		if def.Tier != tier {
			continue
		}
		if !containsFamily(def.Families, family) {
			continue
		}
		if !containsFold(def.Loaders, loader) {
			continue
		}
		return def
	}
	return nil
}

func fallbackChain(m *Manifest, startID string) []string {
	var chain []string
	seen := map[string]struct{}{}
	current := startID
	for current != "" {
		if _, exists := seen[current]; exists {
			break
		}
		seen[current] = struct{}{}
		def := getCompositionByID(m, current)
		if def == nil || def.FallbackTo == "" {
			break
		}
		chain = append(chain, def.FallbackTo)
		current = def.FallbackTo
	}
	return chain
}

func getCompositionByID(m *Manifest, id string) *CompositionDef {
	for i := range m.Compositions {
		if m.Compositions[i].ID == id {
			return &m.Compositions[i]
		}
	}
	return nil
}

func shouldIncludeMod(mod ManagedMod, gameVersion string, hw system.HardwareProfile, installed map[string]struct{}) (bool, string) {
	switch mod.Condition {
	case ConditionAlways:
	case ConditionVersionRange:
		v, err := Parse(gameVersion)
		if err != nil || !v.InRange(mod.VersionRange) {
			return false, ""
		}
	case ConditionHardware:
		if ok, warning := satisfiesHardware(mod, hw); !ok {
			return false, warning
		}
	case ConditionRecommend:
		return false, ""
	default:
		return false, ""
	}

	for _, exclusion := range mod.MutualExclusions {
		if _, exists := installed[strings.ToLower(exclusion)]; exists {
			return false, fmt.Sprintf("%s skipped: incompatible with managed mod %s", mod.Slug, exclusion)
		}
	}
	return true, ""
}

func satisfiesHardware(mod ManagedMod, hw system.HardwareProfile) (bool, string) {
	req := mod.HardwareReq
	if req == nil {
		return true, ""
	}
	if req.GPUVendor != "" && hw.GPU.Vendor != req.GPUVendor {
		if req.GPUVendor == system.GPUVendorNVIDIA {
			return false, fmt.Sprintf("%s skipped: no NVIDIA Turing+ GPU detected", mod.Slug)
		}
		return false, fmt.Sprintf("%s skipped: unsupported GPU vendor", mod.Slug)
	}
	if req.GPUArchMin > 0 && hw.GPU.NVArch < req.GPUArchMin {
		return false, fmt.Sprintf("%s skipped: no NVIDIA Turing+ GPU detected", mod.Slug)
	}
	if req.MinRAMMB > 0 && hw.TotalRAMMB < req.MinRAMMB {
		return false, fmt.Sprintf("%s skipped: not enough system RAM", mod.Slug)
	}
	if req.MinCores > 0 && hw.CPU.LogicalCores < req.MinCores {
		return false, fmt.Sprintf("%s skipped: not enough CPU cores", mod.Slug)
	}
	return true, ""
}

func containsFamily(families []VersionFamily, family VersionFamily) bool {
	for _, candidate := range families {
		if candidate == family {
			return true
		}
	}
	return false
}

func containsFold(values []string, target string) bool {
	for _, candidate := range values {
		if strings.EqualFold(candidate, target) {
			return true
		}
	}
	return false
}
