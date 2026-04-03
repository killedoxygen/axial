package performance

import (
	"context"
	"crypto/sha512"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/modrinth"
)

// installMod downloads one ManagedMod's primary file into instanceModsDir.
func installMod(
	ctx context.Context,
	client modrinth.Client,
	mod composition.ManagedMod,
	gameVersion string,
	loader string,
	instanceModsDir string,
) (*InstalledMod, error) {
	versions, err := client.ListVersions(ctx, mod.ProjectID, []string{gameVersion}, []string{loader})
	if err != nil {
		return nil, err
	}
	if len(versions) == 0 {
		if parent := parentMinorVersion(gameVersion); parent != "" && parent != gameVersion {
			versions, err = client.ListVersions(ctx, mod.ProjectID, []string{parent}, []string{loader})
			if err != nil {
				return nil, err
			}
		}
	}
	if len(versions) == 0 {
		return nil, fmt.Errorf("no compatible versions found for %s", mod.ProjectID)
	}

	version := versions[0]
	file := version.PrimaryFile()
	if file == nil {
		return nil, fmt.Errorf("no downloadable file for %s", mod.ProjectID)
	}
	filename, err := sanitizeModFilename(file.Filename)
	if err != nil {
		return nil, err
	}

	expectedSHA := file.Hashes["sha512"]
	finalPath := filepath.Join(instanceModsDir, filename)
	if expectedSHA != "" {
		if ok, err := fileMatchesSHA512(finalPath, expectedSHA); err == nil && ok {
			return &InstalledMod{
				ProjectID: mod.ProjectID,
				VersionID: version.ID,
				Filename:  filename,
				SHA512:    expectedSHA,
			}, nil
		}
	}

	if err := os.MkdirAll(instanceModsDir, 0755); err != nil {
		return nil, err
	}

	tmpPath := finalPath + ".tmp"
	tmpFile, err := os.Create(tmpPath)
	if err != nil {
		return nil, err
	}

	downloadErr := client.DownloadFile(ctx, file.URL, expectedSHA, tmpFile)
	closeErr := tmpFile.Close()
	if downloadErr != nil {
		os.Remove(tmpPath)
		return nil, downloadErr
	}
	if closeErr != nil {
		os.Remove(tmpPath)
		return nil, closeErr
	}
	if err := replaceFileAtomic(tmpPath, finalPath); err != nil {
		return nil, err
	}

	return &InstalledMod{
		ProjectID: mod.ProjectID,
		VersionID: version.ID,
		Filename:  filename,
		SHA512:    expectedSHA,
	}, nil
}

func parentMinorVersion(gameVersion string) string {
	parts := strings.Split(gameVersion, ".")
	if len(parts) < 2 {
		return ""
	}
	return parts[0] + "." + parts[1]
}

func fileMatchesSHA512(path string, expected string) (bool, error) {
	f, err := os.Open(path)
	if err != nil {
		return false, err
	}
	defer f.Close()

	h := sha512.New()
	if _, err := io.Copy(h, f); err != nil {
		return false, err
	}
	return strings.EqualFold(hex.EncodeToString(h.Sum(nil)), expected), nil
}

func sanitizeModFilename(name string) (string, error) {
	name = strings.TrimSpace(name)
	if name == "" {
		return "", fmt.Errorf("mod filename is required")
	}
	if filepath.IsAbs(name) {
		return "", fmt.Errorf("mod filename must be relative: %s", name)
	}
	base := filepath.Base(name)
	if base != name || strings.Contains(name, "/") || strings.Contains(name, "\\") {
		return "", fmt.Errorf("mod filename must not contain path separators: %s", name)
	}
	return base, nil
}
