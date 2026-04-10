package engine

import (
	"fmt"
	"strings"

	"github.com/mateoltd/croopor/internal/composition"
)

type launchRecovery struct {
	Description string
	Apply       func(*LaunchOptions)
}

func applyRecovery(opts *LaunchOptions, recovery launchRecovery) {
	if recovery.Apply != nil {
		recovery.Apply(opts)
	}
}

func recordRecovery(summary *HealingSummary, recovery launchRecovery) {
	if summary == nil {
		return
	}
	summary.RetryCount++
	summary.FallbackApplied = recovery.Description
}

func recoveryForFailure(class LaunchFailureClass, opts LaunchOptions, ctx *LaunchContext) (launchRecovery, bool) {
	if opts.AdvancedOverrides {
		return launchRecovery{}, false
	}
	switch class {
	case LaunchFailureJVMUnsupportedOption, LaunchFailureJVMExperimentalUnlock, LaunchFailureJVMOptionOrdering:
		if ctx != nil && len(ctx.GCArgs) > 0 {
			preset := conservativeHealingPreset(ctx)
			if preset != "" && preset != ctx.EffectivePreset {
				return launchRecovery{
					Description: fmt.Sprintf("Automatic retry: downgraded JVM preset to %q after startup failure", preset),
					Apply: func(o *LaunchOptions) {
						o.ForcedPreset = preset
						o.DisableCustomGC = false
					},
				}, true
			}
			return launchRecovery{
				Description: "Automatic retry: disabled custom GC flags after startup failure",
				Apply: func(o *LaunchOptions) {
					o.ForcedPreset = ""
					o.DisableCustomGC = true
				},
			}, true
		}
	case LaunchFailureJavaRuntimeMismatch:
		if opts.Config != nil && strings.TrimSpace(opts.Config.JavaPathOverride) != "" {
			return launchRecovery{
				Description: "Automatic retry: switched to managed Java after runtime mismatch",
				Apply: func(o *LaunchOptions) {
					o.ForceManagedJava = true
				},
			}, true
		}
	}
	return launchRecovery{}, false
}

func conservativeHealingPreset(ctx *LaunchContext) string {
	if ctx == nil || !supportsHotSpotTuning(ctx.JavaRuntime.EffectiveInfo) {
		return ""
	}
	family := composition.ClassifyVersion(extractBaseVersion(ctx.Opts.VersionID))
	if ctx.JavaRuntime.EffectiveInfo.Major <= 8 || family == composition.FamilyA || family == composition.FamilyB || family == composition.FamilyC {
		return PresetLegacy
	}
	return PresetPerformance
}
