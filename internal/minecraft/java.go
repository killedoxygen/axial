package minecraft

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

var ErrJavaNotFound = errors.New("java runtime not found")

// JavaResult holds the path to a discovered Java executable.
type JavaResult struct {
	Path      string
	Component string // which runtime component it belongs to (e.g., "java-runtime-delta")
	Source    string // where it was found (e.g., "ms-store", "minecraft-runtime", "system")
}

// FindJava searches for a Java executable suitable for the given version.
func FindJava(mcDir string, javaVersion JavaVersion, overridePath string) (*JavaResult, error) {
	// 1. Check explicit override
	if overridePath != "" {
		if _, err := os.Stat(overridePath); err == nil {
			return &JavaResult{
				Path:      overridePath,
				Component: javaVersion.Component,
				Source:    "override",
			}, nil
		}
	}

	// 2. Search known runtime directories
	component := javaVersion.Component
	if component == "" {
		component = "java-runtime-delta" // reasonable default for modern versions
	}

	for _, dir := range RuntimeDirs(mcDir) {
		result := searchRuntimeDir(dir, component)
		if result != nil {
			return result, nil
		}
	}

	// 3. Check JAVA_HOME
	javaHome := os.Getenv("JAVA_HOME")
	if javaHome != "" {
		javaExe := javaExecutable(javaHome, "bin")
		if _, err := os.Stat(javaExe); err == nil {
			return &JavaResult{
				Path:      javaExe,
				Component: component,
				Source:    "JAVA_HOME",
			}, nil
		}
	}

	// 4. Check PATH
	exeName := "java"
	if runtime.GOOS == "windows" {
		exeName = "javaw.exe"
	}
	if path, err := exec.LookPath(exeName); err == nil {
		return &JavaResult{
			Path:      path,
			Component: component,
			Source:    "PATH",
		}, nil
	}

	// Also try java.exe on Windows if javaw.exe not found
	if runtime.GOOS == "windows" {
		if path, err := exec.LookPath("java.exe"); err == nil {
			return &JavaResult{
				Path:      path,
				Component: component,
				Source:    "PATH",
			}, nil
		}
	}

	return nil, fmt.Errorf("%w: need %s (Java %d)", ErrJavaNotFound, component, javaVersion.MajorVersion)
}

func searchRuntimeDir(baseDir string, component string) *JavaResult {
	if _, err := os.Stat(baseDir); os.IsNotExist(err) {
		return nil
	}

	// Try the exact component first
	candidates := []string{component}

	// Also try common runtime names as fallbacks
	fallbacks := []string{
		"java-runtime-epsilon",
		"java-runtime-delta",
		"java-runtime-gamma",
		"java-runtime-beta",
		"java-runtime-alpha",
		"jre-legacy",
	}
	for _, fb := range fallbacks {
		if fb != component {
			candidates = append(candidates, fb)
		}
	}

	osArch := runtimeOSArch()

	for _, comp := range candidates {
		// Pattern: <baseDir>/<component>/<os-arch>/<component>/bin/java[w.exe]
		javaDir := filepath.Join(baseDir, comp, osArch, comp)
		javaExe := javaExecutable(javaDir, "bin")
		if _, err := os.Stat(javaExe); err == nil {
			source := "minecraft-runtime"
			if strings.Contains(baseDir, "Packages") {
				source = "ms-store"
			}
			return &JavaResult{
				Path:      javaExe,
				Component: comp,
				Source:    source,
			}
		}

		// Some runtimes have the binaries directly in <baseDir>/<component>/bin/
		javaExe = javaExecutable(filepath.Join(baseDir, comp), "bin")
		if _, err := os.Stat(javaExe); err == nil {
			source := "minecraft-runtime"
			if strings.Contains(baseDir, "Packages") {
				source = "ms-store"
			}
			return &JavaResult{
				Path:      javaExe,
				Component: comp,
				Source:    source,
			}
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
	os := runtime.GOOS
	arch := runtime.GOARCH

	var osName string
	switch os {
	case "windows":
		osName = "windows"
	case "darwin":
		osName = "mac-os"
	default:
		osName = "linux"
	}

	var archName string
	switch arch {
	case "amd64":
		archName = "x64"
	case "386":
		archName = "x86"
	case "arm64":
		archName = "arm64"
	default:
		archName = arch
	}

	return osName + "-" + archName
}

// ListJavaRuntimes returns all Java runtimes found on the system.
func ListJavaRuntimes(mcDir string) []JavaResult {
	var results []JavaResult

	components := []string{
		"java-runtime-epsilon",
		"java-runtime-delta",
		"java-runtime-gamma",
		"java-runtime-beta",
		"java-runtime-alpha",
		"jre-legacy",
	}

	for _, dir := range RuntimeDirs(mcDir) {
		for _, comp := range components {
			result := searchRuntimeDir(dir, comp)
			if result != nil {
				results = append(results, *result)
			}
		}
	}

	return results
}
