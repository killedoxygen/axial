package engine

import (
	"testing"
)

func TestMatchVersions(t *testing.T) {
	t.Parallel()
	match := matchVersions("1.16.4", "1.16.5")

	tests := []struct {
		version string
		want    bool
	}{
		{"1.16.4", true},
		{"1.16.5", true},
		{"1.16.3", false},
		{"1.16", false},
		{"1.17", false},
		{"1.17.1", false},
		{"not-a-version", false},
		{"", false},
	}

	for _, tt := range tests {
		if got := match(versionOverrideContext{BaseVersion: tt.version}); got != tt.want {
			t.Errorf("matchVersions(1.16.4, 1.16.5)(%q) = %v, want %v", tt.version, got, tt.want)
		}
	}
}

func TestMatchAuthModes(t *testing.T) {
	t.Parallel()
	match := matchAuthModes(LaunchAuthOffline)
	if !match(versionOverrideContext{AuthMode: LaunchAuthOffline}) {
		t.Fatal("expected offline auth mode to match")
	}
	if match(versionOverrideContext{AuthMode: LaunchAuthAuthenticated}) {
		t.Fatal("expected authenticated auth mode not to match offline override")
	}
}

func TestApplyVersionOverridesStep_Matching(t *testing.T) {
	t.Parallel()
	ctx := &LaunchContext{
		Opts:     LaunchOptions{VersionID: "1.16.4"},
		AuthMode: LaunchAuthOffline,
	}
	step := &applyVersionOverridesStep{}
	if err := step.Execute(ctx); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(ctx.JVMArgs) != 5 {
		t.Fatalf("expected 5 override args, got %d: %v", len(ctx.JVMArgs), ctx.JVMArgs)
	}
	if ctx.JVMArgs[0] != "-Dminecraft.api.env=custom" {
		t.Errorf("first arg = %q, want -Dminecraft.api.env=custom", ctx.JVMArgs[0])
	}
}

func TestApplyVersionOverridesStep_ModdedMatching(t *testing.T) {
	t.Parallel()
	ctx := &LaunchContext{
		Opts:     LaunchOptions{VersionID: "fabric-loader-0.14.21-1.16.5"},
		AuthMode: LaunchAuthOffline,
	}
	step := &applyVersionOverridesStep{}
	if err := step.Execute(ctx); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(ctx.JVMArgs) != 5 {
		t.Fatalf("expected 5 override args for modded 1.16.5, got %d", len(ctx.JVMArgs))
	}
}

func TestApplyVersionOverridesStep_NonMatching(t *testing.T) {
	t.Parallel()
	ctx := &LaunchContext{
		Opts:     LaunchOptions{VersionID: "1.17.1"},
		AuthMode: LaunchAuthOffline,
	}
	step := &applyVersionOverridesStep{}
	if err := step.Execute(ctx); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(ctx.JVMArgs) != 0 {
		t.Fatalf("expected no override args for 1.17.1, got %d: %v", len(ctx.JVMArgs), ctx.JVMArgs)
	}
}

func TestApplyVersionOverridesStep_PreservesExisting(t *testing.T) {
	t.Parallel()
	ctx := &LaunchContext{
		Opts:     LaunchOptions{VersionID: "1.16.4"},
		AuthMode: LaunchAuthOffline,
		JVMArgs:  []string{"-Dfoo=bar"},
	}
	step := &applyVersionOverridesStep{}
	if err := step.Execute(ctx); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(ctx.JVMArgs) != 6 {
		t.Fatalf("expected 6 args (1 existing + 5 overrides), got %d", len(ctx.JVMArgs))
	}
	if ctx.JVMArgs[0] != "-Dfoo=bar" {
		t.Errorf("existing arg not preserved at index 0: %q", ctx.JVMArgs[0])
	}
}

func TestApplyVersionOverridesStep_AuthenticatedLaunchSkipsOfflineOverride(t *testing.T) {
	t.Parallel()
	ctx := &LaunchContext{
		Opts:     LaunchOptions{VersionID: "1.16.4"},
		AuthMode: LaunchAuthAuthenticated,
	}
	step := &applyVersionOverridesStep{}
	if err := step.Execute(ctx); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(ctx.JVMArgs) != 0 {
		t.Fatalf("expected no override args for authenticated launch, got %d: %v", len(ctx.JVMArgs), ctx.JVMArgs)
	}
}
