package minecraft

import (
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type DownloadProgress struct {
	Phase   string `json:"phase"`
	Current int    `json:"current"`
	Total   int    `json:"total"`
	File    string `json:"file,omitempty"`
	Error   string `json:"error,omitempty"`
	Done    bool   `json:"done"`
}

type Downloader struct {
	MCDir      string
	ProgressCh chan DownloadProgress
	client     *http.Client
}

func NewDownloader(mcDir string) *Downloader {
	return &Downloader{
		MCDir:      mcDir,
		ProgressCh: make(chan DownloadProgress, 64),
		client:     &http.Client{Timeout: 5 * time.Minute},
	}
}

// InstallVersion downloads all files needed to launch a version:
// version JSON, client JAR, libraries (including native classifiers),
// asset index, asset objects, and log config.
func (d *Downloader) InstallVersion(versionID, manifestURL string) {
	defer close(d.ProgressCh)

	versionDir := filepath.Join(VersionsDir(d.MCDir), versionID)
	if err := os.MkdirAll(versionDir, 0755); err != nil {
		d.sendError("Failed to create version directory: " + err.Error())
		return
	}

	// Mark installation as in-progress. Removed on successful completion.
	// If the app crashes mid-install, the marker persists and the scanner
	// will flag this version as incomplete on next startup.
	markerPath := filepath.Join(versionDir, ".incomplete")
	os.WriteFile(markerPath, []byte("installing"), 0644)

	jsonPath := filepath.Join(versionDir, versionID+".json")

	// Phase 1: Get version JSON (download or use existing)
	d.send(DownloadProgress{Phase: "version_json", Current: 0, Total: 1, File: versionID + ".json"})

	if manifestURL != "" {
		// Download fresh version JSON from Mojang
		if err := d.downloadFile(manifestURL, jsonPath, ""); err != nil {
			d.sendError("Failed to download version JSON: " + err.Error())
			return
		}
	} else if _, err := os.Stat(jsonPath); os.IsNotExist(err) {
		// No manifest URL and no local JSON — try to resolve from manifest
		resolved, lookupErr := d.resolveManifestURL(versionID)
		if lookupErr != nil || resolved == "" {
			d.sendError("Cannot find download URL for version " + versionID)
			return
		}
		if err := d.downloadFile(resolved, jsonPath, ""); err != nil {
			d.sendError("Failed to download version JSON: " + err.Error())
			return
		}
	}
	// else: local JSON exists, use it

	data, err := os.ReadFile(jsonPath)
	if err != nil {
		d.sendError("Failed to read version JSON: " + err.Error())
		return
	}

	var version VersionJSON
	if err := json.Unmarshal(data, &version); err != nil {
		d.sendError("Failed to parse version JSON: " + err.Error())
		return
	}

	// Phase 2: Download client JAR
	d.send(DownloadProgress{Phase: "client_jar", Current: 0, Total: 1, File: versionID + ".jar"})

	if version.Downloads.Client != nil {
		jarPath := filepath.Join(versionDir, versionID+".jar")
		if !fileExistsWithSHA1(jarPath, version.Downloads.Client.SHA1) {
			if err := d.downloadFile(version.Downloads.Client.URL, jarPath, version.Downloads.Client.SHA1); err != nil {
				d.sendError("Failed to download client JAR: " + err.Error())
				return
			}
		}
	}

	// Phase 3: Download libraries (including native classifier JARs)
	env := DefaultEnvironment()
	libs := filterLibraries(version.Libraries, env)
	totalLibs := len(libs)

	d.send(DownloadProgress{Phase: "libraries", Current: 0, Total: totalLibs})

	for i, lib := range libs {
		// Download main artifact
		libPath, libURL, libSHA1 := resolveLibDownload(lib, d.MCDir)
		if libPath != "" && libURL != "" {
			d.send(DownloadProgress{Phase: "libraries", Current: i + 1, Total: totalLibs, File: filepath.Base(libPath)})
			if !fileExistsWithSHA1(libPath, libSHA1) {
				os.MkdirAll(filepath.Dir(libPath), 0755)
				d.downloadFile(libURL, libPath, libSHA1) // non-fatal
			}
		}

		// Download native classifier JAR if this library has a natives map
		natPath, natURL, natSHA1 := resolveNativeDownload(lib, d.MCDir, env)
		if natPath != "" && natURL != "" && !fileExistsWithSHA1(natPath, natSHA1) {
			os.MkdirAll(filepath.Dir(natPath), 0755)
			d.downloadFile(natURL, natPath, natSHA1) // non-fatal
		}
	}

	// Phase 4: Download asset index
	if version.AssetIndex.URL != "" {
		assetIndexPath := filepath.Join(AssetsDir(d.MCDir), "indexes", version.AssetIndex.ID+".json")
		d.send(DownloadProgress{Phase: "asset_index", Current: 0, Total: 1, File: version.AssetIndex.ID + ".json"})
		if !fileExistsWithSHA1(assetIndexPath, version.AssetIndex.SHA1) {
			if err := d.downloadFile(version.AssetIndex.URL, assetIndexPath, version.AssetIndex.SHA1); err != nil {
				d.sendError("Failed to download asset index: " + err.Error())
				return
			}
		}

		// Phase 5: Download asset objects
		d.downloadAssetObjects(assetIndexPath)
	}

	// Phase 6: Download log config file
	if version.Logging != nil && version.Logging.Client != nil && version.Logging.Client.File.URL != "" {
		logConfigPath := filepath.Join(AssetsDir(d.MCDir), "log_configs", version.Logging.Client.File.ID)
		if !fileExistsWithSHA1(logConfigPath, version.Logging.Client.File.SHA1) {
			d.send(DownloadProgress{Phase: "log_config", Current: 0, Total: 1, File: version.Logging.Client.File.ID})
			d.downloadFile(version.Logging.Client.File.URL, logConfigPath, version.Logging.Client.File.SHA1)
		}
	}

	// Ensure launcher_profiles.json exists for mod loader compatibility
	EnsureLauncherProfiles(d.MCDir, versionID)

	// Installation succeeded — remove the incomplete marker
	os.Remove(filepath.Join(versionDir, ".incomplete"))

	d.send(DownloadProgress{Phase: "done", Current: 1, Total: 1, Done: true})
}

// resolveNativeDownload finds the native classifier JAR for a library with a "natives" map.
// Legacy versions (<=1.12) store native DLLs in classifier JARs like "natives-windows".
func resolveNativeDownload(lib Library, mcDir string, env Environment) (path, url, sha1 string) {
	if lib.Natives == nil {
		return "", "", ""
	}
	classifierKey, ok := lib.Natives[env.OSName]
	if !ok {
		return "", "", ""
	}
	classifierKey = strings.ReplaceAll(classifierKey, "${arch}", archBits())

	libDir := LibrariesDir(mcDir)
	if lib.Downloads != nil && lib.Downloads.Classifiers != nil {
		if artifact, ok := lib.Downloads.Classifiers[classifierKey]; ok {
			return filepath.Join(libDir, filepath.FromSlash(artifact.Path)), artifact.URL, artifact.SHA1
		}
	}
	return "", "", ""
}

// downloadAssetObjects reads the asset index and downloads all referenced objects.
// For legacy/virtual indexes, it also creates the virtual directory structure.
func (d *Downloader) downloadAssetObjects(indexPath string) {
	data, err := os.ReadFile(indexPath)
	if err != nil {
		return
	}

	var index struct {
		Objects map[string]struct {
			Hash string `json:"hash"`
			Size int64  `json:"size"`
		} `json:"objects"`
		Virtual        bool `json:"virtual"`
		MapToResources bool `json:"map_to_resources"`
	}
	if err := json.Unmarshal(data, &index); err != nil {
		return
	}

	objectsDir := filepath.Join(AssetsDir(d.MCDir), "objects")
	total := len(index.Objects)
	current := 0

	for _, obj := range index.Objects {
		current++
		if current%100 == 1 || current == total {
			d.send(DownloadProgress{Phase: "assets", Current: current, Total: total})
		}

		prefix := obj.Hash[:2]
		objPath := filepath.Join(objectsDir, prefix, obj.Hash)
		if fileExistsWithSHA1(objPath, obj.Hash) {
			continue
		}

		url := "https://resources.download.minecraft.net/" + prefix + "/" + obj.Hash
		d.downloadFile(url, objPath, obj.Hash) // non-fatal per object
	}

	// For legacy/virtual asset indexes (pre-1.6), create the virtual directory
	// so ${game_assets} points to actual files at their original paths.
	if index.Virtual || index.MapToResources {
		virtualDir := filepath.Join(AssetsDir(d.MCDir), "virtual", "legacy")
		for name, obj := range index.Objects {
			dstPath := filepath.Join(virtualDir, filepath.FromSlash(name))
			if _, err := os.Stat(dstPath); err == nil {
				continue
			}
			prefix := obj.Hash[:2]
			srcPath := filepath.Join(objectsDir, prefix, obj.Hash)
			srcData, err := os.ReadFile(srcPath)
			if err != nil {
				continue
			}
			os.MkdirAll(filepath.Dir(dstPath), 0755)
			os.WriteFile(dstPath, srcData, 0644)
		}
	}
}

func (d *Downloader) resolveManifestURL(versionID string) (string, error) {
	manifest, err := FetchVersionManifest()
	if err != nil {
		return "", err
	}
	for _, entry := range manifest.Versions {
		if entry.ID == versionID {
			return entry.URL, nil
		}
	}
	return "", fmt.Errorf("version %s not found in manifest", versionID)
}

func filterLibraries(libs []Library, env Environment) []Library {
	var result []Library
	for _, lib := range libs {
		if EvaluateRules(lib.Rules, env) {
			result = append(result, lib)
		}
	}
	return result
}

func resolveLibDownload(lib Library, mcDir string) (path, url, sha1 string) {
	libDir := LibrariesDir(mcDir)

	if lib.Downloads != nil && lib.Downloads.Artifact != nil {
		a := lib.Downloads.Artifact
		return filepath.Join(libDir, filepath.FromSlash(a.Path)), a.URL, a.SHA1
	}

	mavenPath := MavenToPath(lib.Name)
	if mavenPath == "" {
		return "", "", ""
	}

	absPath := filepath.Join(libDir, mavenPath)
	baseURL := lib.URL
	if baseURL == "" {
		baseURL = "https://libraries.minecraft.net/"
	}
	urlPath := filepath.ToSlash(mavenPath)
	return absPath, baseURL + urlPath, lib.SHA1
}

func (d *Downloader) downloadFile(url, destPath, expectedSHA1 string) error {
	os.MkdirAll(filepath.Dir(destPath), 0755)

	tmpPath := destPath + ".tmp"
	resp, err := d.client.Get(url)
	if err != nil {
		return fmt.Errorf("GET %s: %w", url, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("GET %s: status %d", url, resp.StatusCode)
	}

	out, err := os.Create(tmpPath)
	if err != nil {
		return err
	}

	h := sha1.New()
	if _, err := io.Copy(io.MultiWriter(out, h), resp.Body); err != nil {
		out.Close()
		os.Remove(tmpPath)
		return err
	}
	out.Close()

	if expectedSHA1 != "" {
		actual := hex.EncodeToString(h.Sum(nil))
		if actual != expectedSHA1 {
			os.Remove(tmpPath)
			return fmt.Errorf("SHA1 mismatch for %s", filepath.Base(destPath))
		}
	}

	return os.Rename(tmpPath, destPath)
}

func fileExistsWithSHA1(path, expectedSHA1 string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	if expectedSHA1 == "" {
		return true
	}
	f, err := os.Open(path)
	if err != nil {
		return false
	}
	defer f.Close()
	h := sha1.New()
	io.Copy(h, f)
	return hex.EncodeToString(h.Sum(nil)) == expectedSHA1
}

func (d *Downloader) send(p DownloadProgress) {
	select {
	case d.ProgressCh <- p:
	default:
	}
}

func (d *Downloader) sendError(msg string) {
	d.send(DownloadProgress{Phase: "error", Error: msg, Done: true})
}
