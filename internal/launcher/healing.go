package launcher

import (
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/system"
)

const startupObservationWindow = 5 * time.Second

type LaunchFailureClass string

const (
	LaunchFailureUnknown                 LaunchFailureClass = "unknown"
	LaunchFailureJVMUnsupportedOption    LaunchFailureClass = "jvm_unsupported_option"
	LaunchFailureJVMExperimentalUnlock   LaunchFailureClass = "jvm_experimental_unlock_required"
	LaunchFailureJVMOptionOrdering       LaunchFailureClass = "jvm_option_ordering"
	LaunchFailureJavaRuntimeMismatch     LaunchFailureClass = "java_runtime_mismatch"
	LaunchFailureClasspathModuleConflict LaunchFailureClass = "classpath_or_module_conflict"
	LaunchFailureAuthModeIncompatible    LaunchFailureClass = "auth_mode_incompatible"
	LaunchFailureLoaderBootstrapFailure  LaunchFailureClass = "loader_bootstrap_failure"
)

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

type launchValidationError struct {
	Class   LaunchFailureClass
	Message string
}

func (e *launchValidationError) Error() string { return e.Message }

type launchRecovery struct {
	Description string
	Apply       func(*LaunchOptions)
}

type startupOutcome int

const (
	startupStable startupOutcome = iota
	startupExited
	startupTimedOut
)

type resolveHealingStep struct{}

func (s *resolveHealingStep) Name() string { return "resolve healing" }

