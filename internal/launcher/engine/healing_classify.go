package engine

import (
	"errors"
	"strings"
)

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
