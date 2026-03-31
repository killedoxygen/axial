package launcher

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/minecraft"
)

// LaunchOptions holds parameters for launching a version.
type LaunchOptions struct {
	VersionID    string
	InstanceID   string
	Username     string
	MaxMemoryMB  int
	MinMemoryMB  int
	MCDir        string   // Shared .minecraft (assets, libraries, versions)
	GameDir      string   // Instance game dir (saves, mods, config). Falls back to MCDir if empty.
	ExtraJVMArgs []string // Additional JVM args from instance overrides
	Config       *config.Config
}

// LaunchResult contains the constructed command and game process.
type LaunchResult struct {
	Command    []string
	JavaPath   string
	Process    *GameProcess
	SessionID  string
	NativesDir string
	VersionID  string
	InstanceID string
}

// BuildAndLaunch constructs the launch command and starts the game.
func BuildAndLaunch(opts LaunchOptions) (*LaunchResult, error) {
	sessionID := generateSessionID()

	// Step 1: Resolve version (handles inheritsFrom)
	version, err := minecraft.ResolveVersion(opts.MCDir, opts.VersionID)
	if err != nil {
		return nil, fmt.Errorf("resolving version: %w", err)
	}

	// Step 2: Set up environment (is_demo_user = false is the key)
	env := minecraft.DefaultEnvironment()

	// Step 3: Find or download the correct Java runtime
	javaResult, err := minecraft.EnsureJavaRuntime(opts.MCDir, version.JavaVersion, opts.Config.JavaPathOverride)
	if err != nil {
		return nil, fmt.Errorf("java runtime: %w", err)
	}

	// Step 4: Resolve libraries and build classpath
	libs, err := minecraft.ResolveLibraries(version, opts.MCDir, env)
	if err != nil {
		return nil, fmt.Errorf("resolving libraries: %w", err)
	}

	// Step 5: Determine client JAR path
	clientJarPath := findClientJar(opts.MCDir, version, opts.VersionID)

	classpath := minecraft.BuildClasspath(libs, clientJarPath)

	// Step 6: Create natives directory
	nativesDir, err := CreateNativesDir(sessionID)
	if err != nil {
		return nil, fmt.Errorf("creating natives dir: %w", err)
	}

	// Step 7: Extract native DLLs from native library JARs into the natives directory.
	// Required for ALL versions. Legacy versions use classifier JARs, modern versions
	// use separate native library entries, but both need extraction.
	if err := ExtractLegacyNatives(libs, nativesDir); err != nil {
		CleanupNativesDir(nativesDir)
		return nil, fmt.Errorf("extracting natives: %w", err)
	}

	// Step 8: Build launch variables
	username := opts.Username
	if username == "" {
		username = "Player"
	}

	gameDir := opts.GameDir
	if gameDir == "" {
		gameDir = opts.MCDir
	}

	// For pre-1.6 versions with virtual/legacy asset indexes,
	// game_assets must point to assets/virtual/legacy/ instead of assets/.
	var gameAssets string
	if minecraft.IsLegacyAssets(opts.MCDir, version.AssetIndex.ID) {
		gameAssets = filepath.Join(minecraft.AssetsDir(opts.MCDir), "virtual", "legacy")
	}

	vars := &minecraft.LaunchVars{
		AuthPlayerName:     username,
		VersionName:        version.ID,
		GameDirectory:      gameDir,
		AssetsRoot:         minecraft.AssetsDir(opts.MCDir),
		AssetIndexName:     version.AssetIndex.ID,
		AuthUUID:           minecraft.OfflineUUID(username),
		AuthAccessToken:    "null",
		ClientID:           "",
		AuthXUID:           "",
		UserType:           "msa",
		VersionType:        version.Type,
		LauncherName:       "croopor",
		LauncherVersion:    "1.0.0",
		NativesDirectory:   nativesDir,
		Classpath:          classpath,
		LibraryDirectory:   minecraft.LibrariesDir(opts.MCDir),
		ClasspathSeparator: string(os.PathListSeparator),
		GameAssets:         gameAssets,
	}

	if opts.Config.WindowWidth > 0 && opts.Config.WindowHeight > 0 {
		vars.ResolutionWidth = fmt.Sprintf("%d", opts.Config.WindowWidth)
		vars.ResolutionHeight = fmt.Sprintf("%d", opts.Config.WindowHeight)
		env.Features["has_custom_resolution"] = true
	}

	// Step 9: Resolve JVM and game arguments
	jvmArgs, gameArgs := minecraft.ResolveArguments(version, env, vars)

	// Step 10: Add boot throttling and GC preset flags
	javaMajor := version.JavaVersion.MajorVersion
	bootArgs := bootThrottleArgs(javaMajor)
	gcArgs := gcPresetArgs(opts.Config.JVMPreset, javaMajor)

	// Step 10b: Add CDS flags if archive exists (vanilla versions only, Java 11+)
	var cdsArgs []string
	isModded := isModdedVersion(opts.MCDir, opts.VersionID)
	configDir := config.ConfigDir()
	if !isModded && javaMajor >= 11 && CDSArchiveExists(configDir, opts.VersionID) {
		cdsArgs = []string{
			"-Xshare:auto",
			"-XX:SharedArchiveFile=" + CDSArchivePath(configDir, opts.VersionID),
		}
	}

	// Step 11: Add memory flags
	maxMem := opts.MaxMemoryMB
	if maxMem <= 0 {
		maxMem = 4096
	}
	minMem := opts.MinMemoryMB
	if minMem <= 0 {
		minMem = 512
	}
	memArgs := []string{
		fmt.Sprintf("-Xmx%dM", maxMem),
		fmt.Sprintf("-Xms%dM", minMem),
	}

	// Step 12: Assemble full command
	// Order: java [cds_args] [boot_args] [jvm_args] [gc_args] [extra_args] [mem_args] <mainClass> [game_args]
	var cmdArgs []string
	cmdArgs = append(cmdArgs, cdsArgs...)
	cmdArgs = append(cmdArgs, bootArgs...)
	cmdArgs = append(cmdArgs, jvmArgs...)
	cmdArgs = append(cmdArgs, gcArgs...)
	cmdArgs = append(cmdArgs, opts.ExtraJVMArgs...)
	cmdArgs = append(cmdArgs, memArgs...)
	cmdArgs = append(cmdArgs, version.MainClass)
	cmdArgs = append(cmdArgs, gameArgs...)

	// Step 12b: Prefetch key files into OS page cache before JVM starts.
	// Synchronous. Ensures files are in cache before the JVM touches them.
	prefetchForLaunch(libs, clientJarPath, opts.MCDir, version.AssetIndex.ID)

	// Step 13: Create exec.Cmd
	cmd := exec.Command(javaResult.Path, cmdArgs...)
	cmd.Dir = gameDir

	// Set up process attributes for Windows (detach so game survives if croopor exits)
	setProcAttr(cmd)

	result := &LaunchResult{
		Command:    append([]string{javaResult.Path}, cmdArgs...),
		JavaPath:   javaResult.Path,
		SessionID:  sessionID,
		NativesDir: nativesDir,
		VersionID:  opts.VersionID,
		InstanceID: opts.InstanceID,
	}

	// Step 14: Create and start game process
	gp := NewGameProcess(cmd, nativesDir)
	if err := gp.Start(); err != nil {
		CleanupNativesDir(nativesDir)
		return nil, fmt.Errorf("starting game process: %w", err)
	}
	result.Process = gp

	// Step 15: Start boot profiler to capture diagnostic data
	profile := NewBootProfile(
		sessionID, opts.VersionID, gp.PID(),
		opts.Config.JVMPreset, opts.MaxMemoryMB,
		bootCPUCap(), len(cdsArgs) > 0,
	)
	profile.Start()
	gp.Profile = profile

	// Step 16: Schedule CDS archive generation for next launch if not yet cached
	if !isModded && javaMajor >= 11 && len(cdsArgs) == 0 {
		go func() {
			archivePath := CDSArchivePath(configDir, opts.VersionID)
			if err := GenerateCDSArchive(javaResult.Path, classpath, archivePath); err != nil {
				log.Printf("CDS archive generation failed for %s: %v", opts.VersionID, err)
			}
		}()
	}

	// Step 17: Auto-repair CDS. If the JVM detects a corrupted archive, invalidate it.
	if len(cdsArgs) > 0 {
		cdsVersionID := opts.VersionID
		go func() {
			<-gp.Done()
			if gp.CDSFailed {
				log.Printf("CDS archive unusable for %s — invalidating for next launch", cdsVersionID)
				InvalidateCDSArchive(configDir, cdsVersionID)
			}
		}()
	}

	return result, nil
}

