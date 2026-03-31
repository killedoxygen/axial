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

const quiltMetaBase = "https://meta.quiltmc.org/v3/versions"

type quiltLoader struct {
	cache  *MetaCache
	client *http.Client
}

// NewQuiltLoader creates a Quilt loader backed by the given cache.
func NewQuiltLoader(cache *MetaCache) Loader {
	return &quiltLoader{
		cache:  cache,
		client: &http.Client{Timeout: 30 * time.Second},
	}
}

func (q *quiltLoader) Type() LoaderType { return Quilt }

func (q *quiltLoader) Info() LoaderInfo {
	return LoaderInfo{
		Type:        Quilt,
		Name:        "Quilt",
		Description: "Fabric-compatible mod loader",
	}
}

func (q *quiltLoader) NeedsBaseGameFirst() bool { return false }

func (q *quiltLoader) VersionID(mcVersion, loaderVersion string) string {
	return "quilt-loader-" + loaderVersion + "-" + mcVersion
}

func (q *quiltLoader) GameVersions() ([]GameVersion, error) {
	const cacheKey = "quilt:game_versions"

	if data, ok, fresh := q.cache.Get(cacheKey); ok && fresh {
		return data.([]GameVersion), nil
	}

	resp, err := q.client.Get(quiltMetaBase + "/game")
	if err != nil {
		if data, ok, _ := q.cache.Get(cacheKey); ok {
			return data.([]GameVersion), nil
		}
		return nil, fmt.Errorf("quilt meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if data, ok, _ := q.cache.Get(cacheKey); ok {
			return data.([]GameVersion), nil
		}
		return nil, fmt.Errorf("quilt meta API: status %d", resp.StatusCode)
	}

	var raw []struct {
		Version string `json:"version"`
		Stable  bool   `json:"stable"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		return nil, fmt.Errorf("quilt meta API: %w", err)
	}

	versions := make([]GameVersion, len(raw))
	for i, v := range raw {
		versions[i] = GameVersion{Version: v.Version, Stable: v.Stable}
	}

	q.cache.Set(cacheKey, versions)
	return versions, nil
}

func (q *quiltLoader) LoaderVersions(mcVersion string) ([]LoaderVersion, error) {
	cacheKey := "quilt:loader_versions:" + mcVersion

	if data, ok, fresh := q.cache.Get(cacheKey); ok && fresh {
		return data.([]LoaderVersion), nil
	}

	resp, err := q.client.Get(quiltMetaBase + "/loader/" + mcVersion)
	if err != nil {
		if data, ok, _ := q.cache.Get(cacheKey); ok {
			return data.([]LoaderVersion), nil
		}
		return nil, fmt.Errorf("quilt meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if data, ok, _ := q.cache.Get(cacheKey); ok {
			return data.([]LoaderVersion), nil
		}
		return nil, fmt.Errorf("quilt meta API: status %d", resp.StatusCode)
	}

	var raw []struct {
		Loader struct {
			Version string `json:"version"`
		} `json:"loader"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		return nil, fmt.Errorf("quilt meta API: %w", err)
	}

	versions := make([]LoaderVersion, len(raw))
	for i, v := range raw {
		versions[i] = LoaderVersion{Version: v.Loader.Version, Stable: true}
	}

	q.cache.Set(cacheKey, versions)
	return versions, nil
}

func (q *quiltLoader) Install(mcDir, mcVersion, loaderVersion string, progress chan<- Progress) (*InstallResult, error) {
	versionID := q.VersionID(mcVersion, loaderVersion)

	// Check if already installed
	versionDir := filepath.Join(minecraft.VersionsDir(mcDir), versionID)
	jsonPath := filepath.Join(versionDir, versionID+".json")
	markerPath := filepath.Join(versionDir, ".incomplete")
	if _, err := os.Stat(jsonPath); err == nil {
		if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
			return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Quilt}, nil
		}
	}

	// Fetch version profile JSON
	progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Fetching Quilt profile..."}

	profileURL := fmt.Sprintf("%s/loader/%s/%s/profile/json", quiltMetaBase, mcVersion, loaderVersion)
	resp, err := q.client.Get(profileURL)
	if err != nil {
		return nil, fmt.Errorf("fetching Quilt profile: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("Quilt profile API returned status %d", resp.StatusCode)
	}

	profileData, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("reading Quilt profile: %w", err)
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

	// Download Quilt libraries
	var profile minecraft.VersionJSON
	if err := json.Unmarshal(profileData, &profile); err != nil {
		return nil, fmt.Errorf("parsing Quilt profile: %w", err)
	}

	if err := DownloadLibraries(profile.Libraries, mcDir, progress); err != nil {
		return nil, fmt.Errorf("downloading Quilt libraries: %w", err)
	}

	os.Remove(markerPath)
	return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Quilt}, nil
}
