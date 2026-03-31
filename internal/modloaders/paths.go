package modloaders

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/mateoltd/croopor/internal/minecraft"
)

var versionSegmentPattern = regexp.MustCompile(`^[A-Za-z0-9._-]+$`)

func sanitizeVersionSegment(name, value string) (string, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return "", fmt.Errorf("%s is required", name)
	}
	if strings.Contains(value, "..") || !versionSegmentPattern.MatchString(value) {
		return "", fmt.Errorf("invalid %s", name)
	}
	return value, nil
}

func resolveVersionFiles(mcDir, versionID string) (versionDir, jsonPath, markerPath string, err error) {
	versionsRoot := filepath.Clean(minecraft.VersionsDir(mcDir))
	versionDir = filepath.Clean(filepath.Join(versionsRoot, versionID))
	rel, err := filepath.Rel(versionsRoot, versionDir)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(os.PathSeparator)) {
		return "", "", "", fmt.Errorf("invalid version path for %s", versionID)
	}
	return versionDir, filepath.Join(versionDir, versionID+".json"), filepath.Join(versionDir, ".incomplete"), nil
}
