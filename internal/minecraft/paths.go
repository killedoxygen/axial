package minecraft

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
)

var (
	ErrMinecraftNotFound = errors.New("minecraft installation not found")
	ErrInvalidInstall    = errors.New("minecraft installation is missing required directories")
)

// DetectMinecraftDir finds the .minecraft directory for the current OS.
func DetectMinecraftDir() (string, error) {
	var dir string
	switch runtime.GOOS {
	case "windows":
		appdata := os.Getenv("APPDATA")
		if appdata == "" {
			return "", ErrMinecraftNotFound
		}
		dir = filepath.Join(appdata, ".minecraft")
	case "darwin":
		home, err := os.UserHomeDir()
		if err != nil {
			return "", ErrMinecraftNotFound
		}
		dir = filepath.Join(home, "Library", "Application Support", "minecraft")
	default: // linux
		home, err := os.UserHomeDir()
		if err != nil {
			return "", ErrMinecraftNotFound
		}
		dir = filepath.Join(home, ".minecraft")
	}

	if _, err := os.Stat(dir); os.IsNotExist(err) {
		return "", ErrMinecraftNotFound
	}
	return dir, nil
}

// ValidateInstallation checks that the .minecraft directory has required subdirectories.
func ValidateInstallation(mcDir string) error {
	required := []string{"versions", "libraries", "assets"}
	for _, sub := range required {
		path := filepath.Join(mcDir, sub)
		info, err := os.Stat(path)
		if err != nil || !info.IsDir() {
			return ErrInvalidInstall
		}
	}
	return nil
}

// VersionsDir returns the path to the versions directory.
func VersionsDir(mcDir string) string {
	return filepath.Join(mcDir, "versions")
}

// LibrariesDir returns the path to the libraries directory.
func LibrariesDir(mcDir string) string {
	return filepath.Join(mcDir, "libraries")
}

// AssetsDir returns the path to the assets directory.
func AssetsDir(mcDir string) string {
	return filepath.Join(mcDir, "assets")
}

// DefaultMinecraftDir returns the default .minecraft path for the current OS
// without checking whether it exists.
func DefaultMinecraftDir() string {
	switch runtime.GOOS {
	case "windows":
		appdata := os.Getenv("APPDATA")
		if appdata != "" {
			return filepath.Join(appdata, ".minecraft")
		}
	case "darwin":
		home, err := os.UserHomeDir()
		if err == nil {
			return filepath.Join(home, "Library", "Application Support", "minecraft")
		}
	default:
		home, err := os.UserHomeDir()
		if err == nil {
			return filepath.Join(home, ".minecraft")
		}
	}
	return ""
}

// IsLegacyAssets checks if the asset index uses the legacy/virtual format (pre-1.6).
// These versions expect assets at assets/virtual/legacy/<path> instead of assets/objects/.
func IsLegacyAssets(mcDir string, assetIndexID string) bool {
	indexPath := filepath.Join(AssetsDir(mcDir), "indexes", assetIndexID+".json")
	data, err := os.ReadFile(indexPath)
	if err != nil {
		return false
	}
	var idx struct {
		Virtual        bool `json:"virtual"`
		MapToResources bool `json:"map_to_resources"`
	}
	json.Unmarshal(data, &idx)
	return idx.Virtual || idx.MapToResources
}

// CreateMinecraftDir creates a .minecraft directory structure at the given path.
func CreateMinecraftDir(dir string) error {
	for _, sub := range []string{"versions", "libraries", "assets"} {
		if err := os.MkdirAll(filepath.Join(dir, sub), 0755); err != nil {
			return fmt.Errorf("creating %s: %w", sub, err)
		}
	}
	return nil
}

// RuntimeDir returns possible paths for Java runtimes.
// The MS Store launcher keeps runtimes in a different location than the standalone launcher.
func RuntimeDirs(mcDir string) []string {
	dirs := []string{
		filepath.Join(mcDir, "runtime"),
	}
	if runtime.GOOS == "windows" {
		localAppData := os.Getenv("LOCALAPPDATA")
		if localAppData != "" {
			// MS Store Minecraft launcher runtime path
			msStore := filepath.Join(localAppData,
				"Packages", "Microsoft.4297127D64EC6_8wekyb3d8bbwe",
				"LocalCache", "Local", "runtime")
			dirs = append(dirs, msStore)
		}
	}
	return dirs
}
