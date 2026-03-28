package minecraft

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
)

// VersionEntry is a lightweight summary of a version.
type VersionEntry struct {
	ID            string `json:"id"`
	Type          string `json:"type"`
	ReleaseTime   string `json:"release_time,omitempty"`
	InheritsFrom  string `json:"inherits_from,omitempty"`
	Launchable    bool   `json:"launchable"`
	Installed     bool   `json:"installed"`
	Status        string `json:"status"` // "ready", "not_installed", "incomplete"
	StatusDetail  string `json:"status_detail,omitempty"`
	JavaComponent string `json:"java_component,omitempty"`
	JavaMajor     int    `json:"java_major,omitempty"`
	ManifestURL   string `json:"manifest_url,omitempty"`
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

		if stub.InheritsFrom == "" {
			if _, err := os.Stat(jarPath); os.IsNotExist(err) {
				ready = false
				detail = "Game files not fully downloaded"
			}
		} else {
			parentJar := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".jar")
			parentJSON := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".json")
			if _, err := os.Stat(parentJSON); os.IsNotExist(err) {
				ready = false
				detail = "Base version " + stub.InheritsFrom + " not installed"
			} else if _, err := os.Stat(parentJar); os.IsNotExist(err) {
				ready = false
				detail = "Base version " + stub.InheritsFrom + " not fully downloaded"
			}
		}

		ve.Launchable = ready
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

// MergeWithManifest combines local versions with the remote Mojang manifest.
// Local versions are enriched with ManifestURL; remote-only versions are added as not_installed.
func MergeWithManifest(local []VersionEntry, manifest *VersionManifest) []VersionEntry {
	if manifest == nil {
		return local
	}

	localMap := make(map[string]*VersionEntry, len(local))
	for i := range local {
		localMap[local[i].ID] = &local[i]
	}

	var merged []VersionEntry

	for _, entry := range manifest.Versions {
		if lv, exists := localMap[entry.ID]; exists {
			lv.ManifestURL = entry.URL
			if lv.ReleaseTime == "" {
				lv.ReleaseTime = entry.ReleaseTime
			}
			merged = append(merged, *lv)
			delete(localMap, entry.ID)
		} else {
			// Remote-only version
			merged = append(merged, VersionEntry{
				ID:          entry.ID,
				Type:        entry.Type,
				ReleaseTime: entry.ReleaseTime,
				Installed:   false,
				Launchable:  false,
				Status:      "not_installed",
				ManifestURL: entry.URL,
			})
		}
	}

	// Add any local-only versions (modded/custom) that aren't in the manifest
	for _, lv := range localMap {
		merged = append(merged, *lv)
	}

	sortVersions(merged)
	return merged
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
		return versions[i].ID > versions[j].ID
	})
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
