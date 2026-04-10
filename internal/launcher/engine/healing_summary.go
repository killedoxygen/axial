package engine

import (
	"errors"
	"fmt"
	"strings"
	"time"
)

const startupObservationWindow = 5 * time.Second

type HealingSummary struct {
	RequestedPreset   string             `json:"requested_preset,omitempty"`
	EffectivePreset   string             `json:"effective_preset,omitempty"`
	RequestedJavaPath string             `json:"requested_java_path,omitempty"`
	EffectiveJavaPath string             `json:"effective_java_path,omitempty"`
	AuthMode          LaunchAuthMode     `json:"auth_mode,omitempty"`
	Warnings          []string           `json:"warnings,omitempty"`
	FallbackApplied   string             `json:"fallback_applied,omitempty"`
	RetryCount        int                `json:"retry_count,omitempty"`
	FailureClass      LaunchFailureClass `json:"failure_class,omitempty"`
	AdvancedOverrides bool               `json:"advanced_overrides,omitempty"`
}

type LaunchError struct {
	Message string
	Healing HealingSummary
}

func (e *LaunchError) Error() string { return e.Message }

type resolveHealingStep struct{}

func (s *resolveHealingStep) Name() string { return "resolve healing" }

func (s *resolveHealingStep) Execute(ctx *LaunchContext) error {
	if ctx.Healing == nil {
		summary := newHealingSummary(ctx.Opts)
		ctx.Healing = &summary
	}

	ctx.Healing.EffectivePreset = ctx.EffectivePreset
	ctx.Healing.EffectiveJavaPath = ctx.JavaRuntime.EffectivePath
	ctx.Healing.AuthMode = ctx.AuthMode
	if ctx.Healing.RequestedPreset == "" && ctx.CompositionPlan != nil && ctx.CompositionPlan.JVMPreset != "" {
		ctx.Healing.RequestedPreset = ctx.CompositionPlan.JVMPreset
	}

	if ctx.Healing.RequestedPreset != "" && ctx.Healing.RequestedPreset != ctx.EffectivePreset {
		appendHealingWarning(ctx.Healing, fmt.Sprintf("Requested JVM preset %q was downgraded to %q for compatibility", ctx.Healing.RequestedPreset, blankAsNone(ctx.EffectivePreset)))
	}
	if ctx.Healing.RequestedJavaPath != "" && ctx.Healing.RequestedJavaPath != ctx.JavaRuntime.EffectivePath {
		appendHealingWarning(ctx.Healing, "Requested Java override was bypassed in favor of a safer managed runtime")
	}

	if ctx.Opts.AdvancedOverrides {
		if err := validateManualJavaOverride(ctx); err != nil {
			return err
		}
		if err := validateManualJVMArgs(ctx.Opts.ExtraJVMArgs, ctx.JavaRuntime.EffectiveInfo); err != nil {
			return err
		}
	}

	return nil
}

func newHealingSummary(opts LaunchOptions) HealingSummary {
	authMode := opts.AuthMode
	if authMode == "" {
		authMode = LaunchAuthOffline
	}
	summary := HealingSummary{
		AuthMode:          authMode,
		AdvancedOverrides: opts.AdvancedOverrides,
	}
	if opts.Config != nil {
		summary.RequestedPreset = strings.TrimSpace(opts.Config.JVMPreset)
		summary.RequestedJavaPath = strings.TrimSpace(opts.Config.JavaPathOverride)
	}
	return summary
}

func resetHealingAttempt(summary *HealingSummary, opts LaunchOptions) {
	if summary == nil {
		return
	}
	authMode := opts.AuthMode
	if authMode == "" {
		authMode = LaunchAuthOffline
	}
	summary.AuthMode = authMode
	summary.EffectivePreset = ""
	summary.EffectiveJavaPath = ""
	summary.Warnings = nil
	summary.FailureClass = ""
}

func newLaunchError(err error, healing HealingSummary) error {
	if err == nil {
		err = errors.New("launch failed")
	}
	return &LaunchError{
		Message: err.Error(),
		Healing: healing,
	}
}

func formatFailureClass(class LaunchFailureClass) string {
	switch class {
	case LaunchFailureJVMUnsupportedOption:
		return "unsupported JVM option"
	case LaunchFailureJVMExperimentalUnlock:
		return "experimental JVM option requires unlock"
	case LaunchFailureJVMOptionOrdering:
		return "JVM option ordering conflict"
	case LaunchFailureJavaRuntimeMismatch:
		return "Java runtime mismatch"
	case LaunchFailureClasspathModuleConflict:
		return "classpath or module conflict"
	case LaunchFailureAuthModeIncompatible:
		return "auth mode incompatibility"
	case LaunchFailureLoaderBootstrapFailure:
		return "loader bootstrap failure"
	default:
		return "unknown startup failure"
	}
}

func appendHealingWarning(summary *HealingSummary, warning string) {
	warning = strings.TrimSpace(warning)
	if summary == nil || warning == "" {
		return
	}
	for _, existing := range summary.Warnings {
		if existing == warning {
			return
		}
	}
	summary.Warnings = append(summary.Warnings, warning)
}

func blankAsNone(v string) string {
	if strings.TrimSpace(v) == "" {
		return "none"
	}
	return v
}
