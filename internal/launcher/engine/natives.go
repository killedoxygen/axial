package engine

import (
	"archive/zip"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/mateoltd/croopor/internal/minecraft"
)

// CreateNativesDir creates a directory for native library extraction under the
// user's local app cache rather than the system temp folder. This avoids
// heuristic DLL-drop detections that AV engines apply to %TEMP%.
func CreateNativesDir(sessionID string) (string, error) {
	cacheDir, err := os.UserCacheDir()
	if err != nil {
		// Fallback: use LOCALAPPDATA on Windows, home dir elsewhere
		cacheDir = os.Getenv("LOCALAPPDATA")
		if cacheDir == "" {
			cacheDir, _ = os.UserHomeDir()
		}
	}
	dir := filepath.Join(cacheDir, "croopor", "natives", sessionID)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return "", fmt.Errorf("creating natives dir: %w", err)
	}
	return dir, nil
}

// CleanupNativesDir removes the temporary natives directory.
func CleanupNativesDir(dir string) error {
	if dir == "" {
		return nil
	}
	// Safety: only remove dirs we created
	if !strings.Contains(dir, filepath.Join("croopor", "natives")) {
		return fmt.Errorf("refusing to remove non-croopor directory: %s", dir)
	}
	return os.RemoveAll(dir)
}

// ExtractLegacyNatives extracts native JARs into the natives directory.
// This is needed for legacy versions (<=1.12.2) where native libs must be pre-extracted.
// Modern versions (1.13+) handle extraction at runtime via LWJGL.
func ExtractLegacyNatives(libs []minecraft.ResolvedLibrary, nativesDir string) error {
	for _, lib := range libs {
		if !lib.IsNative {
			continue
		}

		if _, err := os.Stat(lib.AbsPath); os.IsNotExist(err) {
			continue // skip missing native JARs
		}

		if err := extractJar(lib.AbsPath, nativesDir); err != nil {
			return fmt.Errorf("extracting native %s: %w", lib.Name, err)
		}
	}
	return nil
}

func extractJar(jarPath, destDir string) error {
	r, err := zip.OpenReader(jarPath)
	if err != nil {
		return err
	}
	defer r.Close()

	for _, f := range r.File {
		// Skip META-INF
		if strings.HasPrefix(f.Name, "META-INF/") || strings.HasPrefix(f.Name, "META-INF\\") {
			continue
		}

		// Skip directories
		if f.FileInfo().IsDir() {
			continue
		}

		destPath := filepath.Join(destDir, filepath.Base(f.Name))

		// Security: ensure we don't write outside destDir
		if !strings.HasPrefix(filepath.Clean(destPath), filepath.Clean(destDir)) {
			continue
		}

		if err := extractFile(f, destPath); err != nil {
			return err
		}
	}
	return nil
}

func extractFile(f *zip.File, destPath string) error {
	rc, err := f.Open()
	if err != nil {
		return err
	}
	defer rc.Close()

	out, err := os.Create(destPath)
	if err != nil {
		return err
	}
	defer out.Close()

	_, err = io.Copy(out, rc)
	return err
}
