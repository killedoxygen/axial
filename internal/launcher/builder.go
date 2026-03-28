package launcher

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
)

// LaunchOptions holds parameters for launching a version.
type LaunchOptions struct {
	VersionID   string
	Username    string
	MaxMemoryMB int
	MinMemoryMB int
	MCDir       string
	Config      *config.Config
}

// LaunchResult contains the constructed command and game process.
type LaunchResult struct {
	Command     []string
	JavaPath    string
	Process     *GameProcess
	SessionID   string
	NativesDir  string
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

	// Step 3: Find Java runtime
	javaResult, err := minecraft.FindJava(opts.MCDir, version.JavaVersion, opts.Config.JavaPathOverride)
	if err != nil {
		return nil, fmt.Errorf("finding java: %w", err)
	}

	// Step 4: Resolve libraries and build classpath
	libs, err := minecraft.ResolveLibraries(version, opts.MCDir, env)
	if err != nil {
		return nil, fmt.Errorf("resolving libraries: %w", err)
	}

	// Step 5: Determine client JAR path
	clientJarPath := findClientJar(opts.MCDir, version)

	classpath := minecraft.BuildClasspath(libs, clientJarPath)

	// Step 6: Create natives directory
	nativesDir, err := CreateNativesDir(sessionID)
	if err != nil {
		return nil, fmt.Errorf("creating natives dir: %w", err)
	}

	// Step 7: Extract legacy natives if needed
	if version.IsLegacyVersion() {
		if err := ExtractLegacyNatives(libs, nativesDir); err != nil {
			CleanupNativesDir(nativesDir)
			return nil, fmt.Errorf("extracting natives: %w", err)
		}
	}

	// Step 8: Build launch variables
	username := opts.Username
	if username == "" {
		username = "Player"
	}

	gameDir := opts.MCDir
	if opts.Config.JavaPathOverride != "" {
		// Keep using .minecraft as game dir
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
		LauncherName:       "paralauncher",
		LauncherVersion:    "1.0.0",
		NativesDirectory:   nativesDir,
		Classpath:          classpath,
		LibraryDirectory:   minecraft.LibrariesDir(opts.MCDir),
		ClasspathSeparator: string(os.PathListSeparator),
	}

	if opts.Config.WindowWidth > 0 && opts.Config.WindowHeight > 0 {
		vars.ResolutionWidth = fmt.Sprintf("%d", opts.Config.WindowWidth)
		vars.ResolutionHeight = fmt.Sprintf("%d", opts.Config.WindowHeight)
		env.Features["has_custom_resolution"] = true
	}

	// Step 9: Resolve JVM and game arguments
	jvmArgs, gameArgs := minecraft.ResolveArguments(version, env, vars)

	// Step 10: Add memory flags
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

	// Step 11: Assemble full command
	// Order: java [jvm_args] [mem_args] <mainClass> [game_args]
	var cmdArgs []string
	cmdArgs = append(cmdArgs, jvmArgs...)
	cmdArgs = append(cmdArgs, memArgs...)
	cmdArgs = append(cmdArgs, version.MainClass)
	cmdArgs = append(cmdArgs, gameArgs...)

	// Step 12: Create exec.Cmd
	cmd := exec.Command(javaResult.Path, cmdArgs...)
	cmd.Dir = gameDir

	// Set up process attributes for Windows (detach so game survives if paralauncher exits)
	setProcAttr(cmd)

	result := &LaunchResult{
		Command:    append([]string{javaResult.Path}, cmdArgs...),
		JavaPath:   javaResult.Path,
		SessionID:  sessionID,
		NativesDir: nativesDir,
	}

	// Step 13: Create and start game process
	gp := NewGameProcess(cmd, nativesDir)
	if err := gp.Start(); err != nil {
		CleanupNativesDir(nativesDir)
		return nil, fmt.Errorf("starting game process: %w", err)
	}
	result.Process = gp

	return result, nil
}

func findClientJar(mcDir string, v *minecraft.VersionJSON) string {
	// For modded versions, the client JAR comes from the parent (vanilla) version
	versionID := v.ID

	// Check if there's a JAR matching the version ID
	jarPath := filepath.Join(minecraft.VersionsDir(mcDir), versionID, versionID+".jar")
	if _, err := os.Stat(jarPath); err == nil {
		return jarPath
	}

	// For versions with inheritsFrom that was already merged, try the original parent
	// The ID in a merged version is the child's ID. The parent's JAR might have a different name.
	// Try common patterns
	entries, err := os.ReadDir(filepath.Join(minecraft.VersionsDir(mcDir), versionID))
	if err == nil {
		for _, e := range entries {
			if filepath.Ext(e.Name()) == ".jar" {
				return filepath.Join(minecraft.VersionsDir(mcDir), versionID, e.Name())
			}
		}
	}

	return ""
}

func generateSessionID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}
