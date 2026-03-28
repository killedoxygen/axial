package server

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"log"
	"net/http"

	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/launcher"
	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
	"github.com/mateoltd/mc-paralauncher/internal/system"
)

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"status":   "ok",
		"mc_dir":   s.mcDir,
		"app_name": "Croopor",
		"version":  "1.0.0",
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

// handleVersions returns ONLY locally installed versions.
func (s *Server) handleVersions(w http.ResponseWriter, r *http.Request) {
	versions, err := minecraft.ScanVersions(s.mcDir)
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to scan versions: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"versions": versions})
}

// handleCatalog returns the remote Mojang version catalog for browsing/installing.
func (s *Server) handleCatalog(w http.ResponseWriter, r *http.Request) {
	manifest, err := minecraft.FetchVersionManifest()
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to fetch catalog: "+err.Error())
		return
	}

	// Build a set of locally installed version IDs for marking
	local, _ := minecraft.ScanVersions(s.mcDir)
	installedSet := make(map[string]bool, len(local))
	for _, v := range local {
		if v.Launchable {
			installedSet[v.ID] = true
		}
	}

	type catalogEntry struct {
		ID          string `json:"id"`
		Type        string `json:"type"`
		ReleaseTime string `json:"release_time"`
		URL         string `json:"url"`
		Installed   bool   `json:"installed"`
	}

	entries := make([]catalogEntry, 0, len(manifest.Versions))
	for _, v := range manifest.Versions {
		entries = append(entries, catalogEntry{
			ID:          v.ID,
			Type:        v.Type,
			ReleaseTime: v.ReleaseTime,
			URL:         v.URL,
			Installed:   installedSet[v.ID],
		})
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"latest":   manifest.Latest,
		"versions": entries,
	})
}

func (s *Server) handleGetConfig(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, s.config)
}

func (s *Server) handleUpdateConfig(w http.ResponseWriter, r *http.Request) {
	var updates map[string]any
	if err := json.NewDecoder(r.Body).Decode(&updates); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	if v, ok := updates["username"].(string); ok && v != "" {
		s.config.Username = v
	}
	if v, ok := updates["max_memory_mb"].(float64); ok && v > 0 {
		s.config.MaxMemoryMB = int(v)
	}
	if v, ok := updates["min_memory_mb"].(float64); ok && v > 0 {
		s.config.MinMemoryMB = int(v)
	}
	if v, ok := updates["last_version_id"].(string); ok && v != "" {
		s.config.LastVersionID = v
	}
	if v, ok := updates["java_path_override"].(string); ok {
		s.config.JavaPathOverride = v
	}
	if v, ok := updates["window_width"].(float64); ok {
		s.config.WindowWidth = int(v)
	}
	if v, ok := updates["window_height"].(float64); ok {
		s.config.WindowHeight = int(v)
	}
	if v, ok := updates["onboarding_done"].(bool); ok {
		s.config.OnboardingDone = v
	}

	if err := config.Save(s.config); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to save config: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, s.config)
}

func (s *Server) handleOnboardingComplete(w http.ResponseWriter, r *http.Request) {
	s.config.OnboardingDone = true
	if err := config.Save(s.config); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to save: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

type launchRequest struct {
	VersionID   string `json:"version_id"`
	Username    string `json:"username"`
	MaxMemoryMB int    `json:"max_memory_mb"`
	MinMemoryMB int    `json:"min_memory_mb"`
}

func (s *Server) handleLaunch(w http.ResponseWriter, r *http.Request) {
	var req launchRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}
	if req.VersionID == "" {
		writeError(w, http.StatusBadRequest, "version_id is required")
		return
	}

	username := req.Username
	if username == "" {
		username = s.config.Username
	}
	maxMem := req.MaxMemoryMB
	if maxMem <= 0 {
		maxMem = s.config.MaxMemoryMB
	}
	minMem := req.MinMemoryMB
	if minMem <= 0 {
		minMem = s.config.MinMemoryMB
	}

	result, err := launcher.BuildAndLaunch(launcher.LaunchOptions{
		VersionID:   req.VersionID,
		Username:    username,
		MaxMemoryMB: maxMem,
		MinMemoryMB: minMem,
		MCDir:       s.mcDir,
		Config:      s.config,
	})
	if err != nil {
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}

	s.sessions.Add(result)
	s.config.LastVersionID = req.VersionID
	s.config.Username = username
	config.Save(s.config)

	writeJSON(w, http.StatusOK, map[string]any{
		"status":     "launching",
		"session_id": result.SessionID,
		"pid":        result.Process.PID(),
	})
}

func (s *Server) handleLaunchCommand(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	result, ok := s.sessions.Get(id)
	if !ok {
		writeError(w, http.StatusNotFound, "session not found")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"command":    result.Command,
		"java_path":  result.JavaPath,
		"session_id": result.SessionID,
	})
}

func (s *Server) handleKillProcess(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	result, ok := s.sessions.Get(id)
	if !ok {
		writeError(w, http.StatusNotFound, "session not found")
		return
	}
	if err := result.Process.Kill(); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to kill: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "killed"})
}

type installRequest struct {
	VersionID   string `json:"version_id"`
	ManifestURL string `json:"manifest_url,omitempty"`
}

// handleInstall starts a version download. manifest_url is optional —
// if empty, the downloader resolves it from the Mojang manifest or uses local JSON.
func (s *Server) handleInstall(w http.ResponseWriter, r *http.Request) {
	var req installRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}
	if req.VersionID == "" {
		writeError(w, http.StatusBadRequest, "version_id is required")
		return
	}

	// manifest_url is now optional — the downloader resolves it if empty
	installID := randomID()
	dl := minecraft.NewDownloader(s.mcDir)
	s.installs.Add(installID, dl)

	log.Printf("Starting install of %s (manifest_url=%q)", req.VersionID, req.ManifestURL)
	go dl.InstallVersion(req.VersionID, req.ManifestURL)

	writeJSON(w, http.StatusOK, map[string]string{"install_id": installID})
}

func (s *Server) handleInstallEvents(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	dl, ok := s.installs.Get(id)
	if !ok {
		writeError(w, http.StatusNotFound, "install session not found")
		return
	}

	flusher, ok := w.(http.Flusher)
	if !ok {
		writeError(w, http.StatusInternalServerError, "streaming not supported")
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")

	ctx := r.Context()
	for {
		select {
		case <-ctx.Done():
			return
		case p, ok := <-dl.ProgressCh:
			if !ok {
				return
			}
			sendSSE(w, flusher, "progress", p)
			if p.Done {
				return
			}
		}
	}
}

func (s *Server) handleJava(w http.ResponseWriter, r *http.Request) {
	runtimes := minecraft.ListJavaRuntimes(s.mcDir)
	writeJSON(w, http.StatusOK, map[string]any{"runtimes": runtimes})
}

func randomID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}
