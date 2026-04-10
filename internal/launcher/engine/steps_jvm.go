package engine

import (
	"fmt"
	"strings"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/system"
)

type prepareCDSStep struct{}

func (s *prepareCDSStep) Name() string { return "prepare CDS" }

func (s *prepareCDSStep) Execute(ctx *LaunchContext) error {
	if !ctx.IsModded && ctx.EffectiveJavaMajor >= 11 && CDSArchiveExists(ctx.ConfigDir, ctx.Opts.VersionID) && !usesNativeAccessProperty(ctx.JVMArgs) {
		ctx.CDSArgs = []string{
			"-Xshare:auto",
			"-XX:SharedArchiveFile=" + CDSArchivePath(ctx.ConfigDir, ctx.Opts.VersionID),
		}
	}
	return nil
}

func usesNativeAccessProperty(args []string) bool {
	for _, arg := range args {
		if strings.HasPrefix(arg, "-Djdk.module.enable.native.access=") {
			return true
		}
	}
	return false
}

type computeMemoryStep struct{}

func (s *computeMemoryStep) Name() string { return "compute memory" }

func (s *computeMemoryStep) Execute(ctx *LaunchContext) error {
	hw := system.Detect()
	recMin, recMax := recommendedMemoryForLaunch(extractBaseVersion(ctx.Opts.VersionID), ctx.IsModded, hw)
	maxMem := ctx.Opts.MaxMemoryMB
	if maxMem <= 0 {
		maxMem = recMax
	}
	minMem := ctx.Opts.MinMemoryMB
	if minMem <= 0 {
		minMem = recMin
	}
	if minMem > maxMem {
		minMem = maxMem
	}
	ctx.EffectiveMaxMemoryMB = maxMem
	ctx.EffectiveMinMemoryMB = minMem
	ctx.MemArgs = []string{
		fmt.Sprintf("-Xmx%dM", maxMem),
		fmt.Sprintf("-Xms%dM", minMem),
	}
	return nil
}

type applyBootThrottleStep struct{}

func (s *applyBootThrottleStep) Name() string { return "apply boot throttle" }

func (s *applyBootThrottleStep) Execute(ctx *LaunchContext) error {
	ctx.BootArgs = bootThrottleArgs(ctx.EffectiveJavaMajor)
	return nil
}

type applyGCPresetStep struct{}

func (s *applyGCPresetStep) Name() string { return "apply GC preset" }

func (s *applyGCPresetStep) Execute(ctx *LaunchContext) error {
	if ctx.Opts.DisableCustomGC {
		ctx.EffectivePreset = ""
		ctx.GCArgs = nil
		return nil
	}
	preset := ctx.Opts.ForcedPreset
	if preset == "" {
		preset = ctx.Opts.Config.JVMPreset
	}
	family := composition.ClassifyVersion(extractBaseVersion(ctx.Opts.VersionID))
	loader := inferLoader(ctx)
	if preset == "" {
		preset = autoSelectPresetForLaunch(system.Detect(), family, loader, ctx.IsModded, ctx.JavaRuntime.EffectiveInfo)
	}
	preset = sanitizePresetForLaunch(preset, family, loader, ctx.IsModded, ctx.JavaRuntime.EffectiveInfo)
	ctx.EffectivePreset = preset
	ctx.GCArgs = gcPresetArgs(preset, ctx.JavaRuntime.EffectiveInfo)
	return nil
}

type applyCompositionJVMStep struct{}

func (s *applyCompositionJVMStep) Name() string { return "apply composition JVM" }

func (s *applyCompositionJVMStep) Execute(ctx *LaunchContext) error {
	if ctx.Opts.DisableCustomGC || ctx.Opts.ForcedPreset != "" {
		return nil
	}
	if ctx.CompositionPlan == nil || ctx.Opts.Config.JVMPreset != "" {
		return nil
	}
	if ctx.CompositionPlan.JVMPreset != "" {
		family := composition.ClassifyVersion(extractBaseVersion(ctx.Opts.VersionID))
		loader := inferLoader(ctx)
		preset := sanitizePresetForLaunch(ctx.CompositionPlan.JVMPreset, family, loader, ctx.IsModded, ctx.JavaRuntime.EffectiveInfo)
		ctx.EffectivePreset = preset
		ctx.GCArgs = gcPresetArgs(preset, ctx.JavaRuntime.EffectiveInfo)
	}
	return nil
}

func recommendedMemoryForLaunch(versionID string, isModded bool, hw system.HardwareProfile) (minMem, maxMem int) {
	totalMB := hw.TotalRAMMB
	if totalMB <= 0 {
		totalMB = 8192
	}
	baseMin, baseMax := system.RecommendedMemoryRange(totalMB)
	family := composition.ClassifyVersion(versionID)

	switch family {
	case composition.FamilyA, composition.FamilyB:
		minMem = 1024
		if hw.Tier == system.HardwareTierLow {
			minMem = 768
		}
		maxMem = minInt(baseMax, 2048)
	case composition.FamilyC:
		minMem = 1024
		if hw.Tier != system.HardwareTierLow {
			minMem = 1536
		}
		maxMem = minInt(baseMax, 4096)
		if isModded && hw.Tier == system.HardwareTierHigh {
			maxMem = minInt(baseMax, 6144)
		}
	case composition.FamilyD:
		minMem = 1536
		if hw.Tier != system.HardwareTierLow {
			minMem = 2048
		}
		maxMem = minInt(baseMax, 6144)
	default:
		minMem = maxInt(baseMin, 2048)
		maxMem = baseMax
		if isModded && hw.Tier == system.HardwareTierHigh {
			maxMem = maxInt(maxMem, 6144)
		}
	}

	if maxMem > totalMB-2048 {
		maxMem = totalMB - 2048
	}
	if maxMem < 1024 {
		maxMem = 1024
	}
	if minMem > maxMem {
		minMem = maxMem
	}
	if minMem < 512 {
		minMem = 512
	}
	return minMem, maxMem
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}
