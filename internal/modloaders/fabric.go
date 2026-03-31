package modloaders

import (
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
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

	if versions, ok, fresh := cacheGetAs[[]GameVersion](f.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	resp, err := f.client.Get(fabricMetaBase + "/game")
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]GameVersion](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("fabric meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if versions, ok, _ := cacheGetAs[[]GameVersion](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("fabric meta API: status %d", resp.StatusCode)
	}

	var raw []struct {
		Version string `json:"version"`
		Stable  bool   `json:"stable"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		if versions, ok, _ := cacheGetAs[[]GameVersion](f.cache, cacheKey); ok {
			log.Printf("fabric meta decode failed, using stale cache: %v", err)
			return versions, nil
		}
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
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}

	cacheKey := "fabric:loader_versions:" + safeMCVersion

	if versions, ok, fresh := cacheGetAs[[]LoaderVersion](f.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	resp, err := f.client.Get(fabricMetaBase + "/loader/" + url.PathEscape(safeMCVersion))
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("fabric meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](f.cache, cacheKey); ok {
			return versions, nil
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
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](f.cache, cacheKey); ok {
			log.Printf("fabric meta decode failed, using stale cache: %v", err)
			return versions, nil
		}
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
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}
	safeLoaderVersion, err := sanitizeVersionSegment("loader version", loaderVersion)
	if err != nil {
		return nil, err
	}

	versionID := f.VersionID(safeMCVersion, safeLoaderVersion)

	return withInstallLock("fabric:"+versionID, func() (*InstallResult, error) {
		versionDir, jsonPath, markerPath, err := resolveVersionFiles(mcDir, versionID)
		if err != nil {
			return nil, err
		}
		if _, err := os.Stat(jsonPath); err == nil {
			if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
				return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Fabric}, nil
			} else if mErr != nil {
				return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
			}
		} else if err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("checking installed Fabric version: %w", err)
		}

		progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Fetching Fabric profile..."}

		profileURL := fmt.Sprintf("%s/loader/%s/%s/profile/json", fabricMetaBase, url.PathEscape(safeMCVersion), url.PathEscape(safeLoaderVersion))
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

		progress <- Progress{Phase: "loader_json", Current: 0, Total: 1, Detail: versionID + ".json"}

		if err := os.MkdirAll(versionDir, 0755); err != nil {
			return nil, fmt.Errorf("creating version directory: %w", err)
		}
		if err := os.WriteFile(markerPath, []byte("installing"), 0644); err != nil {
			return nil, fmt.Errorf("creating incomplete marker: %w", err)
		}

		if err := os.WriteFile(jsonPath, profileData, 0644); err != nil {
			return nil, fmt.Errorf("writing version JSON: %w", err)
		}

		var profile minecraft.VersionJSON
		if err := json.Unmarshal(profileData, &profile); err != nil {
			return nil, fmt.Errorf("parsing Fabric profile: %w", err)
		}

		if err := DownloadLibraries(profile.Libraries, mcDir, progress); err != nil {
			return nil, fmt.Errorf("downloading Fabric libraries: %w", err)
		}

		if err := os.Remove(markerPath); err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("removing incomplete marker: %w", err)
		}
		return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Fabric}, nil
	})
}
