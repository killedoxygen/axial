package minecraft

import (
	"regexp"
	"runtime"
	"strings"
)

// Rule represents a conditional rule for arguments or libraries.
type Rule struct {
	Action   string          `json:"action"`
	OS       *OSRule         `json:"os,omitempty"`
	Features map[string]bool `json:"features,omitempty"`
}

type OSRule struct {
	Name    string `json:"name,omitempty"`
	Arch    string `json:"arch,omitempty"`
	Version string `json:"version,omitempty"`
}

// Environment holds the current system state for rule evaluation.
type Environment struct {
	OSName    string          // "windows", "osx", "linux"
	OSArch    string          // "x86", "x86_64", "arm64"
	OSVersion string          // OS version string for regex matching
	Features  map[string]bool // e.g. {"is_demo_user": false, "has_custom_resolution": false}
}

// DefaultEnvironment returns the environment for the current system,
// with is_demo_user set to false (the core of the para-launcher).
func DefaultEnvironment() Environment {
	env := Environment{
		OSName: currentOSName(),
		OSArch: currentOSArch(),
		Features: map[string]bool{
			"is_demo_user":               false,
			"has_custom_resolution":      false,
			"has_quick_plays_support":    true,
			"is_quick_play_singleplayer": false,
			"is_quick_play_multiplayer":  false,
			"is_quick_play_realms":       false,
		},
	}
	return env
}

// EvaluateRules determines if an item (argument or library) should be included.
// Empty rules = always included. Rules are evaluated in order; last matching rule wins.
func EvaluateRules(rules []Rule, env Environment) bool {
	if len(rules) == 0 {
		return true
	}

	action := "disallow" // default when rules exist
	for _, rule := range rules {
		if ruleMatches(rule, env) {
			action = rule.Action
		}
	}
	return action == "allow"
}

func ruleMatches(rule Rule, env Environment) bool {
	// All conditions in the rule must match (AND logic)

	if rule.OS != nil {
		if rule.OS.Name != "" && rule.OS.Name != env.OSName {
			return false
		}
		if rule.OS.Arch != "" && rule.OS.Arch != env.OSArch {
			return false
		}
		if rule.OS.Version != "" && env.OSVersion != "" {
			matched, err := regexp.MatchString(rule.OS.Version, env.OSVersion)
			if err != nil || !matched {
				return false
			}
		}
	}

	if rule.Features != nil {
		for feature, required := range rule.Features {
			actual, exists := env.Features[feature]
			if !exists {
				// Unknown feature: if required=true, we don't have it → no match
				// if required=false, we don't have it → matches (we also don't have it)
				if required {
					return false
				}
				continue
			}
			if actual != required {
				return false
			}
		}
	}

	return true
}

func currentOSName() string {
	switch runtime.GOOS {
	case "windows":
		return "windows"
	case "darwin":
		return "osx"
	default:
		return "linux"
	}
}

func currentOSArch() string {
	arch := runtime.GOARCH
	switch arch {
	case "amd64":
		return "x86_64"
	case "386":
		return "x86"
	case "arm64":
		return "arm64"
	default:
		return arch
	}
}

// NativeClassifierKey returns the OS-specific classifier suffix (e.g., "natives-windows").
func NativeClassifierKey() string {
	name := currentOSName()
	if name == "osx" {
		name = "macos"
	}
	return "natives-" + name
}

// IsNativeLibrary checks if a library name contains a natives classifier.
func IsNativeLibrary(name string) bool {
	lower := strings.ToLower(name)
	return strings.Contains(lower, "natives-")
}
