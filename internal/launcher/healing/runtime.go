package healing

import (
	"errors"
	"strings"

	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/system"
)

// Resolver resolves a Java runtime for the given override path.
// Pass an empty override path to resolve the managed/default runtime.
type Resolver func(overridePath string) (*minecraft.JavaResult, system.JavaRuntimeInfo, error)

// RuntimeSelection keeps the selected runtime input separate from the
// effective runtime that the launcher will actually use.
type RuntimeSelection struct {
	RequestedPath            string
	SelectedPath             string
	SelectedInfo             system.JavaRuntimeInfo
	EffectivePath            string
	EffectiveInfo            system.JavaRuntimeInfo
	EffectiveSource          string
	BypassedRequestedRuntime bool
}

// ResolveRuntime chooses the effective runtime for a launch while preserving
// the originally selected runtime input for reporting and later validation.
func ResolveRuntime(required minecraft.JavaVersion, requestedPath string, forceManaged bool, resolve Resolver) (RuntimeSelection, error) {
	selection := RuntimeSelection{RequestedPath: strings.TrimSpace(requestedPath)}
	if resolve == nil {
		return selection, errors.New("runtime resolver is required")
	}

	if !forceManaged && selection.RequestedPath != "" {
		selectedResult, selectedInfo, err := resolve(selection.RequestedPath)
		if err != nil {
			return selection, err
		}
		if selectedResult != nil && selectedResult.Source == "override" {
			selection.SelectedPath = selectedResult.Path
			selection.SelectedInfo = selectedInfo
		} else {
			selection.BypassedRequestedRuntime = true
		}
		selection.applyEffective(selectedResult, selectedInfo)
		if ShouldBypassRequestedRuntime(required, selectedResult, selectedInfo) {
			managedResult, managedInfo, managedErr := resolve("")
			if managedErr == nil {
				selection.applyEffective(managedResult, managedInfo)
				selection.BypassedRequestedRuntime = true
			}
		}
		return selection, nil
	}

	effectiveResult, effectiveInfo, err := resolve("")
	if err != nil {
		return selection, err
	}
	selection.applyEffective(effectiveResult, effectiveInfo)
	if forceManaged && selection.RequestedPath != "" {
		selection.BypassedRequestedRuntime = true
	}
	return selection, nil
}

// ShouldBypassRequestedRuntime reports whether the selected override should be
// replaced by a managed runtime before downstream launch decisions are made.
func ShouldBypassRequestedRuntime(required minecraft.JavaVersion, result *minecraft.JavaResult, info system.JavaRuntimeInfo) bool {
	if result == nil || result.Source != "override" {
		return false
	}
	if info.Major == 0 || required.MajorVersion == 0 {
		return false
	}
	if info.Major != required.MajorVersion {
		return true
	}
	if info.Major == 8 && info.Update > 0 && info.Update < 312 {
		return true
	}
	return false
}

func (s *RuntimeSelection) applyEffective(result *minecraft.JavaResult, info system.JavaRuntimeInfo) {
	if s == nil || result == nil {
		return
	}
	s.EffectivePath = result.Path
	s.EffectiveInfo = info
	s.EffectiveSource = result.Source
}