func findClientJar(mcDir string, v *minecraft.VersionJSON, originalVersionID string) string {
	versionsDir := minecraft.VersionsDir(mcDir)

	// Check the version's own directory first
	jarPath := filepath.Join(versionsDir, v.ID, v.ID+".jar")
	if _, err := os.Stat(jarPath); err == nil {
		return jarPath
	}

	// For modded versions (Fabric/Forge/NeoForge), the client JAR lives in the
	// parent vanilla version's directory. Load the original (unmerged) version
	// JSON to find the inheritsFrom field.
	if originalVersionID != "" {
		origJSON := filepath.Join(versionsDir, originalVersionID, originalVersionID+".json")
		if data, err := os.ReadFile(origJSON); err == nil {
			var stub struct {
				InheritsFrom string `json:"inheritsFrom"`
			}
			if json.Unmarshal(data, &stub) == nil && stub.InheritsFrom != "" {
				parentJar := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".jar")
				if _, err := os.Stat(parentJar); err == nil {
					return parentJar
				}
			}
		}
	}

	// Last resort: scan the version directory for any .jar
	entries, err := os.ReadDir(filepath.Join(versionsDir, v.ID))
	if err == nil {
		for _, e := range entries {
			if filepath.Ext(e.Name()) == ".jar" {
				return filepath.Join(versionsDir, v.ID, e.Name())
			}
		}
	}

	return ""
}

