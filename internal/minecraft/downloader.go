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
	"sync"
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
	if err := os.WriteFile(markerPath, []byte("installing"), 0644); err != nil {
		d.sendError("Failed to create install marker: " + err.Error())
		return
	}

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
		// No manifest URL and no local JSON, try to resolve from the manifest
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
		if !FileExistsWithSHA1(jarPath, version.Downloads.Client.SHA1) {
			if err := d.downloadFile(version.Downloads.Client.URL, jarPath, version.Downloads.Client.SHA1); err != nil {
				d.sendError("Failed to download client JAR: " + err.Error())
				return
			}
		}
	}

	// Phase 3: Download libraries (including native classifier JARs)
	env := DefaultEnvironment()
	libs := FilterLibraries(version.Libraries, env)

	type libJob struct {
		path string
		url  string
		sha1 string
		name string
	}

	var libJobs []libJob
	for _, lib := range libs {
		libPath, libURL, libSHA1 := ResolveLibDownload(lib, d.MCDir)
		if libPath != "" && libURL != "" && !FileExistsWithSHA1(libPath, libSHA1) {
			libJobs = append(libJobs, libJob{path: libPath, url: libURL, sha1: libSHA1, name: filepath.Base(libPath)})
		}
		natPath, natURL, natSHA1 := resolveNativeDownload(lib, d.MCDir, env)
		if natPath != "" && natURL != "" && !FileExistsWithSHA1(natPath, natSHA1) {
			libJobs = append(libJobs, libJob{path: natPath, url: natURL, sha1: natSHA1, name: filepath.Base(natPath)})
		}
	}

	totalLibs := len(libJobs)
	d.send(DownloadProgress{Phase: "libraries", Current: 0, Total: totalLibs})

	if totalLibs > 0 {
		var libMu sync.Mutex
		var libCompleted int
		var libErr error
		libSem := make(chan struct{}, 4)
		var libWg sync.WaitGroup

		for _, job := range libJobs {
			libWg.Add(1)
			libSem <- struct{}{}
			go func(j libJob) {
				defer libWg.Done()
				defer func() { <-libSem }()

				libMu.Lock()
				failed := libErr != nil
				libMu.Unlock()
				if failed {
					return
				}

				if err := os.MkdirAll(filepath.Dir(j.path), 0755); err != nil {
					libMu.Lock()
					if libErr == nil {
						libErr = err
					}
					libMu.Unlock()
					return
				}
				if err := d.downloadFile(j.url, j.path, j.sha1); err != nil {
					libMu.Lock()
					if libErr == nil {
						libErr = err
					}
					libMu.Unlock()
					return
				}

				libMu.Lock()
				libCompleted++
				d.send(DownloadProgress{Phase: "libraries", Current: libCompleted, Total: totalLibs, File: j.name})
				libMu.Unlock()
			}(job)
		}

		libWg.Wait()

		if libErr != nil {
			d.sendError("Failed to download library: " + libErr.Error())
			return
		}
	}

	// Phase 4: Download asset index
	if version.AssetIndex.URL != "" {
		assetIndexPath := filepath.Join(AssetsDir(d.MCDir), "indexes", version.AssetIndex.ID+".json")
		d.send(DownloadProgress{Phase: "asset_index", Current: 0, Total: 1, File: version.AssetIndex.ID + ".json"})
		if !FileExistsWithSHA1(assetIndexPath, version.AssetIndex.SHA1) {
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
		if !FileExistsWithSHA1(logConfigPath, version.Logging.Client.File.SHA1) {
			d.send(DownloadProgress{Phase: "log_config", Current: 0, Total: 1, File: version.Logging.Client.File.ID})
			if err := d.downloadFile(version.Logging.Client.File.URL, logConfigPath, version.Logging.Client.File.SHA1); err != nil {
				d.sendError("Failed to download log config: " + err.Error())
				return
			}
		}
	}

	// Ensure launcher_profiles.json exists for mod loader compatibility
	EnsureLauncherProfiles(d.MCDir, versionID)

	// Installation succeeded, remove the incomplete marker
	if err := os.Remove(markerPath); err != nil && !os.IsNotExist(err) {
		d.sendError("Failed to finalize install marker: " + err.Error())
		return
	}

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
			if resolved, ok := resolvePathUnderRoot(libDir, artifact.Path); ok {
				return resolved, artifact.URL, artifact.SHA1
			}
		}
	}

	// Maven-coordinate fallback for old versions that omit the downloads block entirely.
	// Construct the classifier path from lib.Name + classifier key.
	mavenPath := MavenToPath(lib.Name + ":" + classifierKey)
	if mavenPath == "" {
		return "", "", ""
	}
	absPath := filepath.Join(libDir, mavenPath)
	baseURL := lib.URL
	if baseURL == "" {
		baseURL = "https://libraries.minecraft.net/"
	}
	if !strings.HasSuffix(baseURL, "/") {
		baseURL += "/"
	}
	return absPath, baseURL + filepath.ToSlash(mavenPath), ""
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

	type assetJob struct {
		hash string
		path string
	}

	var jobs []assetJob
	for _, obj := range index.Objects {
		prefix := obj.Hash[:2]
		objPath := filepath.Join(objectsDir, prefix, obj.Hash)
		if FileExistsWithSHA1(objPath, obj.Hash) {
			continue
		}
		jobs = append(jobs, assetJob{hash: obj.Hash, path: objPath})
	}

	total := len(jobs)
	if total > 0 {
		var mu sync.Mutex
		var completed int
		sem := make(chan struct{}, 8)
		var wg sync.WaitGroup

		d.send(DownloadProgress{Phase: "assets", Current: 0, Total: total})

		for _, job := range jobs {
			wg.Add(1)
			sem <- struct{}{}
			go func(j assetJob) {
				defer wg.Done()
				defer func() { <-sem }()

				url := "https://resources.download.minecraft.net/" + j.hash[:2] + "/" + j.hash
				d.downloadFile(url, j.path, j.hash) // non-fatal per object

				mu.Lock()
				completed++
				if completed%50 == 0 || completed == total {
					d.send(DownloadProgress{Phase: "assets", Current: completed, Total: total})
				}
				mu.Unlock()
			}(job)
		}

		wg.Wait()
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

// FilterLibraries returns only the libraries whose rules match the given environment.
func FilterLibraries(libs []Library, env Environment) []Library {
	var result []Library
	for _, lib := range libs {
		if EvaluateRules(lib.Rules, env) {
			result = append(result, lib)
		}
	}
	return result
}

// ResolveLibDownload determines the local path, download URL, and expected SHA1
// for a library's main artifact.
func ResolveLibDownload(lib Library, mcDir string) (path, url, sha1 string) {
	libDir := LibrariesDir(mcDir)

	// Libraries that explicitly omit a main artifact only ship classifier JARs.
	// If the entire downloads block is missing, fall back to Maven coordinates below.
	if lib.Natives != nil && lib.Downloads != nil && lib.Downloads.Artifact == nil {
		return "", "", ""
	}

	if lib.Downloads != nil && lib.Downloads.Artifact != nil {
		a := lib.Downloads.Artifact
		if resolved, ok := resolvePathUnderRoot(libDir, a.Path); ok {
			return resolved, a.URL, a.SHA1
		}
		return "", "", ""
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
	if !strings.HasSuffix(baseURL, "/") {
		baseURL += "/"
	}
	urlPath := filepath.ToSlash(mavenPath)
	return absPath, baseURL + urlPath, lib.SHA1
}

func resolvePathUnderRoot(root, rel string) (string, bool) {
	cleanRel := filepath.Clean(filepath.FromSlash(rel))
	if cleanRel == "." || cleanRel == "" || filepath.IsAbs(cleanRel) {
		return "", false
	}

	joined := filepath.Join(root, cleanRel)
	relPath, err := filepath.Rel(root, joined)
	if err != nil || relPath == ".." || strings.HasPrefix(relPath, ".."+string(os.PathSeparator)) {
		return "", false
	}
	return joined, true
}

func (d *Downloader) downloadFile(url, destPath, expectedSHA1 string) error {
	return DownloadFile(d.client, url, destPath, expectedSHA1)
}

// DownloadFile downloads a URL to destPath with optional SHA1 verification.
// It writes to a .tmp file first and renames on success for atomicity.
func DownloadFile(client *http.Client, url, destPath, expectedSHA1 string) error {
	if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
		return err
	}

	resp, err := client.Get(url)
	if err != nil {
		return fmt.Errorf("GET %s: %w", url, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("GET %s: status %d", url, resp.StatusCode)
	}

	out, err := os.CreateTemp(filepath.Dir(destPath), filepath.Base(destPath)+".*.tmp")
	if err != nil {
		return err
	}
	tmpPath := out.Name()
	defer os.Remove(tmpPath)

	h := sha1.New()
	if _, err := io.Copy(io.MultiWriter(out, h), resp.Body); err != nil {
		out.Close()
		return err
	}
	if err := out.Close(); err != nil {
		return err
	}

	if expectedSHA1 != "" {
		actual := hex.EncodeToString(h.Sum(nil))
		if actual != expectedSHA1 {
			return fmt.Errorf("SHA1 mismatch for %s", filepath.Base(destPath))
		}
	}

	if err := os.Rename(tmpPath, destPath); err != nil {
		if expectedSHA1 != "" && FileExistsWithSHA1(destPath, expectedSHA1) {
			return nil
		}
		return err
	}
	return nil
}

// FileExistsWithSHA1 checks whether a file exists and optionally matches the expected hash.
func FileExistsWithSHA1(path, expectedSHA1 string) bool {
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() {
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
	if _, err := io.Copy(h, f); err != nil {
		return false
	}
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
