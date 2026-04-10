package engine

import (
	"fmt"
	"strings"

	"github.com/mateoltd/croopor/internal/system"
)

type launchValidationError struct {
	Class   LaunchFailureClass
	Message string
}

func (e *launchValidationError) Error() string { return e.Message }

func validateManualJavaOverride(ctx *LaunchContext) error {
	if ctx == nil || ctx.Opts.Config == nil {
		return nil
	}
	requested := strings.TrimSpace(ctx.Opts.Config.JavaPathOverride)
	if requested == "" || requested != ctx.JavaRuntime.EffectivePath {
		return nil
	}
	required := ctx.Version.JavaVersion.MajorVersion
	if required > 0 && ctx.JavaRuntime.EffectiveInfo.Major > 0 && ctx.JavaRuntime.EffectiveInfo.Major != required {
		return &launchValidationError{
			Class:   LaunchFailureJavaRuntimeMismatch,
			Message: fmt.Sprintf("explicit Java override targets Java %d but this version requires Java %d", ctx.JavaRuntime.EffectiveInfo.Major, required),
		}
	}
	if required == 8 && ctx.JavaRuntime.EffectiveInfo.Major == 8 && ctx.JavaRuntime.EffectiveInfo.Update > 0 && ctx.JavaRuntime.EffectiveInfo.Update < 312 {
		return &launchValidationError{
			Class:   LaunchFailureJavaRuntimeMismatch,
			Message: fmt.Sprintf("explicit Java 8 override is too old for legacy support (8u%d detected; use 8u312 or newer)", ctx.JavaRuntime.EffectiveInfo.Update),
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