func (s *resolveHealingStep) Execute(ctx *LaunchContext) error {
	if ctx.Healing == nil {
		summary := newHealingSummary(ctx.Opts)
		ctx.Healing = &summary
	}

	ctx.Healing.EffectivePreset = ctx.EffectivePreset
	ctx.Healing.EffectiveJavaPath = ctx.JavaPath
	ctx.Healing.AuthMode = ctx.AuthMode
	if ctx.Healing.RequestedPreset == "" && ctx.CompositionPlan != nil && ctx.CompositionPlan.JVMPreset != "" {
		ctx.Healing.RequestedPreset = ctx.CompositionPlan.JVMPreset
	}

	if ctx.Healing.RequestedPreset != "" && ctx.Healing.RequestedPreset != ctx.EffectivePreset {
		appendHealingWarning(ctx.Healing, fmt.Sprintf("Requested JVM preset %q was downgraded to %q for compatibility", ctx.Healing.RequestedPreset, blankAsNone(ctx.EffectivePreset)))
	}
	if ctx.Healing.RequestedJavaPath != "" && ctx.Healing.RequestedJavaPath != ctx.JavaPath {
		appendHealingWarning(ctx.Healing, "Requested Java override was bypassed in favor of a safer managed runtime")
	}

	if ctx.Opts.AdvancedOverrides {
		if err := validateManualJavaOverride(ctx); err != nil {
			return err
		}
		if err := validateManualJVMArgs(ctx.Opts.ExtraJVMArgs, ctx.JavaInfo); err != nil {
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

func newLaunchError(err error, healing HealingSummary) error {
	if err == nil {
		err = errors.New("launch failed")
	}
	return &LaunchError{
		Message: err.Error(),
		Healing: healing,
	}
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
	appendHealingWarning(summary, recovery.Description)
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
	if ctx == nil || !supportsHotSpotTuning(ctx.JavaInfo) {
		return ""
	}
	family := composition.ClassifyVersion(extractBaseVersion(ctx.Opts.VersionID))
	if ctx.JavaInfo.Major <= 8 || family == composition.FamilyA || family == composition.FamilyB || family == composition.FamilyC {
		return PresetLegacy
	}
	return PresetPerformance
}

func classifyLaunchFailure(err error, gp *GameProcess) LaunchFailureClass {
	var validationErr *launchValidationError
	if err != nil && errors.As(err, &validationErr) && validationErr.Class != "" {
		return validationErr.Class
	}
	if gp != nil {
		if class := gp.GetFailureClass(); class != "" {
			return class
		}
		if detail := gp.GetFailureDetail(); detail != "" {
			return classifyFailureText(detail)
		}
	}
	if err != nil {
		if class := classifyFailureText(err.Error()); class != "" {
			return class
		}
	}
	return LaunchFailureUnknown
}

func classifyFailureText(text string) LaunchFailureClass {
	lo := strings.ToLower(strings.TrimSpace(text))
	switch {
	case lo == "":
		return ""
	case strings.Contains(lo, "unrecognized vm option"), strings.Contains(lo, "unsupported vm option"):
		return LaunchFailureJVMUnsupportedOption
	case strings.Contains(lo, "must be enabled via -xx:+unlockexperimentalvmoptions"):
		return LaunchFailureJVMExperimentalUnlock
	case strings.Contains(lo, "unlock option must precede"), strings.Contains(lo, "must precede"):
		return LaunchFailureJVMOptionOrdering
	case strings.Contains(lo, "unsupportedclassversionerror"),
		strings.Contains(lo, "compiled by a more recent version of the java runtime"),
		strings.Contains(lo, "requires java"),
		strings.Contains(lo, "java runtime"):
		return LaunchFailureJavaRuntimeMismatch
	case strings.Contains(lo, "resolutionexception: modules"),
		strings.Contains(lo, "export package"),
		strings.Contains(lo, "modulelayerhandler.buildlayer"):
		return LaunchFailureClasspathModuleConflict
	case strings.Contains(lo, "bootstraplauncher"),
		strings.Contains(lo, "modlauncher"),
		strings.Contains(lo, "nosuchelementexception: no value present"):
		return LaunchFailureLoaderBootstrapFailure
	case strings.Contains(lo, "microsoft account"),
		strings.Contains(lo, "check your microsoft account"),
		strings.Contains(lo, "multiplayer is disabled"):
		return LaunchFailureAuthModeIncompatible
	default:
		return LaunchFailureUnknown
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

func validateManualJavaOverride(ctx *LaunchContext) error {
	if ctx == nil || ctx.Opts.Config == nil {
		return nil
	}
	requested := strings.TrimSpace(ctx.Opts.Config.JavaPathOverride)
	if requested == "" || requested != ctx.JavaPath {
		return nil
	}
	required := ctx.Version.JavaVersion.MajorVersion
	if required > 0 && ctx.JavaInfo.Major > 0 && ctx.JavaInfo.Major != required {
		return &launchValidationError{
			Class:   LaunchFailureJavaRuntimeMismatch,
			Message: fmt.Sprintf("explicit Java override targets Java %d but this version requires Java %d", ctx.JavaInfo.Major, required),
		}
	}
	if required == 8 && ctx.JavaInfo.Major == 8 && ctx.JavaInfo.Update > 0 && ctx.JavaInfo.Update < 312 {
		return &launchValidationError{
			Class:   LaunchFailureJavaRuntimeMismatch,
			Message: fmt.Sprintf("explicit Java 8 override is too old for legacy support (8u%d detected; use 8u312 or newer)", ctx.JavaInfo.Update),
		}
	}
	return nil
}

func validateManualJVMArgs(args []string, info system.JavaRuntimeInfo) error {
	if len(args) == 0 {
		return nil
	}
	unlockIndex := -1
	for i, arg := range args {
		if arg == "-XX:+UnlockExperimentalVMOptions" {
			unlockIndex = i
			break
		}
	}

	for i, arg := range args {
		switch {
		case arg == "-XX:+UseShenandoahGC" && !supportsShenandoah(info):
			return &launchValidationError{Class: LaunchFailureJVMUnsupportedOption, Message: "explicit JVM args request Shenandoah on an unsupported runtime"}
		case arg == "-XX:+UseZGC" && !supportsZGC(info):
			return &launchValidationError{Class: LaunchFailureJVMUnsupportedOption, Message: "explicit JVM args request ZGC on an unsupported runtime"}
		case arg == "-XX:+ZGenerational" && !supportsGenerationalZGC(info):
			return &launchValidationError{Class: LaunchFailureJVMUnsupportedOption, Message: "explicit JVM args request Generational ZGC on an unsupported runtime"}
		case strings.HasPrefix(arg, "-XX:G1NewSizePercent="), strings.HasPrefix(arg, "-XX:G1MaxNewSizePercent="):
			if !runtimeCaps(info).ExperimentalG1 {
				return &launchValidationError{Class: LaunchFailureJVMUnsupportedOption, Message: "explicit JVM args request experimental G1 tuning on an unsupported runtime"}
			}
			if unlockIndex == -1 {
				return &launchValidationError{Class: LaunchFailureJVMExperimentalUnlock, Message: "explicit JVM args require -XX:+UnlockExperimentalVMOptions"}
			}
			if unlockIndex > i {
				return &launchValidationError{Class: LaunchFailureJVMOptionOrdering, Message: "explicit JVM args place -XX:+UnlockExperimentalVMOptions after dependent flags"}
			}
		}
	}
	return nil
}
