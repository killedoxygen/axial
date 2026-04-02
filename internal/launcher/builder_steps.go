package launcher

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/performance"
	"github.com/mateoltd/croopor/internal/system"
)

// resolveVersionStep resolves the version JSON (handles inheritsFrom).
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

// setupEnvironmentStep sets up the Minecraft environment (is_demo_user = false is the key).
type setupEnvironmentStep struct{}

func (s *setupEnvironmentStep) Name() string { return "setup environment" }

func (s *setupEnvironmentStep) Execute(ctx *LaunchContext) error {
	ctx.Env = minecraft.DefaultEnvironment()
	return nil
}

// resolveJavaStep finds or downloads the correct Java runtime.
type resolveJavaStep struct{}

func (s *resolveJavaStep) Name() string { return "resolve java" }

func (s *resolveJavaStep) Execute(ctx *LaunchContext) error {
	javaResult, err := minecraft.EnsureJavaRuntime(ctx.Opts.MCDir, ctx.Version.JavaVersion, ctx.Opts.Config.JavaPathOverride)
	if err != nil {
		return fmt.Errorf("java runtime: %w", err)
	}
	ctx.JavaPath = javaResult.Path
	ctx.JavaMajor = ctx.Version.JavaVersion.MajorVersion

	info := system.DetectJavaRuntimeInfo(ctx.JavaPath)
	if info.Major > 0 {
		ctx.JavaMajor = info.Major
	}

	baseVersion := extractBaseVersion(ctx.Opts.VersionID)
	family := composition.ClassifyVersion(baseVersion)
	if err := validateJavaForLaunch(family, info); err != nil {
		return err
	}
	return nil
}

// resolveLibrariesStep resolves libraries, finds the client JAR, and builds the classpath.
type resolveLibrariesStep struct{}

func (s *resolveLibrariesStep) Name() string { return "resolve libraries" }

func (s *resolveLibrariesStep) Execute(ctx *LaunchContext) error {
	libs, err := minecraft.ResolveLibraries(ctx.Version, ctx.Opts.MCDir, ctx.Env)
	if err != nil {
		return err
	}
	ctx.Libraries = libs
	ctx.ClientJarPath = findClientJar(ctx.Opts.MCDir, ctx.Version, ctx.Opts.VersionID)
	ctx.Classpath = minecraft.BuildClasspath(libs, ctx.ClientJarPath)
	ctx.IsModded = isModdedVersion(ctx.Opts.MCDir, ctx.Opts.VersionID)
	return nil
}

// resolveCompositionStep resolves the effective performance plan for this launch.
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

// extractNativesStep creates the natives directory and extracts native DLLs.
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

// buildLaunchVarsStep builds the variable map used for argument substitution.
type buildLaunchVarsStep struct{}

func (s *buildLaunchVarsStep) Name() string { return "build launch vars" }

