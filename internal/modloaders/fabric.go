package modloaders

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"time"

	"github.com/mateoltd/croopor/internal/minecraft"
)

const fabricMetaBase = "https://meta.fabricmc.net/v2/versions"

type fabricLoader struct {
	cache  *MetaCache
	client *http.Client
}

// NewFabricLoader creates a Fabric loader backed by the given cache.
func NewFabricLoader(cache *MetaCache) Loader {
	return &fabricLoader{
		cache:  cache,
		client: &http.Client{Timeout: 30 * time.Second},
	}
}

func (f *fabricLoader) Type() LoaderType { return Fabric }

func (f *fabricLoader) Info() LoaderInfo {
	return LoaderInfo{
		Type:        Fabric,
		Name:        "Fabric",
		Description: "Lightweight modding toolchain",
	}
}

func (f *fabricLoader) NeedsBaseGameFirst() bool { return false }

func (f *fabricLoader) VersionID(mcVersion, loaderVersion string) string {
	return "fabric-loader-" + loaderVersion + "-" + mcVersion
}

func (f *fabricLoader) GameVersions() ([]GameVersion, error) {
	const cacheKey = "fabric:game_versions"

	if data, ok, fresh := f.cache.Get(cacheKey); ok && fresh {
		return data.([]GameVersion), nil
	}

	resp, err := f.client.Get(fabricMetaBase + "/game")
	if err != nil {
		if data, ok, _ := f.cache.Get(cacheKey); ok {
			return data.([]GameVersion), nil // stale fallback
		}
		return nil, fmt.Errorf("fabric meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if data, ok, _ := f.cache.Get(cacheKey); ok {
			return data.([]GameVersion), nil
		}
		return nil, fmt.Errorf("fabric meta API: status %d", resp.StatusCode)
	}

	var raw []struct {
		Version string `json:"version"`
		Stable  bool   `json:"stable"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		return nil, fmt.Errorf("fabric meta API: %w", err)
	}

	versions := make([]GameVersion, len(raw))
	for i, v := range raw {
		versions[i] = GameVersion{Version: v.Version, Stable: v.Stable}
	}

	f.cache.Set(cacheKey, versions)
	return versions, nil
}

func (f *fabricLoader) LoaderVersions(mcVersion string) ([]LoaderVersion, error) {
	cacheKey := "fabric:loader_versions:" + mcVersion

	if data, ok, fresh := f.cache.Get(cacheKey); ok && fresh {
		return data.([]LoaderVersion), nil
	}

	resp, err := f.client.Get(fabricMetaBase + "/loader/" + mcVersion)
	if err != nil {
		if data, ok, _ := f.cache.Get(cacheKey); ok {
			return data.([]LoaderVersion), nil
		}
		return nil, fmt.Errorf("fabric meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if data, ok, _ := f.cache.Get(cacheKey); ok {
			return data.([]LoaderVersion), nil
		}
		return nil, fmt.Errorf("fabric meta API: status %d", resp.StatusCode)
	}

	var raw []struct {
		Loader struct {
			Version string `json:"version"`
			Stable  bool   `json:"stable"`
		} `json:"loader"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		return nil, fmt.Errorf("fabric meta API: %w", err)
	}

	versions := make([]LoaderVersion, len(raw))
	for i, v := range raw {
		versions[i] = LoaderVersion{Version: v.Loader.Version, Stable: v.Loader.Stable}
	}

	f.cache.Set(cacheKey, versions)
	return versions, nil
}

func (f *fabricLoader) Install(mcDir, mcVersion, loaderVersion string, progress chan<- Progress) (*InstallResult, error) {
	versionID := f.VersionID(mcVersion, loaderVersion)

	// Check if already installed
	versionDir := filepath.Join(minecraft.VersionsDir(mcDir), versionID)
	jsonPath := filepath.Join(versionDir, versionID+".json")
	markerPath := filepath.Join(versionDir, ".incomplete")
	if _, err := os.Stat(jsonPath); err == nil {
		if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
			// Already installed and complete
			return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Fabric}, nil
		}
	}

	// Fetch version profile JSON from meta API
	progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Fetching Fabric profile..."}

	profileURL := fmt.Sprintf("%s/loader/%s/%s/profile/json", fabricMetaBase, mcVersion, loaderVersion)
	resp, err := f.client.Get(profileURL)
	if err != nil {
		return nil, fmt.Errorf("fetching Fabric profile: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("Fabric profile API returned status %d", resp.StatusCode)
	}

	profileData, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("reading Fabric profile: %w", err)
	}

	// Write version JSON
	progress <- Progress{Phase: "loader_json", Current: 0, Total: 1, Detail: versionID + ".json"}

	if err := os.MkdirAll(versionDir, 0755); err != nil {
		return nil, fmt.Errorf("creating version directory: %w", err)
	}
	os.WriteFile(markerPath, []byte("installing"), 0644)

	if err := os.WriteFile(jsonPath, profileData, 0644); err != nil {
		return nil, fmt.Errorf("writing version JSON: %w", err)
	}

	// Download Fabric libraries
	var profile minecraft.VersionJSON
	if err := json.Unmarshal(profileData, &profile); err != nil {
		return nil, fmt.Errorf("parsing Fabric profile: %w", err)
	}

	if err := DownloadLibraries(profile.Libraries, mcDir, progress); err != nil {
		return nil, fmt.Errorf("downloading Fabric libraries: %w", err)
	}

	// Remove incomplete marker — loader-specific install is done.
	// The base game install will add its own marker for the vanilla version.
	os.Remove(markerPath)

	return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Fabric}, nil
}
