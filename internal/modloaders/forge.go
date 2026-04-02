package modloaders

import (
	"encoding/json"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"

	"github.com/mateoltd/croopor/internal/modloaders/forgeinstall"
)

const (
	forgeMavenBase           = "https://maven.minecraftforge.net"
	forgeMavenMeta           = forgeMavenBase + "/net/minecraftforge/forge/maven-metadata.xml"
	forgePromotionsURL       = "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json"
	maxInstallerDownloadSize = 50 << 20
)

type forgeLoader struct {
	cache  *MetaCache
	client *http.Client
}

// NewForgeLoader creates a Forge loader backed by the given cache.
func NewForgeLoader(cache *MetaCache) Loader {
	return &forgeLoader{
		cache:  cache,
		client: &http.Client{Timeout: 2 * time.Minute},
	}
}

func (f *forgeLoader) Type() LoaderType { return Forge }

func (f *forgeLoader) Info() LoaderInfo {
	return LoaderInfo{
		Type:        Forge,
		Name:        "Forge",
		Description: "Established modding platform",
	}
}

func (f *forgeLoader) NeedsBaseGameFirst() bool { return true }

func (f *forgeLoader) VersionID(mcVersion, loaderVersion string) string {
	return mcVersion + "-forge-" + loaderVersion
}

// mavenMetadata is the structure of maven-metadata.xml
type mavenMetadata struct {
	Versioning struct {
		Versions struct {
			Version []string `xml:"version"`
		} `xml:"versions"`
	} `xml:"versioning"`
}

// forgePromotions maps "1.20.1-recommended" -> "47.3.0"
type forgePromotions struct {
	Promos map[string]string `json:"promos"`
}

func (f *forgeLoader) GameVersions() ([]GameVersion, error) {
	const cacheKey = "forge:game_versions"

	if versions, ok, fresh := cacheGetAs[[]GameVersion](f.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	entries, err := f.fetchMavenVersions()
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]GameVersion](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, err
	}

	// Extract unique MC versions from "mcVersion-forgeVersion" entries
	seen := map[string]bool{}
	var versions []GameVersion
	for _, entry := range entries {
		mcv := extractMCVersion(entry)
		if mcv == "" || seen[mcv] {
			continue
		}
		seen[mcv] = true
		versions = append(versions, GameVersion{Version: mcv, Stable: true})
	}

	f.cache.Set(cacheKey, versions)
	return versions, nil
}

func (f *forgeLoader) LoaderVersions(mcVersion string) ([]LoaderVersion, error) {
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}

	cacheKey := "forge:loader_versions:" + safeMCVersion

	if versions, ok, fresh := cacheGetAs[[]LoaderVersion](f.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	entries, err := f.fetchMavenVersions()
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, err
	}

	// Fetch promotions for recommended/latest flags
	promos := f.fetchPromotions()

	recommended := promos[safeMCVersion+"-recommended"]
	latest := promos[safeMCVersion+"-latest"]

	var versions []LoaderVersion
	for _, entry := range entries {
		mcv := extractMCVersion(entry)
		if mcv != safeMCVersion {
			continue
		}
		forgeVer := extractForgeVersion(entry)
		if forgeVer == "" {
			continue
		}
		versions = append(versions, LoaderVersion{
			Version:     forgeVer,
			Stable:      forgeVer == recommended,
			Recommended: forgeVer == recommended || forgeVer == latest,
		})
	}

	// Maven metadata is oldest-first; reverse to newest-first.
	for i, j := 0, len(versions)-1; i < j; i, j = i+1, j-1 {
		versions[i], versions[j] = versions[j], versions[i]
	}

	f.cache.Set(cacheKey, versions)
	return versions, nil
}

