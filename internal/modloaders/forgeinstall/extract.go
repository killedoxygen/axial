package forgeinstall

import (
	"archive/zip"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/mateoltd/croopor/internal/minecraft"
)

type legacyInstallProfile struct {
	Install struct {
		Path     string `json:"path"`
		FilePath string `json:"filePath"`
		Target   string `json:"target"`
	} `json:"install"`
	VersionInfo json.RawMessage     `json:"versionInfo"`
	Libraries   []minecraft.Library `json:"libraries"`
}

// ExtractInstallerJSONs reads version.json and install_profile.json from an installer JAR.
// Shared by Forge and NeoForge since both use the same installer format.
func ExtractInstallerJSONs(jarData []byte) (versionJSON []byte, installProfile []byte, err error) {
	r, err := zip.NewReader(bytes.NewReader(jarData), int64(len(jarData)))
	if err != nil {
		return nil, nil, fmt.Errorf("opening installer JAR: %w", err)
	}

	for _, f := range r.File {
		switch f.Name {
		case "version.json":
			rc, err := f.Open()
			if err != nil {
				return nil, nil, err
			}
			defer rc.Close()
			versionJSON, err = io.ReadAll(rc)
			if err != nil {
				return nil, nil, err
			}
		case "install_profile.json":
			rc, err := f.Open()
			if err != nil {
				return nil, nil, err
			}
			defer rc.Close()
			installProfile, err = io.ReadAll(rc)
			if err != nil {
				return nil, nil, err
			}
		}
	}

	if versionJSON == nil && installProfile != nil {
		legacyVersionJSON, legacyErr := extractLegacyVersionInfo(installProfile)
		if legacyErr != nil {
			return nil, nil, legacyErr
		}
		versionJSON = legacyVersionJSON
	}

	if versionJSON == nil {
		return nil, nil, fmt.Errorf("version.json not found in installer JAR")
	}

	return versionJSON, installProfile, nil
}

// CollectLibraries gathers libraries from both version.json and install_profile.json.
func CollectLibraries(versionJSON, installProfile []byte) ([]minecraft.Library, error) {
	var version struct {
		Libraries []minecraft.Library `json:"libraries"`
	}
	if err := json.Unmarshal(versionJSON, &version); err != nil {
		return nil, fmt.Errorf("parsing version.json libraries: %w", err)
	}

	libs := version.Libraries

	if installProfile != nil {
		var profile struct {
			Libraries []minecraft.Library `json:"libraries"`
		}
		if err := json.Unmarshal(installProfile, &profile); err != nil {
			return nil, fmt.Errorf("parsing install_profile.json libraries: %w", err)
		}
		libs = append(libs, profile.Libraries...)
	}

	return libs, nil
}

func extractLegacyVersionInfo(installProfile []byte) ([]byte, error) {
	var profile legacyInstallProfile
	if err := json.Unmarshal(installProfile, &profile); err != nil {
		return nil, fmt.Errorf("parsing legacy install_profile.json: %w", err)
	}
	if len(profile.VersionInfo) == 0 {
		return nil, nil
	}

	var raw map[string]json.RawMessage
	if err := json.Unmarshal(profile.VersionInfo, &raw); err != nil {
		return nil, fmt.Errorf("parsing legacy versionInfo: %w", err)
	}

	var versionID string
	if rawID, ok := raw["id"]; ok {
		json.Unmarshal(rawID, &versionID)
	}
	if normalizedID := normalizeLegacyForgeVersionID(profile.Install.Path); normalizedID != "" {
		versionID = normalizedID
	}
	if versionID == "" && profile.Install.Target != "" {
		versionID = profile.Install.Target
	}
	if versionID != "" {
		target, err := json.Marshal(versionID)
		if err != nil {
			return nil, fmt.Errorf("serializing legacy target id: %w", err)
		}
		raw["id"] = target
	}

	normalizedForgeLibrary := normalizeLegacyForgeLibrary(profile.Install.Path, profile.Install.FilePath)
	if normalizedForgeLibrary != "" && raw["libraries"] != nil {
		var libraries []minecraft.Library
		if err := json.Unmarshal(raw["libraries"], &libraries); err != nil {
			return nil, fmt.Errorf("parsing legacy versionInfo libraries: %w", err)
		}
		for i := range libraries {
			if libraries[i].Name == profile.Install.Path {
				libraries[i].Name = normalizedForgeLibrary
				break
			}
		}
		encodedLibraries, err := json.Marshal(libraries)
		if err != nil {
			return nil, fmt.Errorf("serializing legacy versionInfo libraries: %w", err)
		}
		raw["libraries"] = encodedLibraries
	}

	out, err := json.Marshal(raw)
	if err != nil {
		return nil, fmt.Errorf("serializing legacy versionInfo: %w", err)
	}
	return out, nil
}

