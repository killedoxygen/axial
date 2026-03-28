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

// DownloadProgress reports the current state of a version download.
type DownloadProgress struct {
	Phase      string `json:"phase"`
	Current    int    `json:"current"`
	Total      int    `json:"total"`
	File       string `json:"file,omitempty"`
	Error      string `json:"error,omitempty"`
	Done       bool   `json:"done"`
}

// Downloader handles downloading Minecraft version files from Mojang servers.
type Downloader struct {
	MCDir      string
	ProgressCh chan DownloadProgress
	client     *http.Client
}

// NewDownloader creates a new downloader.
func NewDownloader(mcDir string) *Downloader {
	return &Downloader{
		MCDir:      mcDir,
		ProgressCh: make(chan DownloadProgress, 64),
		client:     &http.Client{Timeout: 5 * time.Minute},
	}
}

// InstallVersion downloads all files needed to launch a version.
func (d *Downloader) InstallVersion(versionID, manifestURL string) {
	defer func() {
		close(d.ProgressCh)
	}()

	// Phase 1: Download version JSON
	d.send(DownloadProgress{Phase: "version_json", Current: 0, Total: 3, File: versionID + ".json"})

	versionDir := filepath.Join(VersionsDir(d.MCDir), versionID)
	if err := os.MkdirAll(versionDir, 0755); err != nil {
		d.sendError("Failed to create version directory: " + err.Error())
		return
	}

	jsonPath := filepath.Join(versionDir, versionID+".json")
	if err := d.downloadFile(manifestURL, jsonPath, ""); err != nil {
		d.sendError("Failed to download version JSON: " + err.Error())
		return
	}

	// Parse the version JSON to get download URLs
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

		dir := filepath.Dir(libPath)
		if err := os.MkdirAll(dir, 0755); err != nil {
			continue // skip this library, non-fatal
		}

		if err := d.downloadFile(libURL, libPath, libSHA1); err != nil {
			continue // skip, non-fatal
		}
	}

	d.send(DownloadProgress{Phase: "done", Current: 3, Total: 3, Done: true})
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

	// Standard artifact
	if lib.Downloads != nil && lib.Downloads.Artifact != nil {
		a := lib.Downloads.Artifact
		return filepath.Join(libDir, filepath.FromSlash(a.Path)), a.URL, a.SHA1
	}

	// Maven coordinate fallback
	mavenPath := MavenToPath(lib.Name)
	if mavenPath == "" {
		return "", "", ""
	}

	absPath := filepath.Join(libDir, mavenPath)

	// Construct URL from maven base URL
	baseURL := lib.URL
	if baseURL == "" {
		baseURL = "https://libraries.minecraft.net/"
	}
	// Convert path separators to URL slashes
	urlPath := filepath.ToSlash(mavenPath)
	return absPath, baseURL + urlPath, lib.SHA1
}

func (d *Downloader) downloadFile(url, destPath, expectedSHA1 string) error {
	if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
		return err
	}

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
	writer := io.MultiWriter(out, h)

	if _, err := io.Copy(writer, resp.Body); err != nil {
		out.Close()
		os.Remove(tmpPath)
		return err
	}
	out.Close()

	// Verify SHA1 if provided
	if expectedSHA1 != "" {
		actualSHA1 := hex.EncodeToString(h.Sum(nil))
		if actualSHA1 != expectedSHA1 {
			os.Remove(tmpPath)
			return fmt.Errorf("SHA1 mismatch for %s: expected %s, got %s", filepath.Base(destPath), expectedSHA1, actualSHA1)
		}
	}

	return os.Rename(tmpPath, destPath)
}

func fileExistsWithSHA1(path, expectedSHA1 string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	if expectedSHA1 == "" {
		return true // file exists, no SHA1 to check
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