func (s *buildLaunchVarsStep) Execute(ctx *LaunchContext) error {
	username := ctx.Opts.Username
	if username == "" {
		username = "Player"
	}

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
		AuthAccessToken:    "null",
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

// resolveArgumentsStep resolves JVM and game arguments from the version JSON.
type resolveArgumentsStep struct{}

func (s *resolveArgumentsStep) Name() string { return "resolve arguments" }

func (s *resolveArgumentsStep) Execute(ctx *LaunchContext) error {
	ctx.JVMArgs, ctx.GameArgs = minecraft.ResolveArguments(ctx.Version, ctx.Env, ctx.Vars)
	return nil
}

// prepareCDSStep adds CDS flags if an archive exists (vanilla versions only, Java 11+).
type prepareCDSStep struct{}

func (s *prepareCDSStep) Name() string { return "prepare CDS" }

func (s *prepareCDSStep) Execute(ctx *LaunchContext) error {
	if !ctx.IsModded && ctx.JavaMajor >= 11 && CDSArchiveExists(ctx.ConfigDir, ctx.Opts.VersionID) {
		ctx.CDSArgs = []string{
			"-Xshare:auto",
			"-XX:SharedArchiveFile=" + CDSArchivePath(ctx.ConfigDir, ctx.Opts.VersionID),
		}
	}
	return nil
}

// computeMemoryStep computes memory flags.
type computeMemoryStep struct{}

func (s *computeMemoryStep) Name() string { return "compute memory" }

func (s *computeMemoryStep) Execute(ctx *LaunchContext) error {
	maxMem := ctx.Opts.MaxMemoryMB
	if maxMem <= 0 {
		_, maxMem = recommendedMemoryForLaunch(extractBaseVersion(ctx.Opts.VersionID), ctx.IsModded, system.Detect())
	}
	minMem := ctx.Opts.MinMemoryMB
	if minMem <= 0 {
		minMem, _ = recommendedMemoryForLaunch(extractBaseVersion(ctx.Opts.VersionID), ctx.IsModded, system.Detect())
	}
	if minMem > maxMem {
		minMem = maxMem
	}
	ctx.EffectiveMaxMemoryMB = maxMem
	ctx.EffectiveMinMemoryMB = minMem
	ctx.MemArgs = []string{
		fmt.Sprintf("-Xmx%dM", maxMem),
		fmt.Sprintf("-Xms%dM", minMem),
	}
	return nil
}

// applyBootThrottleStep adds boot throttling JVM flags.
type applyBootThrottleStep struct{}

func (s *applyBootThrottleStep) Name() string { return "apply boot throttle" }

func (s *applyBootThrottleStep) Execute(ctx *LaunchContext) error {
	ctx.BootArgs = bootThrottleArgs(ctx.JavaMajor)
	return nil
}

// applyGCPresetStep adds GC preset JVM flags.
type applyGCPresetStep struct{}

func (s *applyGCPresetStep) Name() string { return "apply GC preset" }

func (s *applyGCPresetStep) Execute(ctx *LaunchContext) error {
	preset := ctx.Opts.Config.JVMPreset
	if preset == "" {
		hw := system.Detect()
		dist := system.JavaDistributionUnknown
		if ctx.JavaPath != "" {
			dist = system.DetectJavaDistribution(ctx.JavaPath)
		}
		preset = AutoSelectPreset(hw, ctx.JavaMajor, dist)
	}
	ctx.EffectivePreset = preset
	ctx.GCArgs = gcPresetArgs(preset, ctx.JavaMajor)
	return nil
}

// applyCompositionJVMStep applies any JVM flags mandated by the composition plan.
type applyCompositionJVMStep struct{}

func (s *applyCompositionJVMStep) Name() string { return "apply composition JVM" }

func (s *applyCompositionJVMStep) Execute(ctx *LaunchContext) error {
	if ctx.CompositionPlan == nil || ctx.Opts.Config.JVMPreset != "" {
		return nil
	}
	if ctx.CompositionPlan.JVMPreset != "" {
		ctx.GCArgs = gcPresetArgs(ctx.CompositionPlan.JVMPreset, ctx.JavaMajor)
	}
	return nil
}

// prefetchStep prefetches key files into OS page cache before JVM starts.
type prefetchStep struct{}

func (s *prefetchStep) Name() string { return "prefetch" }

func (s *prefetchStep) Execute(ctx *LaunchContext) error {
	prefetchForLaunch(ctx.Libraries, ctx.ClientJarPath, ctx.Opts.MCDir, ctx.Version.AssetIndex.ID)
	return nil
}

// buildCommandStep assembles the full command line and creates the exec.Cmd.
type buildCommandStep struct{}

func (s *buildCommandStep) Name() string { return "build command" }

func (s *buildCommandStep) Execute(ctx *LaunchContext) error {
	// Order: java [cds_args] [boot_args] [jvm_args] [gc_args] [extra_args] [mem_args] <mainClass> [game_args]
	var cmdArgs []string
	cmdArgs = append(cmdArgs, ctx.CDSArgs...)
	cmdArgs = append(cmdArgs, ctx.BootArgs...)
	cmdArgs = append(cmdArgs, ctx.JVMArgs...)
	cmdArgs = append(cmdArgs, ctx.GCArgs...)
	cmdArgs = append(cmdArgs, ctx.Opts.ExtraJVMArgs...)
	cmdArgs = append(cmdArgs, ctx.MemArgs...)
	cmdArgs = append(cmdArgs, ctx.Version.MainClass)
	cmdArgs = append(cmdArgs, ctx.GameArgs...)
	ctx.CmdArgs = cmdArgs

	cmd := exec.Command(ctx.JavaPath, cmdArgs...)
	cmd.Dir = ctx.GameDir
	setProcAttr(cmd)
	ctx.Cmd = cmd

	return nil
}

// startProcessStep creates and starts the game process.
type startProcessStep struct{}

func (s *startProcessStep) Name() string { return "start process" }

func (s *startProcessStep) Execute(ctx *LaunchContext) error {
	gp := NewGameProcess(ctx.Cmd, ctx.NativesDir)
	if err := gp.Start(); err != nil {
		CleanupNativesDir(ctx.NativesDir)
		return err
	}
	ctx.Process = gp
	return nil
}

// startProfilerStep starts the boot profiler to capture diagnostic data.
type startProfilerStep struct{}

func (s *startProfilerStep) Name() string { return "start profiler" }

func (s *startProfilerStep) Execute(ctx *LaunchContext) error {
	profile := NewBootProfile(
		ctx.SessionID, ctx.Opts.VersionID, ctx.Process.PID(),
		ctx.EffectivePreset, ctx.EffectiveMaxMemoryMB,
		bootCPUCap(), len(ctx.CDSArgs) > 0,
	)
	profile.Start()
	ctx.Process.Profile = profile
	return nil
}

// scheduleCDSStep schedules CDS archive generation or auto-repair.
type scheduleCDSStep struct{}

func (s *scheduleCDSStep) Name() string { return "schedule CDS" }

func (s *scheduleCDSStep) Execute(ctx *LaunchContext) error {
	// Schedule CDS archive generation for next launch if not yet cached
	if !ctx.IsModded && ctx.JavaMajor >= 11 && len(ctx.CDSArgs) == 0 {
		javaPath := ctx.JavaPath
		classpath := ctx.Classpath
		configDir := ctx.ConfigDir
		versionID := ctx.Opts.VersionID
		go func() {
			archivePath := CDSArchivePath(configDir, versionID)
			if err := GenerateCDSArchive(javaPath, classpath, archivePath); err != nil {
				log.Printf("CDS archive generation failed for %s: %v", versionID, err)
			}
		}()
	}

	// Auto-repair CDS: if the JVM detects a corrupted archive, invalidate it
	if len(ctx.CDSArgs) > 0 {
		configDir := ctx.ConfigDir
		versionID := ctx.Opts.VersionID
		gp := ctx.Process
		go func() {
			<-gp.Done()
			if gp.CDSFailed {
				log.Printf("CDS archive unusable for %s — invalidating for next launch", versionID)
				InvalidateCDSArchive(configDir, versionID)
			}
		}()
	}

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
	parts := strings.Split(versionID, "-")
	if len(parts) == 0 {
		return versionID
	}
	if mcVersionPrefixPattern.MatchString(parts[0]) {
		return parts[0]
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

func validateJavaForLaunch(family composition.VersionFamily, info system.JavaRuntimeInfo) error {
	if info.Major == 0 {
		return nil
	}

	switch family {
	case composition.FamilyB, composition.FamilyC:
		if info.Major >= 9 && info.Major <= 15 {
			return fmt.Errorf("Java %d is not supported for legacy Minecraft versions; use Java 8 or Java 17+", info.Major)
		}
		if info.Major == 8 && info.Update > 0 && info.Update < 312 {
			return fmt.Errorf("Java 8 update %d is too old for legacy Minecraft support; use Java 8u312 or newer", info.Update)
		}
	}
	return nil
}

func recommendedMemoryForLaunch(versionID string, isModded bool, hw system.HardwareProfile) (minMem, maxMem int) {
	totalMB := hw.TotalRAMMB
	if totalMB <= 0 {
		totalMB = 8192
	}
	baseMin, baseMax := system.RecommendedMemoryRange(totalMB)
	family := composition.ClassifyVersion(versionID)

	switch family {
	case composition.FamilyA, composition.FamilyB:
		minMem = 1024
		if hw.Tier == system.HardwareTierLow {
			minMem = 768
		}
		maxMem = minInt(baseMax, 2048)
	case composition.FamilyC:
		minMem = 1024
		if hw.Tier != system.HardwareTierLow {
			minMem = 1536
		}
		maxMem = minInt(baseMax, 4096)
		if isModded && hw.Tier == system.HardwareTierHigh {
			maxMem = minInt(baseMax, 6144)
		}
	case composition.FamilyD:
		minMem = 1536
		if hw.Tier != system.HardwareTierLow {
			minMem = 2048
		}
		maxMem = minInt(baseMax, 6144)
	default:
		minMem = maxInt(baseMin, 2048)
		maxMem = baseMax
		if isModded && hw.Tier == system.HardwareTierHigh {
			maxMem = maxInt(maxMem, 6144)
		}
	}

	if maxMem > totalMB-2048 {
		maxMem = totalMB - 2048
	}
	if maxMem < 1024 {
		maxMem = 1024
	}
	if minMem > maxMem {
		minMem = maxMem
	}
	if minMem < 512 {
		minMem = 512
	}
	return minMem, maxMem
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}