func normalizeLegacyForgeLibrary(path, filePath string) string {
	if path == "" || filePath == "" {
		return ""
	}
	parts := strings.Split(path, ":")
	if len(parts) != 3 {
		return ""
	}
	filename := strings.TrimSuffix(filepath.Base(filePath), filepath.Ext(filePath))
	prefix := parts[1] + "-" + parts[2] + "-"
	if !strings.HasPrefix(filename, prefix) {
		return ""
	}
	classifier := strings.TrimPrefix(filename, prefix)
	if classifier == "" {
		return ""
	}
	return path + ":" + classifier
}

func normalizeLegacyForgeVersionID(path string) string {
	parts := strings.Split(path, ":")
	if len(parts) != 3 {
		return ""
	}
	version := parts[2]
	idx := strings.Index(version, "-")
	if idx <= 0 || idx+1 >= len(version) {
		return ""
	}
	return version[:idx] + "-forge-" + version[idx+1:]
}

// ExtractDataFiles extracts maven/ entries from the installer JAR
// into the libraries directory (Forge stores some artifacts this way).
func ExtractDataFiles(jarData []byte, mcDir string) error {
	r, err := zip.NewReader(bytes.NewReader(jarData), int64(len(jarData)))
	if err != nil {
		return err
	}

	libDir := minecraft.LibrariesDir(mcDir)
	for _, f := range r.File {
		if !strings.HasPrefix(f.Name, "maven/") {
			continue
		}

		relPath := strings.TrimPrefix(f.Name, "maven/")
		if relPath == "" || strings.HasSuffix(relPath, "/") {
			continue
		}

		destPath := filepath.Join(libDir, filepath.FromSlash(relPath))
		cleanLibDir := filepath.Clean(libDir)
		cleanDestPath := filepath.Clean(destPath)
		relDest, err := filepath.Rel(cleanLibDir, cleanDestPath)
		if err != nil || relDest == ".." || strings.HasPrefix(relDest, ".."+string(os.PathSeparator)) {
			return fmt.Errorf("installer entry %q escapes library dir (resolved to %s)", f.Name, cleanDestPath)
		}
		if info, err := os.Stat(destPath); err == nil {
			if uint64(info.Size()) == f.UncompressedSize64 {
				continue // Already exists with matching size
			}
		}

		if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
			return fmt.Errorf("creating installer library dir %s: %w", filepath.Dir(destPath), err)
		}

		rc, err := f.Open()
		if err != nil {
			return fmt.Errorf("opening installer entry %s: %w", f.Name, err)
		}

		out, err := os.OpenFile(destPath, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0644)
		if err != nil {
			rc.Close()
			return fmt.Errorf("creating installer library %s: %w", destPath, err)
		}

		_, err = io.Copy(out, rc)
		rc.Close()
		if closeErr := out.Close(); err == nil {
			err = closeErr
		}
		if err != nil {
			return fmt.Errorf("writing installer library %s: %w", destPath, err)
		}
	}

	return nil
}
