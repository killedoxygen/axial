package engine

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/performance"
	"github.com/mateoltd/croopor/internal/system"
)

type resolveVersionStep struct{}

func (s *resolveVersionStep) Name() string { return "resolve version" }

func (s *resolveVersionStep) Execute(ctx *LaunchContext) error {
	version, err := minecraft.ResolveVersion(ctx.Opts.MCDir, ctx.Opts.VersionID)
	if err != nil {
		return err
	}
	ctx.Version = version
	return nil
}

type setupEnvironmentStep struct{}

func (s *setupEnvironmentStep) Name() string { return "setup environment" }

func (s *setupEnvironmentStep) Execute(ctx *LaunchContext) error {
	ctx.Env = minecraft.DefaultEnvironment()
	return nil
}

type resolveLibrariesStep struct{}

func (s *resolveLibrariesStep) Name() string { return "resolve libraries" }

func (s *resolveLibrariesStep) Execute(ctx *LaunchContext) error {
	libs, err := minecraft.ResolveLibraries(ctx.Version, ctx.Opts.MCDir, ctx.Env)
	if err != nil {
		return err
	}
	ctx.Libraries = libs
	if usesModuleBootstrap(ctx.Version) {
		ctx.ClientJarPath = ""
	} else {
		ctx.ClientJarPath = findClientJar(ctx.Opts.MCDir, ctx.Version, ctx.Opts.VersionID)
	}
	ctx.Classpath = minecraft.BuildClasspath(libs, ctx.ClientJarPath)
	ctx.IsModded = isModdedVersion(ctx.Opts.MCDir, ctx.Opts.VersionID)
	return nil
}

type resolveCompositionStep struct {
	manager *performance.PerformanceManager
}

func (s *resolveCompositionStep) Name() string { return "resolve composition" }

func (s *resolveCompositionStep) Execute(ctx *LaunchContext) error {
	if s.manager == nil {
		return nil
	}
	loader := ctx.Opts.Loader
	if loader == "" {
		loader = inferLoader(ctx)
	}
	mode := ctx.Opts.CompositionMode
	if mode == "" {
		mode = composition.ModeManaged
	}
	gameDir := ctx.Opts.GameDir
	if gameDir == "" {
		gameDir = ctx.Opts.MCDir
	}
	req := composition.ResolutionRequest{
		GameVersion:   extractBaseVersion(ctx.Opts.VersionID),
		Loader:        loader,
		Mode:          mode,
		Hardware:      system.Detect(),
		InstalledMods: listModIDs(gameDir),
	}
	ctx.CompositionPlan = s.manager.GetPlan(req)
	return nil
}

type extractNativesStep struct{}

func (s *extractNativesStep) Name() string { return "extract natives" }

func (s *extractNativesStep) Execute(ctx *LaunchContext) error {
	nativesDir, err := CreateNativesDir(ctx.SessionID)
	if err != nil {
		return fmt.Errorf("creating natives dir: %w", err)
	}
	ctx.NativesDir = nativesDir

	if err := ExtractLegacyNatives(ctx.Libraries, nativesDir); err != nil {
		CleanupNativesDir(nativesDir)
		return fmt.Errorf("extracting natives: %w", err)
	}
	return nil
}

type buildLaunchVarsStep struct{}

func (s *buildLaunchVarsStep) Name() string { return "build launch vars" }

func (s *buildLaunchVarsStep) Execute(ctx *LaunchContext) error {
	username := ctx.Opts.Username
	if username == "" {
		username = "Player"
	}
	authMode := ctx.Opts.AuthMode
	if authMode == "" {
		authMode = LaunchAuthOffline
	}
	ctx.AuthMode = authMode

	gameDir := ctx.Opts.GameDir
	if gameDir == "" {
		gameDir = ctx.Opts.MCDir
	}
	ctx.GameDir = gameDir

	var gameAssets string
	if minecraft.IsLegacyAssets(ctx.Opts.MCDir, ctx.Version.AssetIndex.ID) {
		gameAssets = filepath.Join(minecraft.AssetsDir(ctx.Opts.MCDir), "virtual", "legacy")
	}

	ctx.Vars = &minecraft.LaunchVars{
		AuthPlayerName:     username,
		VersionName:        ctx.Version.ID,
		GameDirectory:      gameDir,
		AssetsRoot:         minecraft.AssetsDir(ctx.Opts.MCDir),
		AssetIndexName:     ctx.Version.AssetIndex.ID,
		AuthUUID:           minecraft.OfflineUUID(username),
		AuthAccessToken:    "0",
		ClientID:           "",
		AuthXUID:           "",
		UserType:           "msa",
		VersionType:        ctx.Version.Type,
		LauncherName:       "croopor",
		LauncherVersion:    "1.0.0",
		NativesDirectory:   ctx.NativesDir,
		Classpath:          ctx.Classpath,
		LibraryDirectory:   minecraft.LibrariesDir(ctx.Opts.MCDir),
		ClasspathSeparator: string(os.PathListSeparator),
		GameAssets:         gameAssets,
	}

	if ctx.Opts.Config.WindowWidth > 0 && ctx.Opts.Config.WindowHeight > 0 {
		ctx.Vars.ResolutionWidth = fmt.Sprintf("%d", ctx.Opts.Config.WindowWidth)
		ctx.Vars.ResolutionHeight = fmt.Sprintf("%d", ctx.Opts.Config.WindowHeight)
		ctx.Env.Features["has_custom_resolution"] = true
	}

	return nil
}

type resolveArgumentsStep struct{}

func (s *resolveArgumentsStep) Name() string { return "resolve arguments" }

func (s *resolveArgumentsStep) Execute(ctx *LaunchContext) error {
	ctx.JVMArgs, ctx.GameArgs = minecraft.ResolveArguments(ctx.Version, ctx.Env, ctx.Vars)
	return nil
}

var mcVersionPrefixPattern = regexp.MustCompile(`^\d+\.\d+(?:\.\d+)?$`)

func inferLoader(ctx *LaunchContext) string {
	versionID := ctx.Opts.VersionID
	if ctx.Version != nil && ctx.Version.ID != "" {
		versionID = ctx.Version.ID
	}
	versionID = strings.ToLower(versionID)
	switch {
	case strings.Contains(versionID, "fabric"):
		return "fabric"
	case strings.Contains(versionID, "forge") && strings.Contains(versionID, "neoforge"):
		return "neoforge"
	case strings.Contains(versionID, "neoforge"):
		return "neoforge"
	case strings.Contains(versionID, "forge"):
		return "forge"
	case strings.Contains(versionID, "quilt"):
		return "quilt"
	default:
		return "vanilla"
	}
}

func extractBaseVersion(versionID string) string {
	var fallback string
	for _, part := range strings.Split(versionID, "-") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		if v, err := composition.Parse(part); err == nil && (v.IsSnapshot || v.Major == 1) {
			return part
		}
		if fallback == "" && mcVersionPrefixPattern.MatchString(part) {
			fallback = part
		}
	}
	if fallback != "" {
		return fallback
	}
	return versionID
}

func listModIDs(gameDir string) []string {
	if gameDir == "" {
		return nil
	}
	state, err := performance.LoadState(filepath.Join(gameDir, "mods"))
	if err != nil || state == nil {
		return nil
	}
	out := make([]string, 0, len(state.InstalledMods))
	for _, mod := range state.InstalledMods {
		out = append(out, mod.ProjectID)
	}
	return out
}
