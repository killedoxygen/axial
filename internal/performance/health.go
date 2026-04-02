package performance

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/mateoltd/croopor/internal/composition"
)

type BundleHealth string

const (
	HealthHealthy  BundleHealth = "healthy"
	HealthDegraded BundleHealth = "degraded"
	HealthFallback BundleHealth = "fallback"
	HealthDisabled BundleHealth = "disabled"
	HealthInvalid  BundleHealth = "invalid"
)

// DeriveHealth checks the lock file state against actual files on disk.
func DeriveHealth(state *CompositionState, plan *composition.CompositionPlan, instanceModsDir string) (BundleHealth, []string) {
	if state == nil {
		return HealthDisabled, nil
	}

	warnings := make([]string, 0)
	for _, mod := range state.InstalledMods {
		if _, err := os.Stat(filepath.Join(instanceModsDir, mod.Filename)); err != nil {
			warnings = append(warnings, fmt.Sprintf("%s missing from mods folder", mod.Filename))
		}
	}
	if len(warnings) > 0 {
		return HealthInvalid, warnings
	}

	if plan != nil && plan.Tier != "" && state.Tier != "" && tierRank(state.Tier) < tierRank(plan.Tier) {
		return HealthDegraded, append(warnings, "managed composition resolved to a lower tier than expected")
	}

	return HealthHealthy, warnings
}

func tierRank(tier composition.CompositionTier) int {
	switch tier {
	case composition.TierExtended:
		return 3
	case composition.TierCore:
		return 2
	case composition.TierVanillaEnhanced:
		return 1
	default:
		return 0
	}
}
