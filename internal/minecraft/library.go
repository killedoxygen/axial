package minecraft

import (
	"os"
	"path/filepath"
	"strings"
)

// ResolvedLibrary holds a resolved library path and whether it's a native.
type ResolvedLibrary struct {
	AbsPath  string
	IsNative bool
	Name     string
}

// ResolveLibraries filters and resolves all library paths for the given version.
func ResolveLibraries(v *VersionJSON, mcDir string, env Environment) ([]ResolvedLibrary, error) {
	libDir := LibrariesDir(mcDir)
	var resolved []ResolvedLibrary

	for _, lib := range v.Libraries {
		// Check rules
		if !EvaluateRules(lib.Rules, env) {
			continue
		}

		isNative := IsNativeLibrary(lib.Name)

		// Legacy natives handling: library has "natives" map with classifiers
		if lib.Natives != nil {
			nativeLibs := resolveLegacyNatives(lib, libDir, env)
			resolved = append(resolved, nativeLibs...)
			// If this library ONLY provides natives (no artifact), skip artifact resolution
			if lib.Downloads != nil && lib.Downloads.Artifact == nil {
				continue
			}
		}

		// Pattern A: has downloads.artifact.path
		if lib.Downloads != nil && lib.Downloads.Artifact != nil {
			absPath := filepath.Join(libDir, filepath.FromSlash(lib.Downloads.Artifact.Path))
			resolved = append(resolved, ResolvedLibrary{
				AbsPath:  absPath,
				IsNative: isNative,
				Name:     lib.Name,
			})
			continue
		}

		// Pattern B: Maven coordinate → path (Fabric/Forge style)
		mavenPath := MavenToPath(lib.Name)
		if mavenPath == "" {
			continue
		}

		// Check in libraries dir first
		absPath := filepath.Join(libDir, mavenPath)
		if _, err := os.Stat(absPath); err == nil {
			resolved = append(resolved, ResolvedLibrary{
				AbsPath:  absPath,
				IsNative: isNative,
				Name:     lib.Name,
			})
			continue
		}

		// For libraries with a URL base (Fabric/Forge repos), the file might
		// have been downloaded to the standard libraries dir with Maven path
		// Try without the filepath separator conversion (use forward slashes)
		altPath := MavenToPath(lib.Name)
		altAbs := filepath.Join(libDir, altPath)
		if altAbs != absPath {
			if _, err := os.Stat(altAbs); err == nil {
				resolved = append(resolved, ResolvedLibrary{
					AbsPath:  altAbs,
					IsNative: isNative,
					Name:     lib.Name,
				})
				continue
			}
		}

		// Library not found on disk, include it anyway so the classpath is complete
		// (it may still work if the game doesn't need it, or the error will be clear)
		resolved = append(resolved, ResolvedLibrary{
			AbsPath:  absPath,
			IsNative: isNative,
			Name:     lib.Name,
		})
	}

	return resolved, nil
}

// resolveLegacyNatives handles the old-style natives with classifiers.
func resolveLegacyNatives(lib Library, libDir string, env Environment) []ResolvedLibrary {
	var results []ResolvedLibrary

	classifierKey, ok := lib.Natives[env.OSName]
	if !ok {
		return nil
	}

	// Replace ${arch} placeholder in classifier key
	classifierKey = strings.ReplaceAll(classifierKey, "${arch}", archBits())

	if lib.Downloads != nil && lib.Downloads.Classifiers != nil {
		if artifact, ok := lib.Downloads.Classifiers[classifierKey]; ok {
			absPath := filepath.Join(libDir, filepath.FromSlash(artifact.Path))
			results = append(results, ResolvedLibrary{
				AbsPath:  absPath,
				IsNative: true,
				Name:     lib.Name + ":" + classifierKey,
			})
		}
	}

	return results
}

// BuildClasspath builds the classpath string from resolved libraries and the client JAR.
func BuildClasspath(libs []ResolvedLibrary, clientJarPath string) string {
	seen := make(map[string]bool)
	var parts []string

	for _, lib := range libs {
		if seen[lib.AbsPath] {
			continue
		}
		seen[lib.AbsPath] = true
		parts = append(parts, lib.AbsPath)
	}

	// Add the client JAR at the end
	if clientJarPath != "" && !seen[clientJarPath] {
		parts = append(parts, clientJarPath)
	}

	return strings.Join(parts, string(os.PathListSeparator))
}

func archBits() string {
	arch := currentOSArch()
	if arch == "x86_64" || arch == "arm64" {
		return "64"
	}
	return "32"
}
