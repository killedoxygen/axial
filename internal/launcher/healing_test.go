package launcher

import "testing"

func TestResetHealingAttemptClearsAttemptScopedState(t *testing.T) {
	summary := HealingSummary{
		RequestedPreset:   PresetPerformance,
		EffectivePreset:   PresetLegacy,
		RequestedJavaPath: "C:/java/jre-legacy/bin/javaw.exe",
		EffectiveJavaPath: "C:/java/runtime/bin/javaw.exe",
		AuthMode:          LaunchAuthOffline,
		Warnings:          []string{"stale warning"},
		FallbackApplied:   "Automatic retry: switched to managed Java after runtime mismatch",
		RetryCount:        1,
		FailureClass:      LaunchFailureJavaRuntimeMismatch,
		AdvancedOverrides: true,
	}

	resetHealingAttempt(&summary, LaunchOptions{AuthMode: LaunchAuthAuthenticated})

	if summary.RequestedPreset != PresetPerformance {
		t.Fatalf("requested preset changed: got %q", summary.RequestedPreset)
	}
	if summary.RequestedJavaPath != "C:/java/jre-legacy/bin/javaw.exe" {
		t.Fatalf("requested java path changed: got %q", summary.RequestedJavaPath)
	}
	if summary.EffectivePreset != "" {
		t.Fatalf("effective preset was not cleared: got %q", summary.EffectivePreset)
	}
	if summary.EffectiveJavaPath != "" {
		t.Fatalf("effective java path was not cleared: got %q", summary.EffectiveJavaPath)
	}
	if len(summary.Warnings) != 0 {
		t.Fatalf("warnings were not cleared: got %v", summary.Warnings)
	}
	if summary.FallbackApplied != "Automatic retry: switched to managed Java after runtime mismatch" {
		t.Fatalf("fallback changed: got %q", summary.FallbackApplied)
	}
	if summary.RetryCount != 1 {
		t.Fatalf("retry count changed: got %d", summary.RetryCount)
	}
	if summary.FailureClass != "" {
		t.Fatalf("failure class was not cleared: got %q", summary.FailureClass)
	}
	if summary.AuthMode != LaunchAuthAuthenticated {
		t.Fatalf("auth mode not refreshed: got %q", summary.AuthMode)
	}
	if !summary.AdvancedOverrides {
		t.Fatal("advanced overrides changed")
	}
}

func TestRecordRecoveryKeepsFallbackSeparateFromWarnings(t *testing.T) {
	summary := HealingSummary{}
	recovery := launchRecovery{Description: "Automatic retry: switched to managed Java after runtime mismatch"}

	recordRecovery(&summary, recovery)

	if summary.RetryCount != 1 {
		t.Fatalf("expected retry count 1, got %d", summary.RetryCount)
	}
	if summary.FallbackApplied != recovery.Description {
		t.Fatalf("expected fallback %q, got %q", recovery.Description, summary.FallbackApplied)
	}
	if len(summary.Warnings) != 0 {
		t.Fatalf("expected no warnings, got %v", summary.Warnings)
	}
}
