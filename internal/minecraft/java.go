package minecraft

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

var ErrJavaNotFound = errors.New("java runtime not found")

const runtimeManifestURL = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json"

type JavaResult struct {
	Path      string `json:"path"`
	Component string `json:"component"`
	Source    string `json:"source"`
}

// FindJava searches for the EXACT Java runtime component required by a version.
// It will NOT fall back to a different component. The wrong Java version breaks launch.
// If the runtime is not found locally, it returns ErrJavaNotFound.
func FindJava(mcDir string, javaVersion JavaVersion, overridePath string) (*JavaResult, error) {
	if overridePath != "" {
		if _, err := os.Stat(overridePath); err == nil {
			return &JavaResult{Path: overridePath, Component: javaVersion.Component, Source: "override"}, nil
		}
	}

	component := javaVersion.Component
	if component == "" {
		component = "java-runtime-delta"
	}

	// Search ONLY for the exact component. No fallbacks.
	for _, dir := range RuntimeDirs(mcDir) {
		result := searchExactRuntime(dir, component)
		if result != nil {
			return result, nil
		}
	}

	// Also check our own downloaded runtimes
	crooportDir := runtimeCacheDir()
	if crooportDir != "" {
		result := searchExactRuntime(crooportDir, component)
		if result != nil {
			return result, nil
		}
	}

	return nil, fmt.Errorf("%w: %s (Java %d) not installed", ErrJavaNotFound, component, javaVersion.MajorVersion)
}

func searchExactRuntime(baseDir, component string) *JavaResult {
	if _, err := os.Stat(baseDir); os.IsNotExist(err) {
		return nil
	}

	osArch := runtimeOSArch()

	// Pattern 1: <baseDir>/<component>/<os-arch>/<component>/bin/javaw.exe
	javaExe := javaExecutable(filepath.Join(baseDir, component, osArch, component), "bin")
	if _, err := os.Stat(javaExe); err == nil {
		source := "minecraft-runtime"
		if strings.Contains(baseDir, "Packages") {
			source = "ms-store"
		} else if strings.Contains(baseDir, "croopor") {
			source = "croopor"
		}
		return &JavaResult{Path: javaExe, Component: component, Source: source}
	}

	// Pattern 2: <baseDir>/<component>/bin/javaw.exe
	javaExe = javaExecutable(filepath.Join(baseDir, component), "bin")
	if _, err := os.Stat(javaExe); err == nil {
		source := "minecraft-runtime"
		if strings.Contains(baseDir, "Packages") {
			source = "ms-store"
		} else if strings.Contains(baseDir, "croopor") {
			source = "croopor"
		}
		return &JavaResult{Path: javaExe, Component: component, Source: source}
	}

	return nil
}

// EnsureJavaRuntime downloads the required Java runtime if not found locally.
// Returns the path to the Java executable.
func EnsureJavaRuntime(mcDir string, javaVersion JavaVersion, overridePath string) (*JavaResult, error) {
	// Try to find it first
	result, err := FindJava(mcDir, javaVersion, overridePath)
	if err == nil {
		return result, nil
	}

	component := javaVersion.Component
	if component == "" {
		component = "java-runtime-delta"
	}

	// Download it
	destDir := filepath.Join(runtimeCacheDir(), component)
	if err := downloadRuntime(component, destDir); err != nil {
		return nil, fmt.Errorf("downloading %s: %w", component, err)
	}

	// Try again after download
	return FindJava(mcDir, javaVersion, overridePath)
}

// runtimeCacheDir returns where Croopor stores its own downloaded runtimes.
func runtimeCacheDir() string {
	if runtime.GOOS == "windows" {
		appdata := os.Getenv("APPDATA")
		if appdata != "" {
			return filepath.Join(appdata, "croopor", "runtimes")
		}
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".croopor", "runtimes")
}

