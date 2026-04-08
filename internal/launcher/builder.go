package launcher

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/performance"
	"github.com/mateoltd/croopor/internal/system"
)

// JVM GC preset names.
const (
	PresetSmooth          = "smooth"            // Shenandoah — default for Java 11+
	PresetPerformance     = "performance"       // brucethemoose client G1GC
	PresetUltraLowLatency = "ultra_low_latency" // Generational ZGC — Java 21+
	PresetGraalVM         = "graalvm"           // GraalVM-specific tuning
	PresetLegacy          = "legacy"            // conservative G1GC for Java 8
	PresetLegacyPvP       = "legacy_pvp"        // low-pause G1GC for 1.8.9 PvP
	PresetLegacyHeavy     = "legacy_heavy"      // tuned G1GC for heavy modpacks
)

type LaunchAuthMode string

const (
	LaunchAuthOffline       LaunchAuthMode = "offline"
	LaunchAuthAuthenticated LaunchAuthMode = "authenticated"
)

// LaunchOptions holds parameters for launching a version.
type LaunchOptions struct {
	VersionID          string
	InstanceID         string
	Username           string
	AuthMode           LaunchAuthMode
	AdvancedOverrides  bool
	MaxMemoryMB        int
	MinMemoryMB        int
	MCDir              string   // Shared .minecraft (assets, libraries, versions)
	GameDir            string   // Instance game dir (saves, mods, config). Falls back to MCDir if empty.
	ExtraJVMArgs       []string // Additional JVM args from instance overrides
	Loader             string
	CompositionMode    composition.CompositionMode
	PerformanceManager *performance.PerformanceManager
	Config             *config.Config

	ForcedPreset     string
	DisableCustomGC  bool
	ForceManagedJava bool
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
	Healing    HealingSummary
}

// BuildAndLaunch constructs the launch command and starts the game.
func BuildAndLaunch(opts LaunchOptions) (*LaunchResult, error) {
	sessionID := generateSessionID()
	healing := newHealingSummary(opts)
	attemptOpts := opts

	for attempt := 0; attempt < 2; attempt++ {
		ctx := newLaunchContext(attemptOpts)
		ctx.SessionID = sessionID
		ctx.Healing = &healing

		if err := runPipeline(ctx, defaultPipeline(attemptOpts.PerformanceManager)); err != nil {
			healing.FailureClass = classifyLaunchFailure(err, nil)
			if attempt == 0 {
				if recovery, ok := recoveryForFailure(healing.FailureClass, attemptOpts, ctx); ok {
					applyRecovery(&attemptOpts, recovery)
					recordRecovery(&healing, recovery)
					continue
				}
			}
			return nil, newLaunchError(err, healing)
		}

		switch ctx.Process.WaitForStartup(startupObservationWindow) {
		case startupStable, startupTimedOut:
			healing.FailureClass = ""
			return &LaunchResult{
				Command:    append([]string{ctx.JavaPath}, ctx.CmdArgs...),
				JavaPath:   ctx.JavaPath,
				Process:    ctx.Process,
				SessionID:  ctx.SessionID,
				NativesDir: ctx.NativesDir,
				VersionID:  opts.VersionID,
				InstanceID: opts.InstanceID,
				Healing:    healing,
			}, nil
		case startupExited:
			healing.FailureClass = classifyLaunchFailure(nil, ctx.Process)
			if attempt == 0 {
				if recovery, ok := recoveryForFailure(healing.FailureClass, attemptOpts, ctx); ok {
					applyRecovery(&attemptOpts, recovery)
					recordRecovery(&healing, recovery)
					continue
				}
			}
			return nil, newLaunchError(fmt.Errorf("launch failed during startup: %s", formatFailureClass(healing.FailureClass)), healing)
		}
	}

	return nil, newLaunchError(fmt.Errorf("launch failed during startup"), healing)
}

