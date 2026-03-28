package minecraft

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strconv"
)

// VersionEntry is a lightweight summary of a version.
type VersionEntry struct {
	ID             string `json:"id"`
	Type           string `json:"type"`
	ReleaseTime    string `json:"release_time,omitempty"`
	InheritsFrom   string `json:"inherits_from,omitempty"`
	Launchable     bool   `json:"launchable"`
	Installed      bool   `json:"installed"`
	Status         string `json:"status"` // "ready", "incomplete"
	StatusDetail   string `json:"status_detail,omitempty"`
	NeedsInstall   string `json:"needs_install,omitempty"` // version ID to install (self or parent)
	JavaComponent  string `json:"java_component,omitempty"`
	JavaMajor      int    `json:"java_major,omitempty"`
	ManifestURL    string `json:"manifest_url,omitempty"`
}

// versionStub is used for quick JSON parsing without full version resolution.
type versionStub struct {
	ID           string       `json:"id"`
	Type         string       `json:"type"`
	ReleaseTime  string       `json:"releaseTime"`
	InheritsFrom string       `json:"inheritsFrom"`
	JavaVersion  *javaVerStub `json:"javaVersion"`
}

type javaVerStub struct {
	Component    string `json:"component"`
	MajorVersion int    `json:"majorVersion"`
}

// ScanVersions scans the local versions directory.
func ScanVersions(mcDir string) ([]VersionEntry, error) {
	versionsDir := VersionsDir(mcDir)
	entries, err := os.ReadDir(versionsDir)
	if err != nil {
		return nil, err
	}

	var versions []VersionEntry
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		id := entry.Name()
		jsonPath := filepath.Join(versionsDir, id, id+".json")
		jarPath := filepath.Join(versionsDir, id, id+".jar")

		jsonData, err := os.ReadFile(jsonPath)
		if err != nil {
			continue
		}

		var stub versionStub
		if err := json.Unmarshal(jsonData, &stub); err != nil {
			continue
		}

		ve := VersionEntry{
			ID:           id,
			Type:         stub.Type,
			ReleaseTime:  stub.ReleaseTime,
			InheritsFrom: stub.InheritsFrom,
			Installed:    true,
		}

		if stub.JavaVersion != nil {
			ve.JavaComponent = stub.JavaVersion.Component
			ve.JavaMajor = stub.JavaVersion.MajorVersion
		}

		// Determine launch readiness
		ready := true
		detail := ""
		needsInstall := ""

		// Check for incomplete install marker (written at install start,
		// removed on success). Persists across crashes.
		markerPath := filepath.Join(versionsDir, id, ".incomplete")
		if _, err := os.Stat(markerPath); err == nil {
			ready = false
			detail = "Installation incomplete — reinstall required"
			needsInstall = id
		} else if stub.InheritsFrom == "" {
			// Vanilla version: needs its own client JAR
			if _, err := os.Stat(jarPath); os.IsNotExist(err) {
				ready = false
				detail = "Game files not fully downloaded"
				needsInstall = id
			}
		} else {
			// Modded version: needs the parent's client JAR
			parentJar := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".jar")
			parentJSON := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".json")
			if _, err := os.Stat(parentJSON); os.IsNotExist(err) {
				ready = false
				detail = "Base version " + stub.InheritsFrom + " needs to be installed"
				needsInstall = stub.InheritsFrom
			} else if _, err := os.Stat(parentJar); os.IsNotExist(err) {
				ready = false
				detail = "Base version " + stub.InheritsFrom + " needs to be downloaded"
				needsInstall = stub.InheritsFrom
			}
		}

		ve.Launchable = ready
		ve.NeedsInstall = needsInstall
		if ready {
			ve.Status = "ready"
		} else {
			ve.Status = "incomplete"
			ve.StatusDetail = detail
		}

		versions = append(versions, ve)
	}

	sortVersions(versions)
	return versions, nil
}

func sortVersions(versions []VersionEntry) {
	sort.Slice(versions, func(i, j int) bool {
		// Installed first
		if versions[i].Installed != versions[j].Installed {
			return versions[i].Installed
		}
		ti := versionTypePriority(versions[i].Type)
		tj := versionTypePriority(versions[j].Type)
		if ti != tj {
			return ti < tj
		}
		return compareVersionIDs(versions[i].ID, versions[j].ID) > 0
	})
}

// compareVersionIDs compares two Minecraft version strings numerically.
// Returns >0 if a should sort before b (higher/newer), <0 if after, 0 if equal.
// Handles versions like "1.8.9", "1.21.1", "24w14a", "1.0", "b1.8.1", etc.
func compareVersionIDs(a, b string) int {
	partsA := splitVersionParts(a)
	partsB := splitVersionParts(b)

	n := len(partsA)
	if len(partsB) > n {
		n = len(partsB)
	}
	for i := 0; i < n; i++ {
		var pa, pb string
		if i < len(partsA) {
			pa = partsA[i]
		}
		if i < len(partsB) {
			pb = partsB[i]
		}
		na, errA := strconv.Atoi(pa)
		nb, errB := strconv.Atoi(pb)
		if errA == nil && errB == nil {
			if na != nb {
				return na - nb
			}
			continue
		}
		// Fall back to string comparison for non-numeric parts
		if pa != pb {
			if pa > pb {
				return 1
			}
			return -1
		}
	}
	return 0
}

func splitVersionParts(v string) []string {
	// Split on "." and "-" to handle "1.21.1", "1.8.9-forge", "24w14a", etc.
	var parts []string
	current := ""
	for _, c := range v {
		if c == '.' || c == '-' {
			if current != "" {
				parts = append(parts, current)
			}
			current = ""
		} else {
			current += string(c)
		}
	}
	if current != "" {
		parts = append(parts, current)
	}
	return parts
}

func versionTypePriority(t string) int {
	switch t {
	case "release":
		return 0
	case "snapshot":
		return 1
	case "old_beta":
		return 2
	case "old_alpha":
		return 3
	default:
		return 4
	}
}