// downloadRuntime fetches a Java runtime from Mojang's servers.
func downloadRuntime(component, destDir string) error {
	osKey := runtimeOSArch()

	// Step 1: Fetch the all-runtimes manifest
	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Get(runtimeManifestURL)
	if err != nil {
		return fmt.Errorf("fetching runtime manifest: %w", err)
	}
	defer resp.Body.Close()

	var allRuntimes map[string]map[string][]struct {
		Availability struct {
			Group    int `json:"group"`
			Progress int `json:"progress"`
		} `json:"availability"`
		Manifest struct {
			SHA1 string `json:"sha1"`
			Size int64  `json:"size"`
			URL  string `json:"url"`
		} `json:"manifest"`
		Version struct {
			Name     string `json:"name"`
			Released string `json:"released"`
		} `json:"version"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&allRuntimes); err != nil {
		return fmt.Errorf("parsing runtime manifest: %w", err)
	}

	// Find our component + OS
	osRuntimes, ok := allRuntimes[osKey]
	if !ok {
		return fmt.Errorf("no runtimes available for %s", osKey)
	}

	entries, ok := osRuntimes[component]
	if !ok || len(entries) == 0 {
		return fmt.Errorf("runtime %s not available for %s", component, osKey)
	}

	manifestURL := entries[0].Manifest.URL

	// Step 2: Fetch the component manifest (lists all files to download)
	resp2, err := client.Get(manifestURL)
	if err != nil {
		return fmt.Errorf("fetching component manifest: %w", err)
	}
	defer resp2.Body.Close()

	var compManifest struct {
		Files map[string]struct {
			Type       string `json:"type"`
			Executable bool   `json:"executable"`
			Downloads  struct {
				Raw struct {
					SHA1 string `json:"sha1"`
					Size int64  `json:"size"`
					URL  string `json:"url"`
				} `json:"raw"`
			} `json:"downloads"`
		} `json:"files"`
	}
	if err := json.NewDecoder(resp2.Body).Decode(&compManifest); err != nil {
		return fmt.Errorf("parsing component manifest: %w", err)
	}

	// Step 3: Download all files
	dlClient := &http.Client{Timeout: 5 * time.Minute}
	for path, entry := range compManifest.Files {
		destPath := filepath.Join(destDir, filepath.FromSlash(path))

		if entry.Type == "directory" {
			os.MkdirAll(destPath, 0755)
			continue
		}

		if entry.Type != "file" || entry.Downloads.Raw.URL == "" {
			continue
		}

		// Skip if already exists
		if _, err := os.Stat(destPath); err == nil {
			continue
		}

		os.MkdirAll(filepath.Dir(destPath), 0755)

		r, err := dlClient.Get(entry.Downloads.Raw.URL)
		if err != nil {
			continue
		}
		f, err := os.Create(destPath)
		if err != nil {
			r.Body.Close()
			continue
		}
		io.Copy(f, r.Body)
		f.Close()
		r.Body.Close()

		// Mark executable on unix
		if entry.Executable && runtime.GOOS != "windows" {
			os.Chmod(destPath, 0755)
		}
	}

	return nil
}

func javaExecutable(baseDir, binDir string) string {
	if runtime.GOOS == "windows" {
		return filepath.Join(baseDir, binDir, "javaw.exe")
	}
	return filepath.Join(baseDir, binDir, "java")
}

func runtimeOSArch() string {
	var osName string
	switch runtime.GOOS {
	case "windows":
		osName = "windows"
	case "darwin":
		osName = "mac-os"
	default:
		osName = "linux"
	}

	var archName string
	switch runtime.GOARCH {
	case "amd64":
		archName = "x64"
	case "386":
		archName = "x86"
	case "arm64":
		archName = "arm64"
	default:
		archName = runtime.GOARCH
	}

	return osName + "-" + archName
}

func ListJavaRuntimes(mcDir string) []JavaResult {
	var results []JavaResult
	components := []string{
		"java-runtime-epsilon", "java-runtime-delta", "java-runtime-gamma",
		"java-runtime-beta", "java-runtime-alpha", "jre-legacy",
	}

	dirs := RuntimeDirs(mcDir)
	if cd := runtimeCacheDir(); cd != "" {
		dirs = append(dirs, cd)
	}

	for _, dir := range dirs {
		for _, comp := range components {
			result := searchExactRuntime(dir, comp)
			if result != nil {
				results = append(results, *result)
			}
		}
	}
	return results
}
