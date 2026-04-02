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

// LaunchOptions holds parameters for launching a version.
type LaunchOptions struct {
	VersionID       string
	InstanceID      string
	Username        string
	MaxMemoryMB     int
	MinMemoryMB     int
	MCDir           string   // Shared .minecraft (assets, libraries, versions)
	GameDir         string   // Instance game dir (saves, mods, config). Falls back to MCDir if empty.
	ExtraJVMArgs    []string // Additional JVM args from instance overrides
	Loader          string
	CompositionMode composition.CompositionMode
	Config          *config.Config
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
	ctx := newLaunchContext(opts)

	if err := runPipeline(ctx, defaultPipeline()); err != nil {
		return nil, err
	}

	return &LaunchResult{
		Command:    append([]string{ctx.JavaPath}, ctx.CmdArgs...),
		JavaPath:   ctx.JavaPath,
		Process:    ctx.Process,
		SessionID:  ctx.SessionID,
		NativesDir: ctx.NativesDir,
		VersionID:  opts.VersionID,
		InstanceID: opts.InstanceID,
	}, nil
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

// AutoSelectPreset chooses the best JVM GC preset based on the detected
// hardware profile, Java version, and JVM distribution. It is called only
// when the user has not explicitly set a preset in config.
func AutoSelectPreset(profile system.HardwareProfile, javaMajor int, dist system.JavaDistribution) string {
	if dist == system.JavaDistributionGraalVM {
		return PresetGraalVM
	}
	if javaMajor <= 8 {
		return PresetLegacy
	}
	if javaMajor >= 21 && profile.CPU.LogicalCores >= 8 && profile.TotalRAMMB >= 8192 {
		return PresetUltraLowLatency
	}
	if javaMajor >= 11 {
		return PresetSmooth
	}
	return PresetPerformance
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

	case PresetSmooth:
		// Shenandoah requires Java 11+; fall back to performance if unavailable.
		if javaMajor < 11 {
			return gcPresetArgs(PresetPerformance, javaMajor)
		}
		return []string{
			"-XX:+UseShenandoahGC",
			"-XX:ShenandoahGCHeuristics=compact",
			"-XX:+AlwaysPreTouch",
			"-XX:+DisableExplicitGC",
			"-XX:+UseNUMA",
			"-XX:-UseBiasedLocking",
			"-XX:+PerfDisableSharedMem",
		}

	case PresetPerformance:
		return []string{
			"-XX:+UseG1GC",
			"-XX:MaxGCPauseMillis=37",
			"-XX:+PerfDisableSharedMem",
			"-XX:+AlwaysPreTouch",
			"-XX:-UseAdaptiveSizePolicy",
			"-XX:G1NewSizePercent=20",
			"-XX:G1MaxNewSizePercent=40",
			"-XX:G1HeapRegionSize=16M",
			"-XX:G1ReservePercent=20",
			"-XX:G1MixedGCCountTarget=3",
			"-XX:InitiatingHeapOccupancyPercent=15",
			"-XX:G1MixedGCLiveThresholdPercent=90",
			"-XX:G1RSetUpdatingPauseTimePercent=0",
			"-XX:SurvivorRatio=32",
			"-XX:MaxTenuringThreshold=1",
			"-XX:+UseNUMA",
			"-XX:-DontCompileHugeMethods",
			"-XX:+DisableExplicitGC",
			"-XX:-UseBiasedLocking",
		}

	case PresetUltraLowLatency, "zgc":
		// Generational ZGC requires Java 21+; fall back through smooth then performance.
		if javaMajor < 21 {
			return gcPresetArgs(PresetSmooth, javaMajor)
		}
		return []string{
			"-XX:+UseZGC",
			"-XX:+ZGenerational",
			"-XX:+AlwaysPreTouch",
			"-XX:+DisableExplicitGC",
			"-XX:+PerfDisableSharedMem",
			"-XX:+UseNUMA",
		}

	case PresetGraalVM:
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
		return []string{
			"-XX:+UseG1GC",
			"-XX:+ParallelRefProcEnabled",
			"-XX:MaxGCPauseMillis=200",
			"-XX:+UnlockExperimentalVMOptions",
			"-XX:+DisableExplicitGC",
			"-XX:G1NewSizePercent=20",
			"-XX:G1MaxNewSizePercent=40",
			"-XX:G1HeapRegionSize=8M",
			"-XX:G1ReservePercent=20",
			"-XX:InitiatingHeapOccupancyPercent=15",
			"-XX:G1MixedGCLiveThresholdPercent=90",
			"-XX:G1RSetUpdatingPauseTimePercent=5",
			"-XX:SurvivorRatio=32",
			"-XX:MaxTenuringThreshold=1",
			"-XX:+PerfDisableSharedMem",
		}

	case PresetLegacyPvP:
		return []string{
			"-XX:+UseG1GC",
			"-XX:MaxGCPauseMillis=15",
			"-XX:+ParallelRefProcEnabled",
			"-XX:+UnlockExperimentalVMOptions",
			"-XX:+DisableExplicitGC",
			"-XX:G1NewSizePercent=20",
			"-XX:G1MaxNewSizePercent=40",
			"-XX:G1HeapRegionSize=4M",
			"-XX:G1ReservePercent=20",
			"-XX:InitiatingHeapOccupancyPercent=20",
			"-XX:G1MixedGCLiveThresholdPercent=85",
			"-XX:SurvivorRatio=32",
			"-XX:MaxTenuringThreshold=1",
			"-XX:+PerfDisableSharedMem",
		}

	case PresetLegacyHeavy:
		return []string{
			"-XX:+UseG1GC",
			"-XX:+ParallelRefProcEnabled",
			"-XX:MaxGCPauseMillis=100",
			"-XX:+UnlockExperimentalVMOptions",
			"-XX:+DisableExplicitGC",
			"-XX:G1NewSizePercent=30",
			"-XX:G1MaxNewSizePercent=50",
			"-XX:G1HeapRegionSize=32M",
			"-XX:G1ReservePercent=20",
			"-XX:G1HeapWastePercent=5",
			"-XX:G1MixedGCCountTarget=4",
			"-XX:InitiatingHeapOccupancyPercent=15",
			"-XX:G1MixedGCLiveThresholdPercent=90",
			"-XX:G1RSetUpdatingPauseTimePercent=5",
			"-XX:SurvivorRatio=32",
			"-XX:MaxTenuringThreshold=1",
			"-XX:+PerfDisableSharedMem",
			"-XX:+AlwaysPreTouch",
		}

	default:
		return nil
	}
}