// prefetchForLaunch reads key files to warm the OS page cache before the JVM needs them.
// Runs in a goroutine so it overlaps with process startup. The OS caches file contents
// in memory after a read, so subsequent reads by the JVM hit RAM instead of disk.
func prefetchForLaunch(libs []minecraft.ResolvedLibrary, clientJar, mcDir, assetIndexID string) {
	buf := make([]byte, 256*1024) // 256KB read buffer

	touch := func(path string) {
		f, err := os.Open(path)
		if err != nil {
			return
		}
		defer f.Close()
		for {
			_, err := f.Read(buf)
			if err != nil {
				return
			}
		}
	}

	// Prefetch the client JAR (largest single file, ~20-40MB)
	touch(clientJar)

	// Prefetch library JARs (classpath entries the JVM scans at startup)
	for _, lib := range libs {
		if !lib.IsNative {
			touch(lib.AbsPath)
		}
	}

	// Prefetch the asset index (small but read early)
	if assetIndexID != "" {
		touch(filepath.Join(minecraft.AssetsDir(mcDir), "indexes", assetIndexID+".json"))
	}
}

// isModdedVersion checks the original (unmerged) version JSON to see if it inherits from another version.
func isModdedVersion(mcDir, versionID string) bool {
	origJSON := filepath.Join(minecraft.VersionsDir(mcDir), versionID, versionID+".json")
	data, err := os.ReadFile(origJSON)
	if err != nil {
		return false
	}
	var stub struct {
		InheritsFrom string `json:"inheritsFrom"`
	}
	if json.Unmarshal(data, &stub) == nil && stub.InheritsFrom != "" {
		return true
	}
	return false
}

func generateSessionID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}

// bootThrottleArgs returns JVM flags that limit concurrency during boot to prevent
// the JVM from overwhelming the system. These are always applied regardless of GC preset.
func bootThrottleArgs(javaMajor int) []string {
	// Determine a reasonable thread budget.
	// Leave at least 2 cores free for the OS and other applications.
	cpus := runtime.NumCPU()
	budget := cpus - 2
	if budget < 2 {
		budget = 2
	}

	// CICompilerCount: limits JIT compilation threads. Default is max(2, cores/2)
	// which causes a huge CPU spike during class loading. Cap it.
	ciThreads := budget / 2
	if ciThreads < 2 {
		ciThreads = 2
	}
	if ciThreads > 4 {
		ciThreads = 4
	}

	args := []string{
		fmt.Sprintf("-XX:CICompilerCount=%d", ciThreads),
	}

	// On Java 9+, limit parallel GC threads so collection doesn't steal all cores.
	// Only apply if user hasn't selected ZGC (which manages its own threads).
	if javaMajor >= 9 {
		gcThreads := budget
		if gcThreads > 6 {
			gcThreads = 6
		}
		args = append(args,
			fmt.Sprintf("-XX:ParallelGCThreads=%d", gcThreads),
			fmt.Sprintf("-XX:ConcGCThreads=%d", ciThreads),
		)
	}

	return args
}

// gcPresetArgs returns JVM garbage collector flags for the given preset.
func gcPresetArgs(preset string, javaMajor int) []string {
	switch preset {
	case "aikar":
		return []string{
			"-XX:+UseG1GC",
			"-XX:+ParallelRefProcEnabled",
			"-XX:MaxGCPauseMillis=200",
			"-XX:+UnlockExperimentalVMOptions",
			"-XX:+DisableExplicitGC",
			"-XX:G1NewSizePercent=30",
			"-XX:G1MaxNewSizePercent=40",
			"-XX:G1HeapRegionSize=8M",
			"-XX:G1ReservePercent=20",
			"-XX:G1HeapWastePercent=5",
			"-XX:G1MixedGCCountTarget=4",
			"-XX:InitiatingHeapOccupancyPercent=15",
			"-XX:G1MixedGCLiveThresholdPercent=90",
			"-XX:G1RSetUpdatingPauseTimePercent=5",
			"-XX:SurvivorRatio=32",
			"-XX:+PerfDisableSharedMem",
			"-XX:MaxTenuringThreshold=1",
		}
	case "zgc":
		if javaMajor < 17 {
			return nil
		}
		args := []string{"-XX:+UseZGC"}
		if javaMajor >= 21 {
			args = append(args, "-XX:+ZGenerational")
		}
		return args
	default:
		return nil
	}
}
