package server

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"time"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/system"
	appupdate "github.com/mateoltd/croopor/internal/update"
)

const updateCacheTTL = time.Minute

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	mcDir := s.GetMCDir()
	writeJSON(w, http.StatusOK, map[string]any{
		"status":         "ok",
		"mc_dir":         mcDir,
		"setup_required": mcDir == "",
		"app_name":       "Croopor",
		"version":        s.appVersion,
		"dev_mode":       devMode,
	})
}

func (s *Server) handleSystem(w http.ResponseWriter, r *http.Request) {
	totalMB, err := system.TotalMemoryMB()
	if err != nil {
		totalMB = 8192
	}
	recMin, recMax := system.RecommendedMemoryRange(totalMB)
	writeJSON(w, http.StatusOK, map[string]any{
		"total_memory_mb":    totalMB,
		"recommended_min_mb": recMin,
		"recommended_max_mb": recMax,
		"max_allocatable_gb": totalMB / 1024,
	})
}

func (s *Server) requireMCDir(w http.ResponseWriter) string {
	mcDir := s.GetMCDir()
	if mcDir == "" {
		writeError(w, http.StatusPreconditionFailed, "minecraft directory not configured")
	}
	return mcDir
}

func (s *Server) handleUpdate(w http.ResponseWriter, r *http.Request) {
	if s.updater == nil {
		writeError(w, http.StatusServiceUnavailable, "update service is not configured")
		return
	}
	force := r.URL.Query().Get("force") != ""
	if !force {
		if cached, ok := s.readCachedUpdate(); ok {
			writeJSON(w, http.StatusOK, cached)
			return
		}
	}

	s.updateCacheMu.Lock()
	defer s.updateCacheMu.Unlock()
	if !force && s.updateCache.ok && s.updateCache.version == s.appVersion && time.Since(s.updateCache.checked) < updateCacheTTL {
		writeJSON(w, http.StatusOK, s.updateCache.result)
		return
	}

	result, err := s.updater.Check(s.appVersion)
	if err != nil {
		writeError(w, http.StatusBadGateway, "failed to check updates: "+err.Error())
		return
	}
	s.updateCache = updateCacheEntry{
		version: s.appVersion,
		result:  result,
		checked: time.Now(),
		ok:      true,
	}
	writeJSON(w, http.StatusOK, result)
}

func (s *Server) readCachedUpdate() (appupdate.Result, bool) {
	s.updateCacheMu.RLock()
	defer s.updateCacheMu.RUnlock()
	if !s.updateCache.ok || s.updateCache.version != s.appVersion || time.Since(s.updateCache.checked) >= updateCacheTTL {
		return appupdate.Result{}, false
	}
	return s.updateCache.result, true
}

func (s *Server) handleJava(w http.ResponseWriter, r *http.Request) {
	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}
	runtimes := minecraft.ListJavaRuntimes(mcDir)
	writeJSON(w, http.StatusOK, map[string]any{"runtimes": runtimes})
}

func randomID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}

var musicTracks = []struct {
	File string
	URL  string
}{
	{"vapor-halo.mp3", "https://github.com/mateoltd/croopor/releases/download/music-v2/vapor-halo.mp3"},
	{"sublunar-hum.mp3", "https://github.com/mateoltd/croopor/releases/download/music-v2/sublunar-hum.mp3"},
}

var musicHTTPClient = &http.Client{Timeout: 2 * time.Minute}
var musicDownloadLocks sync.Map

func musicLocalPath(idx int) string {
	return filepath.Join(config.MusicDir(), musicTracks[idx].File)
}

// handleMusicTrack serves the cached music file, downloading it on first request.
// Uses http.ServeFile for zero-copy transfer with Range request support.
func (s *Server) handleMusicTrack(w http.ResponseWriter, r *http.Request) {
	if len(musicTracks) == 0 {
		writeError(w, http.StatusNotFound, "no music tracks available")
		return
	}

	idx := 0
	if idxStr := r.URL.Query().Get("t"); idxStr != "" {
		if i, err := strconv.Atoi(idxStr); err == nil {
			if i < 0 {
				i = 0
			}
			if i >= len(musicTracks) {
				i = len(musicTracks) - 1
			}
			idx = i
		}
	}

	localPath := musicLocalPath(idx)

	if _, err := os.Stat(localPath); err == nil {
		http.ServeFile(w, r, localPath)
		return
	}

	if err := withMusicDownloadLock(localPath, func() error {
		if _, err := os.Stat(localPath); err == nil {
			return nil
		}
		return downloadMusicFile(localPath, musicTracks[idx].URL)
	}); err != nil {
		log.Printf("Music download failed: %v", err)
		writeError(w, http.StatusBadGateway, "failed to download music: "+err.Error())
		return
	}

	http.ServeFile(w, r, localPath)
}

func downloadMusicFile(localPath, remoteURL string) error {
	if err := os.MkdirAll(filepath.Dir(localPath), 0755); err != nil {
		return fmt.Errorf("create directory: %w", err)
	}

	log.Printf("Downloading background music from %s", remoteURL)
	resp, err := musicHTTPClient.Get(remoteURL)
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %d", resp.StatusCode)
	}

	tmpPath := localPath + ".tmp"
	f, err := os.Create(tmpPath)
	if err != nil {
		return fmt.Errorf("create file: %w", err)
	}

	if _, err := io.Copy(f, resp.Body); err != nil {
		f.Close()
		os.Remove(tmpPath)
		return fmt.Errorf("write: %w", err)
	}
	if err := f.Close(); err != nil {
		os.Remove(tmpPath)
		return fmt.Errorf("close: %w", err)
	}

	if err := os.Rename(tmpPath, localPath); err != nil {
		os.Remove(tmpPath)
		return fmt.Errorf("finalize: %w", err)
	}

	log.Printf("Music cached at %s", localPath)
	return nil
}

// handleMusicStatus returns whether each track is cached locally.
func (s *Server) handleMusicStatus(w http.ResponseWriter, r *http.Request) {
	tracks := make([]map[string]any, len(musicTracks))
	for i, t := range musicTracks {
		_, err := os.Stat(filepath.Join(config.MusicDir(), t.File))
		tracks[i] = map[string]any{"cached": err == nil, "file": t.File}
	}
	writeJSON(w, http.StatusOK, map[string]any{"tracks": tracks, "count": len(musicTracks)})
}

func withMusicDownloadLock(path string, fn func() error) error {
	lockAny, _ := musicDownloadLocks.LoadOrStore(path, &sync.Mutex{})
	lock := lockAny.(*sync.Mutex)
	lock.Lock()
	defer lock.Unlock()
	return fn()
}

func sendSSE(w http.ResponseWriter, flusher http.Flusher, eventType string, data any) {
	jsonData, err := json.Marshal(data)
	if err != nil {
		return
	}
	fmt.Fprintf(w, "event: %s\ndata: %s\n\n", eventType, string(jsonData))
	flusher.Flush()
}
