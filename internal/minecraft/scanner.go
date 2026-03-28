package minecraft

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
)

// VersionEntry is a lightweight summary of a detected version (no full parse needed).
type VersionEntry struct {
	ID           string `json:"id"`
	Type         string `json:"type"`
	ReleaseTime  string `json:"release_time,omitempty"`
	InheritsFrom string `json:"inherits_from,omitempty"`
	Launchable   bool   `json:"launchable"`
	Missing      []string `json:"missing,omitempty"`
	JavaComponent string `json:"java_component,omitempty"`
	JavaMajor     int    `json:"java_major,omitempty"`
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

// ScanVersions scans the versions directory and returns a list of detected versions.
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
			continue // skip dirs without a version JSON
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
		}

		if stub.JavaVersion != nil {
			ve.JavaComponent = stub.JavaVersion.Component
			ve.JavaMajor = stub.JavaVersion.MajorVersion
		}

		// Determine launch readiness
		var missing []string
		if stub.InheritsFrom == "" {
			// Vanilla version: needs its own client JAR
			if _, err := os.Stat(jarPath); os.IsNotExist(err) {
				missing = append(missing, "client_jar")
			}
		} else {
			// Modded version: needs the parent's client JAR
			parentJar := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".jar")
			if _, err := os.Stat(parentJar); os.IsNotExist(err) {
				missing = append(missing, "parent_client_jar")
			}
			// Also check parent JSON exists
			parentJSON := filepath.Join(versionsDir, stub.InheritsFrom, stub.InheritsFrom+".json")
			if _, err := os.Stat(parentJSON); os.IsNotExist(err) {
				missing = append(missing, "parent_version_json")
			}
		}

		ve.Missing = missing
		ve.Launchable = len(missing) == 0
		versions = append(versions, ve)
	}

	// Sort: releases first, then by ID descending (newest first for semver-like IDs)
	sort.Slice(versions, func(i, j int) bool {
		ti := versionTypePriority(versions[i].Type)
		tj := versionTypePriority(versions[j].Type)
		if ti != tj {
			return ti < tj
		}
		return versions[i].ID > versions[j].ID
	})

	return versions, nil
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
