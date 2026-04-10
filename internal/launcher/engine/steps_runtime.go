package engine

import (
	"fmt"

	launchruntime "github.com/mateoltd/croopor/internal/launcher/runtime"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/system"
)

var resolveJavaRuntime = func(mcDir string, javaVersion minecraft.JavaVersion, overridePath string) (*minecraft.JavaResult, system.JavaRuntimeInfo, error) {
	javaResult, resolveErr := minecraft.EnsureJavaRuntime(mcDir, javaVersion, overridePath)
	if resolveErr != nil {
		return nil, system.JavaRuntimeInfo{}, fmt.Errorf("java runtime: %w", resolveErr)
	}
	info := system.DetectJavaRuntimeInfo(javaResult.Path)
	return javaResult, info, nil
}

type resolveJavaStep struct{}

func (s *resolveJavaStep) Name() string { return "resolve java" }

func (s *resolveJavaStep) Execute(ctx *LaunchContext) error {
	selection, err := launchruntime.ResolveRuntime(
		ctx.Version.JavaVersion,
		ctx.Opts.Config.JavaPathOverride,
		ctx.Opts.ForceManagedJava,
		func(overridePath string) (*minecraft.JavaResult, system.JavaRuntimeInfo, error) {
			return resolveJavaRuntime(ctx.Opts.MCDir, ctx.Version.JavaVersion, overridePath)
		},
	)
	if err != nil {
		return err
	}
	if selection.EffectivePath != "" && selection.EffectiveSource != "override" && selection.EffectiveInfo.Major == 0 && ctx.Version.JavaVersion.MajorVersion > 0 {
		selection.EffectiveInfo.Major = ctx.Version.JavaVersion.MajorVersion
	}
	ctx.JavaRuntime = selection
	ctx.EffectiveJavaMajor = ctx.Version.JavaVersion.MajorVersion
	if selection.EffectiveInfo.Major > 0 {
		ctx.EffectiveJavaMajor = selection.EffectiveInfo.Major
	}
	return nil
}
