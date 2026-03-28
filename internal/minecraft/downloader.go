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

// InstallVersion downloads all files needed to launch a version.
// manifestURL can be empty for incomplete local versions — the local JSON will be used.
func (d *Downloader) InstallVersion(versionID, manifestURL string) {
	defer close(d.ProgressCh)

	versionDir := filepath.Join(VersionsDir(d.MCDir), versionID)
	if err := os.MkdirAll(versionDir, 0755); err != nil {
		d.sendError("Failed to create version directory: " + err.Error())
		return
	}

	jsonPath := filepath.Join(versionDir, versionID+".json")

	// Phase 1: Get version JSON (download or use existing)
	d.send(DownloadProgress{Phase: "version_json", Current: 0, Total: 3, File: versionID + ".json"})

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
	d.send(DownloadProgress{Phase: "client_jar", Current: 1, Total: 3, File: versionID + ".jar"})

	if version.Downloads.Client != nil {
		jarPath := filepath.Join(versionDir, versionID+".jar")
		if !fileExistsWithSHA1(jarPath, version.Downloads.Client.SHA1) {
			if err := d.downloadFile(version.Downloads.Client.URL, jarPath, version.Downloads.Client.SHA1); err != nil {
				d.sendError("Failed to download client JAR: " + err.Error())
				return
			}
		}
	}

	// Phase 3: Download libraries
	env := DefaultEnvironment()
	libs := filterLibraries(version.Libraries, env)
	totalLibs := len(libs)

	d.send(DownloadProgress{Phase: "libraries", Current: 0, Total: totalLibs})

	for i, lib := range libs {
		libPath, libURL, libSHA1 := resolveLibDownload(lib, d.MCDir)
		if libPath == "" || libURL == "" {
			continue
		}

		d.send(DownloadProgress{Phase: "libraries", Current: i + 1, Total: totalLibs, File: filepath.Base(libPath)})

		if fileExistsWithSHA1(libPath, libSHA1) {
			continue
		}

		os.MkdirAll(filepath.Dir(libPath), 0755)
		d.downloadFile(libURL, libPath, libSHA1) // non-fatal
	}

	d.send(DownloadProgress{Phase: "done", Current: 3, Total: 3, Done: true})
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