func findClientJar(mcDir string, v *minecraft.VersionJSON, originalVersionID string) string {
	versionsDir := minecraft.VersionsDir(mcDir)

	// For inherited versions, prefer the parent game jar first. Loader installers
	// often drop helper jars into the child version directory that should not be
	// treated as the vanilla client jar for ${classpath}.
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

	// Check the version's own directory after inherited parent fallback.
	jarPath := filepath.Join(versionsDir, v.ID, v.ID+".jar")
	if _, err := os.Stat(jarPath); err == nil {
		return jarPath
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

func usesModuleBootstrap(v *minecraft.VersionJSON) bool {
	if v == nil || v.Arguments == nil {
		return false
	}
	if v.MainClass == "cpw.mods.bootstraplauncher.BootstrapLauncher" {
		return true
	}
	hasModulePath := false
	hasAllModulePath := false
	for _, arg := range v.Arguments.JVM {
		for _, val := range arg.Value {
			switch val {
			case "-p", "--module-path":
				hasModulePath = true
			case "ALL-MODULE-PATH":
				hasAllModulePath = true
			}
		}
	}
	return hasModulePath && hasAllModulePath
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

// AutoSelectPreset chooses the best JVM GC preset based on the detected
// hardware profile, Java version, and JVM distribution.
func AutoSelectPreset(profile system.HardwareProfile, javaMajor int, dist system.JavaDistribution) string {
	info := system.JavaRuntimeInfo{Distribution: dist, Major: javaMajor}
	return autoSelectPresetForLaunch(profile, composition.VersionFamily(""), "vanilla", false, info)
}

func autoSelectPresetForLaunch(profile system.HardwareProfile, family composition.VersionFamily, loader string, isModded bool, info system.JavaRuntimeInfo) string {
	caps := runtimeCaps(info)
	if !caps.HotSpotTuning {
		return ""
	}
	if caps.GraalVM && info.Major >= 17 && !isModded {
		return PresetGraalVM
	}
	if info.Major <= 8 {
		return PresetLegacy
	}
	if family == composition.FamilyA || family == composition.FamilyB || family == composition.FamilyC {
		return PresetPerformance
	}
	if loader == "forge" || loader == "neoforge" || isModded {
		return PresetPerformance
	}
	if supportsGenerationalZGC(info) && profile.CPU.LogicalCores >= 8 && profile.TotalRAMMB >= 8192 {
		return PresetUltraLowLatency
	}
	if supportsShenandoah(info) {
		return PresetSmooth
	}
	return PresetPerformance
}

// gcPresetArgs returns JVM garbage collector flags for the given preset.
func gcPresetArgs(preset string, info system.JavaRuntimeInfo) []string {
	preset = sanitizePresetForLaunch(preset, composition.VersionFamily(""), "vanilla", false, info)
	caps := runtimeCaps(info)
	switch preset {
	case "aikar":
		return advancedG1Args(caps, info, 200, []string{
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
			"-XX:MaxTenuringThreshold=1",
		})

	case PresetSmooth:
		if !caps.Shenandoah {
			return gcPresetArgs(PresetPerformance, info)
		}
		args := []string{
			"-XX:+UseShenandoahGC",
			"-XX:ShenandoahGCHeuristics=compact",
			"-XX:+AlwaysPreTouch",
			"-XX:+DisableExplicitGC",
			"-XX:+PerfDisableSharedMem",
		}
		if caps.NUMA {
			args = append(args, "-XX:+UseNUMA")
		}
		if caps.BiasedLockingFlag {
			args = append(args, "-XX:-UseBiasedLocking")
		}
		return args

	case PresetPerformance:
		return conservativeG1Args(caps, 37)

	case PresetUltraLowLatency, "zgc":
		if !caps.ZGC {
			return gcPresetArgs(PresetPerformance, info)
		}
		args := []string{
			"-XX:+UseZGC",
			"-XX:+AlwaysPreTouch",
			"-XX:+DisableExplicitGC",
			"-XX:+PerfDisableSharedMem",
		}
		if caps.NUMA {
			args = append(args, "-XX:+UseNUMA")
		}
		if caps.GenerationalZGC {
			args = append(args, "-XX:+ZGenerational")
		}
		return args

	case PresetGraalVM:
		if !caps.GraalVM || info.Major < 17 {
			return gcPresetArgs(PresetPerformance, info)
		}
		return []string{
			"-XX:+UseG1GC",
			"-XX:+EnableJVMCI",
			"-XX:+UseJVMCICompiler",
			"-XX:-TieredCompilation",
			"-XX:ReservedCodeCacheSize=256M",
			"-XX:InitialCodeCacheSize=256M",
			"-XX:+AlwaysPreTouch",
			"-XX:+DisableExplicitGC",
			"-XX:MaxInlineLevel=15",
			"-XX:MaxInlineSize=270",
		}

	case PresetLegacy:
		if info.Major > 8 {
			return gcPresetArgs(PresetPerformance, info)
		}
		return conservativeG1Args(caps, 200)

	case PresetLegacyPvP:
		if info.Major > 8 {
			return gcPresetArgs(PresetPerformance, info)
		}
		return conservativeG1Args(caps, 15)

	case PresetLegacyHeavy:
		if info.Major > 8 {
			return gcPresetArgs(PresetPerformance, info)
		}
		return conservativeG1Args(caps, 100)

	default:
		return nil
	}
}

func sanitizePresetForLaunch(preset string, family composition.VersionFamily, loader string, isModded bool, info system.JavaRuntimeInfo) string {
	caps := runtimeCaps(info)
	if !caps.HotSpotTuning {
		return ""
	}
	switch preset {
	case PresetLegacy, PresetLegacyPvP, PresetLegacyHeavy:
		if info.Major > 8 {
			return PresetPerformance
		}
	case PresetSmooth:
		if family == composition.FamilyA || family == composition.FamilyB || family == composition.FamilyC || !caps.Shenandoah {
			return PresetPerformance
		}
	case PresetUltraLowLatency, "zgc":
		if !caps.ZGC {
			if info.Major <= 8 {
				return PresetLegacy
			}
			return PresetPerformance
		}
	case PresetGraalVM:
		if info.Distribution != system.JavaDistributionGraalVM || info.Major < 17 {
			return PresetPerformance
		}
	}

	if info.Major <= 8 {
		return PresetLegacy
	}
	if family == composition.FamilyA || family == composition.FamilyB || family == composition.FamilyC {
		if preset == PresetUltraLowLatency || preset == "zgc" || preset == PresetSmooth {
			return PresetPerformance
		}
	}
	if loader == "forge" || loader == "neoforge" || isModded {
		if preset == PresetUltraLowLatency {
			return PresetPerformance
		}
	}
	return preset
}

func supportsShenandoah(info system.JavaRuntimeInfo) bool {
	return runtimeCaps(info).Shenandoah
}

func supportsZGC(info system.JavaRuntimeInfo) bool {
	return runtimeCaps(info).ZGC
}

func supportsGenerationalZGC(info system.JavaRuntimeInfo) bool {
	return runtimeCaps(info).GenerationalZGC
}

func supportsHotSpotTuning(info system.JavaRuntimeInfo) bool {
	return runtimeCaps(info).HotSpotTuning
}

type runtimeCapabilities struct {
	HotSpotTuning     bool
	GraalVM           bool
	Shenandoah        bool
	ZGC               bool
	GenerationalZGC   bool
	NUMA              bool
	BiasedLockingFlag bool
	ExperimentalG1    bool
}

func runtimeCaps(info system.JavaRuntimeInfo) runtimeCapabilities {
	caps := runtimeCapabilities{
		HotSpotTuning:     info.Distribution != system.JavaDistributionOpenJ9,
		GraalVM:           info.Distribution == system.JavaDistributionGraalVM,
		NUMA:              info.Distribution != system.JavaDistributionOpenJ9 && info.Major >= 8,
		BiasedLockingFlag: info.Distribution != system.JavaDistributionOpenJ9 && info.Major > 0 && info.Major < 18,
		ExperimentalG1:    info.Distribution != system.JavaDistributionOpenJ9 && info.Major == 8,
	}
	if caps.HotSpotTuning && info.Major >= 17 && !caps.GraalVM {
		caps.Shenandoah = true
		caps.ZGC = true
	}
	if caps.ZGC && info.Major >= 21 && info.Major <= 23 {
		caps.GenerationalZGC = true
	}
	return caps
}

func conservativeG1Args(caps runtimeCapabilities, pauseMillis int) []string {
	if !caps.HotSpotTuning {
		return nil
	}
	args := []string{
		"-XX:+UseG1GC",
		"-XX:+ParallelRefProcEnabled",
		fmt.Sprintf("-XX:MaxGCPauseMillis=%d", pauseMillis),
		"-XX:+DisableExplicitGC",
		"-XX:+PerfDisableSharedMem",
	}
	if caps.NUMA {
		args = append(args, "-XX:+UseNUMA")
	}
	return args
}

func advancedG1Args(caps runtimeCapabilities, info system.JavaRuntimeInfo, pauseMillis int, tuning []string) []string {
	args := conservativeG1Args(caps, pauseMillis)
	if len(args) == 0 {
		return nil
	}
	if !caps.ExperimentalG1 {
		return args
	}
	args = append(args, "-XX:+UnlockExperimentalVMOptions")
	args = append(args, tuning...)
	if caps.BiasedLockingFlag {
		args = append(args, "-XX:-UseBiasedLocking")
	}
	if info.Major >= 11 {
		args = append(args, "-XX:+AlwaysPreTouch")
	}
	return args
}
