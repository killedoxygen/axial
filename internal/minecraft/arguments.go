package minecraft

import (
	"crypto/md5"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// LaunchVars holds all the template variables for argument substitution.
type LaunchVars struct {
	AuthPlayerName string
	VersionName    string
	GameDirectory  string
	AssetsRoot     string
	AssetIndexName string
	AuthUUID       string
	AuthAccessToken string
	ClientID       string
	AuthXUID       string
	UserType       string
	VersionType    string
	LauncherName   string
	LauncherVersion string
	NativesDirectory string
	Classpath      string
	LibraryDirectory string
	ClasspathSeparator string
	ResolutionWidth  string
	ResolutionHeight string
	GameAssets       string // for pre-1.6 versions: assets/virtual/legacy/
}

// BuildVarMap returns the template variable map for argument substitution.
func (lv *LaunchVars) BuildVarMap() map[string]string {
	return map[string]string{
		"auth_player_name":  lv.AuthPlayerName,
		"version_name":      lv.VersionName,
		"game_directory":    lv.GameDirectory,
		"assets_root":       lv.AssetsRoot,
		"assets_index_name": lv.AssetIndexName,
		"auth_uuid":         lv.AuthUUID,
		"auth_access_token": lv.AuthAccessToken,
		"clientid":          lv.ClientID,
		"auth_xuid":         lv.AuthXUID,
		"user_type":         lv.UserType,
		"version_type":      lv.VersionType,
		"launcher_name":     lv.LauncherName,
		"launcher_version":  lv.LauncherVersion,
		"natives_directory":  lv.NativesDirectory,
		"classpath":         lv.Classpath,
		"library_directory":  lv.LibraryDirectory,
		"classpath_separator": lv.ClasspathSeparator,
		"resolution_width":  lv.ResolutionWidth,
		"resolution_height": lv.ResolutionHeight,
		// Some versions use these alternate names
		"game_assets":       lv.gameAssetsDir(),
		"user_properties":   "{}",
	}
}

func (lv *LaunchVars) gameAssetsDir() string {
	if lv.GameAssets != "" {
		return lv.GameAssets
	}
	return lv.AssetsRoot
}

// ResolveArguments processes the version's arguments, evaluates rules,
// substitutes template variables, and strips --demo.
func ResolveArguments(v *VersionJSON, env Environment, vars *LaunchVars) (jvmArgs []string, gameArgs []string) {
	varMap := vars.BuildVarMap()

	if v.IsLegacyVersion() {
		gameArgs = resolveLegacyArgs(v.MinecraftArguments, varMap)
		jvmArgs = defaultLegacyJVMArgs(varMap)
	} else if v.Arguments != nil {
		jvmArgs = resolveArgList(v.Arguments.JVM, env, varMap)
		gameArgs = resolveArgList(v.Arguments.Game, env, varMap)
	}

	// Add logging config if present
	if v.Logging != nil && v.Logging.Client != nil {
		logArg := resolveLoggingArg(v.Logging.Client, vars.GameDirectory)
		if logArg != "" {
			jvmArgs = append(jvmArgs, logArg)
		}
	}

	// Safety net: strip any --demo that slipped through
	gameArgs = stripDemo(gameArgs)

	return jvmArgs, gameArgs
}

func resolveArgList(args []Argument, env Environment, varMap map[string]string) []string {
	var result []string
	for _, arg := range args {
		if !EvaluateRules(arg.Rules, env) {
			continue
		}
		for _, val := range arg.Value {
			result = append(result, substituteVars(val, varMap))
		}
	}
	return result
}

func resolveLegacyArgs(minecraftArgs string, varMap map[string]string) []string {
	parts := strings.Fields(minecraftArgs)
	result := make([]string, 0, len(parts))
	for _, part := range parts {
		result = append(result, substituteVars(part, varMap))
	}
	return result
}

func defaultLegacyJVMArgs(varMap map[string]string) []string {
	return []string{
		"-Djava.library.path=" + varMap["natives_directory"],
		"-Dminecraft.launcher.brand=" + varMap["launcher_name"],
		"-Dminecraft.launcher.version=" + varMap["launcher_version"],
		"-cp",
		varMap["classpath"],
	}
}

func resolveLoggingArg(entry *LoggingEntry, gameDir string) string {
	if entry.Argument == "" || entry.File.ID == "" {
		return ""
	}

	// The log config file is typically in assets/log_configs/
	mcDir := filepath.Dir(gameDir)
	if gameDir == mcDir {
		mcDir = gameDir
	}
	logConfigPath := filepath.Join(gameDir, "assets", "log_configs", entry.File.ID)

	// Check if file exists; if gameDir IS .minecraft, the path is correct
	// If not, try the standard .minecraft location
	if _, err := os.Stat(logConfigPath); os.IsNotExist(err) {
		// gameDir might be .minecraft itself
		logConfigPath = filepath.Join(gameDir, "assets", "log_configs", entry.File.ID)
	}

	// The argument template is like "-Dlog4j.configurationFile=${path}"
	return strings.ReplaceAll(entry.Argument, "${path}", logConfigPath)
}

func substituteVars(s string, varMap map[string]string) string {
	result := s
	for key, val := range varMap {
		result = strings.ReplaceAll(result, "${"+key+"}", val)
	}
	return result
}

// stripDemo removes --demo and any standalone "demo" arguments.
func stripDemo(args []string) []string {
	result := make([]string, 0, len(args))
	for _, arg := range args {
		if arg == "--demo" {
			continue
		}
		result = append(result, arg)
	}
	return result
}

// OfflineUUID generates a deterministic UUID v3 from "OfflinePlayer:" + username,
// matching how Minecraft servers generate offline UUIDs.
func OfflineUUID(username string) string {
	data := []byte("OfflinePlayer:" + username)
	hash := md5.Sum(data)

	// Set version 3 (MD5) and variant bits
	hash[6] = (hash[6] & 0x0f) | 0x30 // version 3
	hash[8] = (hash[8] & 0x3f) | 0x80 // variant 10

	return fmt.Sprintf("%x%x%x%x%x",
		hash[0:4], hash[4:6], hash[6:8], hash[8:10], hash[10:16])
}
