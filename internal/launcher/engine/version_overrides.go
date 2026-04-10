package engine

import (
	"log"

	"github.com/mateoltd/croopor/internal/composition"
)

type versionOverrideContext struct {
	BaseVersion string
	AuthMode    LaunchAuthMode
}

// versionOverride defines a set of JVM flags to inject when the launch context
// matches a predicate. All matching overrides apply (no short-circuit).
type versionOverride struct {
	ID      string
	Reason  string
	Match   func(versionOverrideContext) bool
	JVMArgs []string
}

// matchVersions returns a matcher that checks if the base version equals
// any of the given version strings via exact semantic comparison.
func matchVersions(versions ...string) func(versionOverrideContext) bool {
	targets := make([]composition.MCVersion, 0, len(versions))
	for _, v := range versions {
		parsed, err := composition.Parse(v)
		if err != nil {
			log.Printf("version override: invalid target version %q: %v", v, err)
			continue
		}
		targets = append(targets, parsed)
	}
	return func(ctx versionOverrideContext) bool {
		v, err := composition.Parse(ctx.BaseVersion)
		if err != nil {
			return false
		}
		for _, t := range targets {
			if v.Compare(t) == 0 {
				return true
			}
		}
		return false
	}
}

func matchAuthModes(modes ...LaunchAuthMode) func(versionOverrideContext) bool {
	return func(ctx versionOverrideContext) bool {
		for _, mode := range modes {
			if ctx.AuthMode == mode {
				return true
			}
		}
		return false
	}
}

func matchAll(matchers ...func(versionOverrideContext) bool) func(versionOverrideContext) bool {
	return func(ctx versionOverrideContext) bool {
		for _, matcher := range matchers {
			if matcher == nil {
				continue
			}
			if !matcher(ctx) {
				return false
			}
		}
		return true
	}
}

// builtinOverrides is the table of all hardcoded version overrides.
var builtinOverrides = []versionOverride{
	{
		ID:     "authlib-offline-mp",
		Reason: "1.16.4-1.16.5 authlib bug disables multiplayer with offline tokens",
		Match: matchAll(
			matchVersions("1.16.4", "1.16.5"),
			matchAuthModes(LaunchAuthOffline),
		),
		JVMArgs: []string{
			"-Dminecraft.api.env=custom",
			"-Dminecraft.api.auth.host=https://nope.invalid",
			"-Dminecraft.api.account.host=https://nope.invalid",
			"-Dminecraft.api.session.host=https://nope.invalid",
			"-Dminecraft.api.services.host=https://nope.invalid",
		},
	},
}

// applyVersionOverridesStep injects JVM flags for known per-version quirks.
type applyVersionOverridesStep struct{}

func (s *applyVersionOverridesStep) Name() string { return "apply version overrides" }

func (s *applyVersionOverridesStep) Execute(ctx *LaunchContext) error {
	overrideCtx := versionOverrideContext{
		BaseVersion: extractBaseVersion(ctx.Opts.VersionID),
		AuthMode:    ctx.AuthMode,
	}
	for _, ov := range builtinOverrides {
		if ov.Match(overrideCtx) {
			log.Printf("version override %s: %s", ov.ID, ov.Reason)
			ctx.JVMArgs = append(ctx.JVMArgs, ov.JVMArgs...)
		}
	}
	return nil
}
