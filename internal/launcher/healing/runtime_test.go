package healing

import (
	"testing"

	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/system"
)

func TestResolveRuntimeBypassesIncompatibleOverride(t *testing.T) {
	required := minecraft.JavaVersion{MajorVersion: 25, Component: "java-runtime-delta"}
	overridePath := "/runtimes/jre-legacy/bin/java"
	managedPath := "/runtimes/java-runtime-delta/bin/java"

	selection, err := ResolveRuntime(required, overridePath, false, func(path string) (*minecraft.JavaResult, system.JavaRuntimeInfo, error) {
		switch path {
		case overridePath:
			return &minecraft.JavaResult{Path: overridePath, Component: "jre-legacy", Source: "override"}, system.JavaRuntimeInfo{Major: 8, Update: 402, Distribution: system.JavaDistributionOpenJDK}, nil
		case "":
			return &minecraft.JavaResult{Path: managedPath, Component: "java-runtime-delta", Source: "croopor"}, system.JavaRuntimeInfo{Major: 25, Distribution: system.JavaDistributionOpenJDK}, nil
		default:
			t.Fatalf("unexpected resolver path %q", path)
			return nil, system.JavaRuntimeInfo{}, nil
		}
	})
	if err != nil {
		t.Fatalf("ResolveRuntime returned error: %v", err)
	}

	if selection.RequestedPath != overridePath {
		t.Fatalf("expected requested path %q, got %q", overridePath, selection.RequestedPath)
	}
	if selection.SelectedPath != overridePath {
		t.Fatalf("expected selected path %q, got %q", overridePath, selection.SelectedPath)
	}
	if selection.EffectivePath != managedPath {
		t.Fatalf("expected effective path %q, got %q", managedPath, selection.EffectivePath)
	}
	if selection.EffectiveInfo.Major != 25 {
		t.Fatalf("expected effective Java 25, got %d", selection.EffectiveInfo.Major)
	}
	if !selection.BypassedRequestedRuntime {
		t.Fatal("expected requested runtime to be bypassed")
	}
}

func TestResolveRuntimeForceManagedKeepsRequestedInput(t *testing.T) {
	required := minecraft.JavaVersion{MajorVersion: 25, Component: "java-runtime-delta"}
	overridePath := "/runtimes/jre-legacy/bin/java"
	managedPath := "/runtimes/java-runtime-delta/bin/java"

	selection, err := ResolveRuntime(required, overridePath, true, func(path string) (*minecraft.JavaResult, system.JavaRuntimeInfo, error) {
		if path != "" {
			t.Fatalf("expected managed resolution only, got %q", path)
		}
		return &minecraft.JavaResult{Path: managedPath, Component: "java-runtime-delta", Source: "croopor"}, system.JavaRuntimeInfo{Major: 25, Distribution: system.JavaDistributionOpenJDK}, nil
	})
	if err != nil {
		t.Fatalf("ResolveRuntime returned error: %v", err)
	}

	if selection.RequestedPath != overridePath {
		t.Fatalf("expected requested path %q, got %q", overridePath, selection.RequestedPath)
	}
	if selection.SelectedPath != "" {
		t.Fatalf("expected no selected path probe, got %q", selection.SelectedPath)
	}
	if selection.EffectivePath != managedPath {
		t.Fatalf("expected effective path %q, got %q", managedPath, selection.EffectivePath)
	}
	if !selection.BypassedRequestedRuntime {
		t.Fatal("expected requested runtime to be bypassed")
	}
}
