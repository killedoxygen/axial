package modloaders

import (
	"archive/zip"
	"bytes"
	"encoding/json"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

const (
	neoforgeMavenBase = "https://maven.neoforged.net/releases"
	neoforgeMavenMeta = neoforgeMavenBase + "/net/neoforged/neoforge/maven-metadata.xml"
)

type neoforgeLoader struct {
	cache  *MetaCache
	client *http.Client
}

// NewNeoForgeLoader creates a NeoForge loader backed by the given cache.
func NewNeoForgeLoader(cache *MetaCache) Loader {
	return &neoforgeLoader{
		cache:  cache,
		client: &http.Client{Timeout: 2 * time.Minute},
	}
}

func (n *neoforgeLoader) Type() LoaderType { return NeoForge }

func (n *neoforgeLoader) Info() LoaderInfo {
	return LoaderInfo{
		Type:        NeoForge,
		Name:        "NeoForge",
		Description: "Next-gen Forge fork",
	}
}

func (n *neoforgeLoader) NeedsBaseGameFirst() bool { return true }

func (n *neoforgeLoader) VersionID(mcVersion, loaderVersion string) string {
	// NeoForge version IDs vary; typically "neoforge-{loaderVersion}" but the
	// actual ID comes from the version.json inside the installer.
	return "neoforge-" + loaderVersion
}

func (n *neoforgeLoader) GameVersions() ([]GameVersion, error) {
	const cacheKey = "neoforge:game_versions"

	if versions, ok, fresh := cacheGetAs[[]GameVersion](n.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	entries, err := n.fetchMavenVersions()
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]GameVersion](n.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, err
	}

	// NeoForge version numbers encode the MC version:
	// 20.4.x -> MC 1.20.4, 21.0.x -> MC 1.21, 21.4.x -> MC 1.21.4
	seen := map[string]bool{}
	var versions []GameVersion
	for _, entry := range entries {
		mcv := neoforgeToMCVersion(entry)
		if mcv == "" || seen[mcv] {
			continue
		}
		seen[mcv] = true
		versions = append(versions, GameVersion{Version: mcv, Stable: true})
	}

	n.cache.Set(cacheKey, versions)
	return versions, nil
}

func (n *neoforgeLoader) LoaderVersions(mcVersion string) ([]LoaderVersion, error) {
	cacheKey := "neoforge:loader_versions:" + mcVersion

	if versions, ok, fresh := cacheGetAs[[]LoaderVersion](n.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	entries, err := n.fetchMavenVersions()
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]LoaderVersion](n.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, err
	}

	var versions []LoaderVersion
	for _, entry := range entries {
		mcv := neoforgeToMCVersion(entry)
		if mcv != mcVersion {
			continue
		}
		versions = append(versions, LoaderVersion{
			Version: entry,
			Stable:  !strings.Contains(entry, "beta"),
		})
	}

	// Maven metadata is oldest-first; reverse to newest-first.
	for i, j := 0, len(versions)-1; i < j; i, j = i+1, j-1 {
		versions[i], versions[j] = versions[j], versions[i]
	}

	n.cache.Set(cacheKey, versions)
	return versions, nil
}