func (f *forgeLoader) Install(mcDir, mcVersion, loaderVersion string, progress chan<- Progress) (*InstallResult, error) {
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}
	safeLoaderVersion, err := sanitizeVersionSegment("loader version", loaderVersion)
	if err != nil {
		return nil, err
	}

	versionID := f.VersionID(safeMCVersion, safeLoaderVersion)

	return withInstallLock("forge:"+versionID, func() (*InstallResult, error) {
		versionDir, jsonPath, markerPath, err := resolveVersionFiles(mcDir, versionID)
		if err != nil {
			return nil, err
		}
		if _, err := os.Stat(jsonPath); err == nil {
			if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
				return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Forge}, nil
			} else if mErr != nil {
				return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
			}
		} else if err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("checking installed Forge version: %w", err)
		}

		progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Downloading Forge installer..."}

		mavenCoord := safeMCVersion + "-" + safeLoaderVersion
		escapedMavenCoord := url.PathEscape(mavenCoord)
		installerURL := fmt.Sprintf("%s/net/minecraftforge/forge/%s/forge-%s-installer.jar", forgeMavenBase, escapedMavenCoord, escapedMavenCoord)

		installerData, err := f.downloadToMemory(installerURL)
		if err != nil {
			return nil, fmt.Errorf("downloading Forge installer: %w", err)
		}

		progress <- Progress{Phase: "loader_json", Current: 0, Total: 1, Detail: "Extracting installer..."}

		versionJSON, installProfile, err := forgeinstall.ExtractInstallerJSONs(installerData)
		if err != nil {
			return nil, fmt.Errorf("extracting Forge installer: %w", err)
		}

		var parsedVersion struct {
			ID string `json:"id"`
		}
		if err := json.Unmarshal(versionJSON, &parsedVersion); err == nil && parsedVersion.ID != "" {
			versionID, err = sanitizeVersionSegment("installer version id", parsedVersion.ID)
			if err != nil {
				return nil, err
			}
			versionDir, jsonPath, markerPath, err = resolveVersionFiles(mcDir, versionID)
			if err != nil {
				return nil, err
			}
			if _, err := os.Stat(jsonPath); err == nil {
				if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
					return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Forge}, nil
				} else if mErr != nil {
					return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
				}
			} else if err != nil && !os.IsNotExist(err) {
				return nil, fmt.Errorf("checking installed Forge version: %w", err)
			}
		}

		if err := os.MkdirAll(versionDir, 0755); err != nil {
			return nil, fmt.Errorf("creating version directory: %w", err)
		}
		if err := os.WriteFile(markerPath, []byte("installing"), 0644); err != nil {
			return nil, fmt.Errorf("creating incomplete marker: %w", err)
		}

		if err := os.WriteFile(jsonPath, versionJSON, 0644); err != nil {
			return nil, fmt.Errorf("writing version JSON: %w", err)
		}

		allLibs, err := forgeinstall.CollectLibraries(versionJSON, installProfile)
		if err != nil {
			return nil, fmt.Errorf("parsing Forge libraries: %w", err)
		}

		if err := DownloadLibraries(allLibs, mcDir, progress); err != nil {
			return nil, fmt.Errorf("downloading Forge libraries: %w", err)
		}

		if err := forgeinstall.ExtractDataFiles(installerData, mcDir); err != nil {
			return nil, fmt.Errorf("extracting installer data: %w", err)
		}

		if installProfile != nil {
			progress <- Progress{Phase: "loader_processors", Current: 0, Total: 1, Detail: "Running processors..."}

			if err := forgeinstall.RunProcessors(mcDir, safeMCVersion, versionID, installProfile, installerData, func(current, total int, detail string) {
				progress <- Progress{Phase: "loader_processors", Current: current, Total: total, Detail: detail}
			}); err != nil {
				return nil, fmt.Errorf("running Forge processors: %w", err)
			}
		}

		if err := os.Remove(markerPath); err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("removing incomplete marker: %w", err)
		}
		return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: Forge}, nil
	})
}

// fetchMavenVersions fetches and parses the Forge Maven metadata XML.
func (f *forgeLoader) fetchMavenVersions() ([]string, error) {
	const cacheKey = "forge:maven_versions"
	if versions, ok, fresh := cacheGetAs[[]string](f.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	resp, err := f.client.Get(forgeMavenMeta)
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]string](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("forge maven: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if versions, ok, _ := cacheGetAs[[]string](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("forge maven: status %d", resp.StatusCode)
	}

	var meta mavenMetadata
	if err := xml.NewDecoder(resp.Body).Decode(&meta); err != nil {
		if versions, ok, _ := cacheGetAs[[]string](f.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("parsing forge maven metadata: %w", err)
	}

	versions := meta.Versioning.Versions.Version
	f.cache.Set(cacheKey, versions)
	return versions, nil
}

func (f *forgeLoader) fetchPromotions() map[string]string {
	const cacheKey = "forge:promotions"
	if promos, ok, fresh := cacheGetAs[map[string]string](f.cache, cacheKey); ok && fresh {
		return promos
	}

	resp, err := f.client.Get(forgePromotionsURL)
	if err != nil {
		if promos, ok, _ := cacheGetAs[map[string]string](f.cache, cacheKey); ok {
			return promos
		}
		return map[string]string{}
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return map[string]string{}
	}

	var promos forgePromotions
	if err := json.NewDecoder(resp.Body).Decode(&promos); err != nil {
		return map[string]string{}
	}

	f.cache.Set(cacheKey, promos.Promos)
	return promos.Promos
}

func (f *forgeLoader) downloadToMemory(url string) ([]byte, error) {
	resp, err := f.client.Get(url)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("status %d for %s", resp.StatusCode, url)
	}
	if resp.ContentLength > 0 && resp.ContentLength > maxInstallerDownloadSize {
		return nil, fmt.Errorf("download too large for %s", url)
	}

	return io.ReadAll(io.LimitReader(resp.Body, maxInstallerDownloadSize))
}

// extractMCVersion extracts "1.20.1" from "1.20.1-47.3.0"
func extractMCVersion(mavenVersion string) string {
	idx := strings.Index(mavenVersion, "-")
	if idx < 0 {
		return ""
	}
	return mavenVersion[:idx]
}

// extractForgeVersion extracts "47.3.0" from "1.20.1-47.3.0"
func extractForgeVersion(mavenVersion string) string {
	idx := strings.Index(mavenVersion, "-")
	if idx < 0 || idx+1 >= len(mavenVersion) {
		return ""
	}
	return mavenVersion[idx+1:]
}

