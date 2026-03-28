package minecraft

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// IntegrityIssue describes a single problem with a version's installation.
type IntegrityIssue struct {
	Type   string `json:"type"`
	Detail string `json:"detail"`
}

// IntegrityResult holds the outcome of a pre-launch integrity check.
type IntegrityResult struct {
	OK     bool             `json:"ok"`
	Issues []IntegrityIssue `json:"issues,omitempty"`
}

func (r *IntegrityResult) fail(typ, detail string) {
	r.OK = false
	r.Issues = append(r.Issues, IntegrityIssue{Type: typ, Detail: detail})
}

// VerifyIntegrity performs a deep check on a version before launch.
// It validates that all critical files exist: version JSON, client JAR,
// libraries, and asset index. This catches corrupted or partially-installed
// versions that the lightweight scanner cannot detect.
func VerifyIntegrity(mcDir, versionID string) *IntegrityResult {
	result := &IntegrityResult{OK: true}
	versionsDir := VersionsDir(mcDir)

	// 1. Check for incomplete install marker
	markerPath := filepath.Join(versionsDir, versionID, ".incomplete")
	if _, err := os.Stat(markerPath); err == nil {
		result.fail("incomplete_install", "Installation was interrupted — reinstall this version")
		return result
	}

	// 2. Load and resolve version (handles inheritsFrom chain)
	version, err := ResolveVersion(mcDir, versionID)
	if err != nil {
		result.fail("bad_version", fmt.Sprintf("Cannot read version data: %s", err))
		return result
	}

	// 3. Check client JAR exists (handles modded inheritance)
	if !clientJarExists(mcDir, version, versionID) {
		result.fail("missing_jar", "Client JAR not found")
	}

	// 4. Check libraries exist
	env := DefaultEnvironment()
	libs, err := ResolveLibraries(version, mcDir, env)
	if err == nil {
		missing := 0
		for _, lib := range libs {
			if lib.IsNative {
				continue // natives are extracted at launch time
			}
			if _, err := os.Stat(lib.AbsPath); os.IsNotExist(err) {
				missing++
			}
		}
		if missing > 0 {
			result.fail("missing_libraries", fmt.Sprintf("%d library files missing", missing))
		}
	}

	// 5. Check asset index exists
	if version.AssetIndex.ID != "" {
		indexPath := filepath.Join(AssetsDir(mcDir), "indexes", version.AssetIndex.ID+".json")
		if _, err := os.Stat(indexPath); os.IsNotExist(err) {
			result.fail("missing_asset_index", "Asset index not found")
		}
	}

	return result
}

// clientJarExists checks whether the client JAR can be found, handling
// modded versions that inherit the JAR from a parent vanilla version.
func clientJarExists(mcDir string, v *VersionJSON, originalVersionID string) bool {
	versionsDir := VersionsDir(mcDir)

	// Check the version's own directory
	jarPath := filepath.Join(versionsDir, v.ID, v.ID+".jar")
	if _, err := os.Stat(jarPath); err == nil {
		return true
	}

	// For modded versions, check the parent via the unmerged JSON
	if originalVersionID != "" {
		origJSON := filepath.Join(versionsDir, originalVersionID, originalVersionID+".json")
		if data, err := os.ReadFile(origJSON); err == nil {
			var stub struct {
				InheritsFrom string `json:"inheritsFrom"`
			}
			if json.Unmarshal(data, &stub) == nil && stub.InheritsFrom != "" {
				parentJar := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".jar")
				if _, err := os.Stat(parentJar); err == nil {
					return true
				}
			}
		}
	}

	// Scan version directory for any .jar
	entries, err := os.ReadDir(filepath.Join(versionsDir, v.ID))
	if err == nil {
		for _, e := range entries {
			if filepath.Ext(e.Name()) == ".jar" {
				return true
			}
		}
	}

	return false
}

// FormatIssues returns a human-readable summary of integrity issues.
func (r *IntegrityResult) FormatIssues() string {
	if r.OK || len(r.Issues) == 0 {
		return ""
	}
	if len(r.Issues) == 1 {
		return r.Issues[0].Detail
	}
	msg := "Multiple issues found:"
	for _, issue := range r.Issues {
		msg += "\n  - " + issue.Detail
	}
	return msg
}
