package modloaders

import (
	"archive/zip"
	"bytes"
	"encoding/json"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/mateoltd/croopor/internal/minecraft"
)

const (
	forgeMavenBase     = "https://maven.minecraftforge.net"
	forgeMavenMeta     = forgeMavenBase + "/net/minecraftforge/forge/maven-metadata.xml"
	forgePromotionsURL = "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json"
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
	cacheKey := "forge:loader_versions:" + mcVersion

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

	recommended := promos[mcVersion+"-recommended"]
	latest := promos[mcVersion+"-latest"]

	var versions []LoaderVersion
	for _, entry := range entries {
		mcv := extractMCVersion(entry)
		if mcv != mcVersion {
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
	versionID := f.VersionID(mcVersion, loaderVersion)

	return withInstallLock("forge:"+versionID, func() (*InstallResult, error) {
		versionDir := filepath.Join(minecraft.VersionsDir(mcDir), versionID)
		jsonPath := filepath.Join(versionDir, versionID+".json")
		markerPath := filepath.Join(versionDir, ".incomplete")
		if _, err := os.Stat(jsonPath); err == nil {
			if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
				return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Forge}, nil
			} else if mErr != nil {
				return nil, fmt.Errorf("checking incomplete marker: %w", mErr)
			}
		} else if err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("checking installed Forge version: %w", err)
		}

		progress <- Progress{Phase: "loader_meta", Current: 0, Total: 1, Detail: "Downloading Forge installer..."}

		mavenCoord := mcVersion + "-" + loaderVersion
		installerURL := fmt.Sprintf("%s/net/minecraftforge/forge/%s/forge-%s-installer.jar", forgeMavenBase, mavenCoord, mavenCoord)

		installerData, err := f.downloadToMemory(installerURL)
		if err != nil {
			return nil, fmt.Errorf("downloading Forge installer: %w", err)
		}

		progress <- Progress{Phase: "loader_json", Current: 0, Total: 1, Detail: "Extracting installer..."}

		versionJSON, installProfile, err := extractForgeInstaller(installerData)
		if err != nil {
			return nil, fmt.Errorf("extracting Forge installer: %w", err)
		}

		var parsedVersion struct {
			ID string `json:"id"`
		}
		if err := json.Unmarshal(versionJSON, &parsedVersion); err == nil && parsedVersion.ID != "" {
			versionID = parsedVersion.ID
			versionDir = filepath.Join(minecraft.VersionsDir(mcDir), versionID)
			jsonPath = filepath.Join(versionDir, versionID+".json")
			markerPath = filepath.Join(versionDir, ".incomplete")
			if _, err := os.Stat(jsonPath); err == nil {
				if _, mErr := os.Stat(markerPath); os.IsNotExist(mErr) {
					return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Forge}, nil
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

		allLibs, err := collectForgeLibraries(versionJSON, installProfile)
		if err != nil {
			return nil, fmt.Errorf("parsing Forge libraries: %w", err)
		}

		if err := DownloadLibraries(allLibs, mcDir, progress); err != nil {
			return nil, fmt.Errorf("downloading Forge libraries: %w", err)
		}

		if err := extractInstallerDataFiles(installerData, mcDir); err != nil {
			return nil, fmt.Errorf("extracting installer data: %w", err)
		}

		if installProfile != nil {
			progress <- Progress{Phase: "loader_processors", Current: 0, Total: 1, Detail: "Running processors..."}

			if err := RunForgeProcessors(mcDir, mcVersion, versionID, installProfile, installerData, progress); err != nil {
				return nil, fmt.Errorf("running Forge processors: %w", err)
			}
		}

		if err := os.Remove(markerPath); err != nil && !os.IsNotExist(err) {
			return nil, fmt.Errorf("removing incomplete marker: %w", err)
		}
		return &InstallResult{VersionID: versionID, GameVersion: mcVersion, LoaderType: Forge}, nil
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

	return io.ReadAll(resp.Body)
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

// extractForgeInstaller reads version.json and install_profile.json from the installer JAR.
func extractForgeInstaller(jarData []byte) (versionJSON []byte, installProfile []byte, err error) {
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

// collectForgeLibraries gathers libraries from both version.json and install_profile.json.
func collectForgeLibraries(versionJSON, installProfile []byte) ([]minecraft.Library, error) {
	var version struct {
		Libraries []minecraft.Library `json:"libraries"`
	}
	if err := json.Unmarshal(versionJSON, &version); err != nil {
		return nil, err
	}

	libs := version.Libraries

	if installProfile != nil {
		var profile struct {
			Libraries []minecraft.Library `json:"libraries"`
		}
		if err := json.Unmarshal(installProfile, &profile); err == nil {
			libs = append(libs, profile.Libraries...)
		}
	}

	return libs, nil
}

// extractInstallerDataFiles extracts data/ entries from the installer JAR
// into the libraries directory (Forge stores some artifacts this way).
func extractInstallerDataFiles(jarData []byte, mcDir string) error {
	r, err := zip.NewReader(bytes.NewReader(jarData), int64(len(jarData)))
	if err != nil {
		return err
	}

	libDir := minecraft.LibrariesDir(mcDir)
	for _, f := range r.File {
		if !strings.HasPrefix(f.Name, "maven/") {
			continue
		}

		relPath := strings.TrimPrefix(f.Name, "maven/")
		if relPath == "" || strings.HasSuffix(relPath, "/") {
			continue
		}

		destPath := filepath.Join(libDir, filepath.FromSlash(relPath))
		cleanLibDir := filepath.Clean(libDir)
		cleanDestPath := filepath.Clean(destPath)
		relDest, err := filepath.Rel(cleanLibDir, cleanDestPath)
		if err != nil || relDest == ".." || strings.HasPrefix(relDest, ".."+string(os.PathSeparator)) {
			continue
		}
		if _, err := os.Stat(destPath); err == nil {
			continue // Already exists
		}

		rc, err := f.Open()
		if err != nil {
			continue
		}
		data, err := io.ReadAll(rc)
		rc.Close()
		if err != nil {
			continue
		}

		if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
			return fmt.Errorf("creating installer library dir %s: %w", filepath.Dir(destPath), err)
		}
		if err := os.WriteFile(destPath, data, 0644); err != nil {
			return fmt.Errorf("writing installer library %s: %w", destPath, err)
		}
	}

	return nil
}
