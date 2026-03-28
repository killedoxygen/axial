package launcher

import (
	"archive/zip"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
)

// CreateNativesDir creates a temporary directory for native library extraction.
func CreateNativesDir(sessionID string) (string, error) {
	dir := filepath.Join(os.TempDir(), "paralauncher-natives-"+sessionID)
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
	if !strings.Contains(dir, "paralauncher-natives-") {
		return fmt.Errorf("refusing to remove non-paralauncher directory: %s", dir)
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
