package minecraft

import (
	"crypto/rand"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// LauncherProfiles represents the launcher_profiles.json structure
// that mod loaders (Forge, Fabric, NeoForge) expect to find.
type LauncherProfiles struct {
	Profiles    map[string]Profile `json:"profiles"`
	ClientToken string             `json:"clientToken"`
	Settings    ProfileSettings    `json:"settings"`
	Version     int                `json:"version"`
}

// Profile represents a single launcher profile entry.
type Profile struct {
	Created       string `json:"created"`
	Icon          string `json:"icon"`
	LastUsed      string `json:"lastUsed"`
	LastVersionID string `json:"lastVersionId"`
	Name          string `json:"name"`
	Type          string `json:"type"`
}

// ProfileSettings holds minimal launcher settings.
type ProfileSettings struct{}

// EnsureLauncherProfiles creates launcher_profiles.json if it doesn't exist,
// or updates it to include the given version ID as a profile.
func EnsureLauncherProfiles(mcDir string, versionID string) error {
	profilesPath := filepath.Join(mcDir, "launcher_profiles.json")

	var profiles LauncherProfiles

	data, err := os.ReadFile(profilesPath)
	if err == nil {
		json.Unmarshal(data, &profiles)
	}

	if profiles.Profiles == nil {
		profiles.Profiles = make(map[string]Profile)
	}
	if profiles.ClientToken == "" {
		profiles.ClientToken = generateClientToken()
	}
	if profiles.Version == 0 {
		profiles.Version = 3
	}

	// Add a default profile if none exist
	if len(profiles.Profiles) == 0 {
		now := time.Now().UTC().Format(time.RFC3339)
		profiles.Profiles["(Default)"] = Profile{
			Created:       now,
			Icon:          "Grass",
			LastUsed:      now,
			LastVersionID: "latest-release",
			Name:          "(Default)",
			Type:          "latest-release",
		}
	}

	// If a specific version was installed, add/update its profile
	if versionID != "" {
		now := time.Now().UTC().Format(time.RFC3339)
		key := versionID
		if _, exists := profiles.Profiles[key]; !exists {
			profiles.Profiles[key] = Profile{
				Created:       now,
				Icon:          "Furnace",
				LastUsed:      now,
				LastVersionID: versionID,
				Name:          versionID,
				Type:          "custom",
			}
		}
	}

	out, err := json.MarshalIndent(profiles, "", "  ")
	if err != nil {
		return fmt.Errorf("marshaling profiles: %w", err)
	}
	if err := os.WriteFile(profilesPath, out, 0644); err != nil {
		return err
	}

	// Also write the Microsoft Store variant. Forge and Fabric check both filenames.
	msStorePath := filepath.Join(mcDir, "launcher_profiles_microsoft_store.json")
	if _, err := os.Stat(msStorePath); os.IsNotExist(err) {
		os.WriteFile(msStorePath, out, 0644)
	}
	return nil
}

func generateClientToken() string {
	b := make([]byte, 16)
	rand.Read(b)
	// Format as UUID
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}
