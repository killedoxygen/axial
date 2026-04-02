package launcher

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/mateoltd/croopor/internal/minecraft"
)

// resolveVersionStep resolves the version JSON (handles inheritsFrom).
type resolveVersionStep struct{}

func (s *resolveVersionStep) Name() string { return "resolve version" }

func (s *resolveVersionStep) Execute(ctx *LaunchContext) error {
	version, err := minecraft.ResolveVersion(ctx.Opts.MCDir, ctx.Opts.VersionID)
	if err != nil {
		return fmt.Errorf("resolving version: %w", err)
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
	return nil
}

// resolveLibrariesStep resolves libraries, finds the client JAR, and builds the classpath.
type resolveLibrariesStep struct{}

func (s *resolveLibrariesStep) Name() string { return "resolve libraries" }

func (s *resolveLibrariesStep) Execute(ctx *LaunchContext) error {
	libs, err := minecraft.ResolveLibraries(ctx.Version, ctx.Opts.MCDir, ctx.Env)
	if err != nil {
		return fmt.Errorf("resolving libraries: %w", err)
	}
	ctx.Libraries = libs
	ctx.ClientJarPath = findClientJar(ctx.Opts.MCDir, ctx.Version, ctx.Opts.VersionID)
	ctx.Classpath = minecraft.BuildClasspath(libs, ctx.ClientJarPath)
	ctx.IsModded = isModdedVersion(ctx.Opts.MCDir, ctx.Opts.VersionID)
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
		maxMem = 4096
	}
	minMem := ctx.Opts.MinMemoryMB
	if minMem <= 0 {
		minMem = 512
	}
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
	ctx.GCArgs = gcPresetArgs(ctx.Opts.Config.JVMPreset, ctx.JavaMajor)
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
		return fmt.Errorf("starting game process: %w", err)
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
		ctx.Opts.Config.JVMPreset, ctx.Opts.MaxMemoryMB,
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