func (n *neoforgeLoader) Install(mcDir, mcVersion, loaderVersion string, progress chan<- Progress) (*InstallResult, error) {
	safeMCVersion, err := sanitizeVersionSegment("minecraft version", mcVersion)
	if err != nil {
		return nil, err
	}
	safeLoaderVersion, err := sanitizeVersionSegment("loader version", loaderVersion)
	if err != nil {
		return nil, err
	}

	versionID := n.VersionID(safeMCVersion, safeLoaderVersion)

	return withInstallLock("neoforge:"+versionID, func() (*InstallResult, error) {
		versionDir, jsonPath, markerPath, err := resolveVersionFiles(mcDir, versionID)
		if err != nil {
			return nil, err
		}
		if _, err := os.Stat(jsonPath); err == nil {
			if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
				return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: NeoForge}, nil
			} else if mErr != nil {
				return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
			}
		} else if err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("checking installed NeoForge version: %w", err)
		}

		progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Downloading NeoForge installer..."}

		installerURL := fmt.Sprintf(
			"%s/net/neoforged/neoforge/%s/neoforge-%s-installer.jar",
			neoforgeMavenBase,
			url.PathEscape(safeLoaderVersion),
			url.PathEscape(safeLoaderVersion),
		)

		installerData, err := n.downloadToMemory(installerURL)
		if err != nil {
			return nil, fmt.Errorf("downloading NeoForge installer: %w", err)
		}

		progress <- Progress{Phase: "loader_json", Current: 0, Total: 1, Detail: "Extracting installer..."}

		versionJSON, installProfile, err := extractNeoForgeInstaller(installerData)
		if err != nil {
			return nil, fmt.Errorf("extracting NeoForge installer: %w", err)
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
					return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: NeoForge}, nil
				} else if mErr != nil {
					return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
				}
			} else if err != nil && !os.IsNotExist(err) {
				return nil, fmt.Errorf("checking installed NeoForge version: %w", err)
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

		allLibs, err := collectForgeLibraries(versionJSON, installProfile)
		if err != nil {
			return nil, fmt.Errorf("parsing NeoForge libraries: %w", err)
		}

		if err := DownloadLibraries(allLibs, mcDir, progress); err != nil {
			return nil, fmt.Errorf("downloading NeoForge libraries: %w", err)
		}

		if err := extractInstallerDataFiles(installerData, mcDir); err != nil {
			return nil, fmt.Errorf("extracting installer data: %w", err)
		}

		if installProfile != nil {
			progress <- Progress{Phase: "loader_processors", Current: 0, Total: 1, Detail: "Running processors..."}

			if err := RunForgeProcessors(mcDir, safeMCVersion, versionID, installProfile, installerData, progress); err != nil {
				return nil, fmt.Errorf("running NeoForge processors: %w", err)
			}
		}

		if err := os.Remove(markerPath); err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("removing incomplete marker: %w", err)
		}
		return &InstallResult{VersionID: versionID, GameVersion: safeMCVersion, LoaderType: NeoForge}, nil
	})
}

func (n *neoforgeLoader) fetchMavenVersions() ([]string, error) {
	const cacheKey = "neoforge:maven_versions"
	if versions, ok, fresh := cacheGetAs[[]string](n.cache, cacheKey); ok && fresh {
		return versions, nil
	}

	resp, err := n.client.Get(neoforgeMavenMeta)
	if err != nil {
		if versions, ok, _ := cacheGetAs[[]string](n.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("neoforge maven: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if versions, ok, _ := cacheGetAs[[]string](n.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("neoforge maven: status %d", resp.StatusCode)
	}

	var meta mavenMetadata
	if err := xml.NewDecoder(resp.Body).Decode(&meta); err != nil {
		if versions, ok, _ := cacheGetAs[[]string](n.cache, cacheKey); ok {
			return versions, nil
		}
		return nil, fmt.Errorf("parsing neoforge maven metadata: %w", err)
	}

	versions := meta.Versioning.Versions.Version
	n.cache.Set(cacheKey, versions)
	return versions, nil
}

func (n *neoforgeLoader) downloadToMemory(url string) ([]byte, error) {
	resp, err := n.client.Get(url)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("status %d for %s", resp.StatusCode, url)
	}

	return io.ReadAll(resp.Body)
}

// neoforgeToMCVersion maps a NeoForge version to its Minecraft version.
// E.g. "20.4.237" -> "1.20.4", "21.0.1" -> "1.21", "21.4.1" -> "1.21.4"
func neoforgeToMCVersion(neoVersion string) string {
	parts := strings.SplitN(neoVersion, ".", 3)
	if len(parts) < 2 {
		return ""
	}
	major := parts[0] // "20" or "21"
	minor := parts[1] // "4" or "0"

	if minor == "0" {
		return "1." + major
	}
	return "1." + major + "." + minor
}

// extractNeoForgeInstaller reads version.json and install_profile.json from the installer.
func extractNeoForgeInstaller(jarData []byte) (versionJSON []byte, installProfile []byte, err error) {
	r, err := zip.NewReader(bytes.NewReader(jarData), int64(len(jarData)))
	if err != nil {
		return nil, nil, fmt.Errorf("opening installer JAR: %w", err)
	}

	for _, f := range r.File {
		switch f.Name {
		case "version.json":
			rc, err := f.Open()
			if err != nil {
				return nil, nil, err
			}
			versionJSON, err = io.ReadAll(rc)
			rc.Close()
			if err != nil {
				return nil, nil, err
			}
		case "install_profile.json":
			rc, err := f.Open()
			if err != nil {
				return nil, nil, err
			}
			installProfile, err = io.ReadAll(rc)
			rc.Close()
			if err != nil {
				return nil, nil, err
			}
		}
	}

	if versionJSON == nil {
		return nil, nil, fmt.Errorf("version.json not found in installer JAR")
	}

	return versionJSON, installProfile, nil
}
