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
		return nil, err
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
			continue
		}
		if _, err := os.Stat(destPath); err == nil {
			continue // Already exists
		}

		rc, err := f.Open()
		if err != nil {
			return fmt.Errorf("opening installer entry %s: %w", f.Name, err)
		}
		data, err := io.ReadAll(rc)
		rc.Close()
		if err != nil {
			return fmt.Errorf("reading installer entry %s: %w", f.Name, err)
		}

		if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
			return fmt.Errorf("creating installer library dir %s: %w", filepath.Dir(destPath), err)
		}
		if err := os.WriteFile(destPath, data, 0644); err != nil {
			return fmt.Errorf("writing installer library %s: %w", destPath, err)
		}
	}

	return nil
}
