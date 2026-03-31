package modloaders

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
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

	if versions, ok, fresh := cacheGetAs[[]GameVersion](q.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	resp, err := q.client.Get(quiltMetaBase + "/game")
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]GameVersion](q.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("quilt meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if versions, ok, _ := cacheGetAs[[]GameVersion](q.cache, cacheKey); ok {
			return versions, nil
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
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}

	cacheKey := "quilt:loader_versions:" + safeMCVersion

	if versions, ok, fresh := cacheGetAs[[]LoaderVersion](q.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	resp, err := q.client.Get(quiltMetaBase + "/loader/" + url.PathEscape(safeMCVersion))
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](q.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("quilt meta API: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](q.cache, cacheKey); ok {
			return versions, nil
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
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}
	safeLoaderVersion, err := sanitizeVersionSegment("loader version", loaderVersion)
	if err != nil {
		return nil, err
	}

	versionID := q.VersionID(safeMCVersion, safeLoaderVersion)

	return withInstallLock("quilt:"+versionID, func() (*InstallResult, error) {
		versionDir, jsonPath, markerPath, err := resolveVersionFiles(mcDir, versionID)
		if err != nil {
			return nil, err
		}
		if _, err := os.Stat(jsonPath); err == nil {
			if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
				return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Quilt}, nil
			} else if mErr != nil {
				return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
			}
		} else if err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("checking installed Quilt version: %w", err)
		}

		progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Fetching Quilt profile..."}

		profileURL := fmt.Sprintf("%s/loader/%s/%s/profile/json", quiltMetaBase, url.PathEscape(safeMCVersion), url.PathEscape(safeLoaderVersion))
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
			return nil, fmt.Errorf("parsing Quilt profile: %w", err)
		}

		if err := DownloadLibraries(profile.Libraries, mcDir, progress); err != nil {
			return nil, fmt.Errorf("downloading Quilt libraries: %w", err)
		}

		if err := os.Remove(markerPath); err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("removing incomplete marker: %w", err)
		}
		return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Quilt}, nil
	})
}
