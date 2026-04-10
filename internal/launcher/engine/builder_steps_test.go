package engine

import (
	"testing"

	"github.com/mateoltd/croopor/internal/config"
	launchruntime "github.com/mateoltd/croopor/internal/launcher/runtime"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/system"
)

func TestApplyGCPresetUsesEffectiveRuntime(t *testing.T) {
	ctx := &LaunchContext{
		Opts: LaunchOptions{
			VersionID: "1.26.1.2",
			Config: &config.Config{
				JavaPathOverride: "/runtimes/jre-legacy/bin/java",
				JVMPreset:        PresetPerformance,
			},
		},
		JavaRuntime: launchruntime.RuntimeSelection{
			RequestedPath: "/runtimes/jre-legacy/bin/java",
			EffectivePath: "/runtimes/java-runtime-delta/bin/java",
			EffectiveInfo: system.JavaRuntimeInfo{
				Distribution: system.JavaDistributionOpenJDK,
				Major:        25,
			},
		},
	}

	if err := (&applyGCPresetStep{}).Execute(ctx); err != nil {
		t.Fatalf("applyGCPresetStep returned error: %v", err)
	}

	if ctx.EffectivePreset != PresetPerformance {
		t.Fatalf("expected effective preset %q, got %q", PresetPerformance, ctx.EffectivePreset)
	}
	if len(ctx.GCArgs) == 0 {
		t.Fatal("expected GC args to be applied")
	}
}

func TestApplyGCPresetDoesNotDowngradeManagedRuntimeWhenProbeMajorIsUnknown(t *testing.T) {
	ctx := &LaunchContext{
		Version: &minecraft.VersionJSON{
			JavaVersion: minecraft.JavaVersion{
				Component:    "java-runtime-epsilon",
				MajorVersion: 25,
			},
		},
		Opts: LaunchOptions{
			VersionID: "26.1.2",
			Config: &config.Config{
				JavaPathOverride: "/runtimes/jre-legacy/bin/java",
				JVMPreset:        PresetPerformance,
			},
		},
	}

	step := &resolveJavaStep{}
	origResolveJavaRuntime := resolveJavaRuntime
	defer func() { resolveJavaRuntime = origResolveJavaRuntime }()

	resolveJavaRuntime = func(mcDir string, javaVersion minecraft.JavaVersion, overridePath string) (*minecraft.JavaResult, system.JavaRuntimeInfo, error) {
		if overridePath == "" {
			return &minecraft.JavaResult{Path: "/runtimes/java-runtime-epsilon/bin/java", Component: "java-runtime-epsilon", Source: "croopor"}, system.JavaRuntimeInfo{}, nil
		}
		return &minecraft.JavaResult{Path: overridePath, Component: "jre-legacy", Source: "override"}, system.JavaRuntimeInfo{
			Distribution: system.JavaDistributionOpenJDK,
			Major:        8,
			Update:       402,
		}, nil
	}

	if err := step.Execute(ctx); err != nil {
		t.Fatalf("resolveJavaStep returned error: %v", err)
	}
	if ctx.JavaRuntime.EffectiveInfo.Major != 25 {
		t.Fatalf("expected effective Java 25 fallback, got %d", ctx.JavaRuntime.EffectiveInfo.Major)
	}

	if err := (&applyGCPresetStep{}).Execute(ctx); err != nil {
		t.Fatalf("applyGCPresetStep returned error: %v", err)
	}
	if ctx.EffectivePreset != PresetPerformance {
		t.Fatalf("expected effective preset %q, got %q", PresetPerformance, ctx.EffectivePreset)
	}
}
