package server

import (
	"encoding/json"
	"net/http"

	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/launcher"
	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
)

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"status":   "ok",
		"mc_dir":   s.mcDir,
		"app_name": "ParaLauncher",
		"version":  "1.0.0",
	})
}

func (s *Server) handleVersions(w http.ResponseWriter, r *http.Request) {
	versions, err := minecraft.ScanVersions(s.mcDir)
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to scan versions: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"versions": versions,
	})
}

func (s *Server) handleGetConfig(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, s.config)
}

func (s *Server) handleUpdateConfig(w http.ResponseWriter, r *http.Request) {
	var updates config.Config
	if err := json.NewDecoder(r.Body).Decode(&updates); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	// Apply non-zero updates
	if updates.Username != "" {
		s.config.Username = updates.Username
	}
	if updates.MaxMemoryMB > 0 {
		s.config.MaxMemoryMB = updates.MaxMemoryMB
	}
	if updates.MinMemoryMB > 0 {
		s.config.MinMemoryMB = updates.MinMemoryMB
	}
	if updates.LastVersionID != "" {
		s.config.LastVersionID = updates.LastVersionID
	}
	if updates.JavaPathOverride != "" {
		s.config.JavaPathOverride = updates.JavaPathOverride
	}
	if updates.WindowWidth > 0 {
		s.config.WindowWidth = updates.WindowWidth
	}
	if updates.WindowHeight > 0 {
		s.config.WindowHeight = updates.WindowHeight
	}

	if err := config.Save(s.config); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to save config: "+err.Error())
		return
	}

	writeJSON(w, http.StatusOK, s.config)
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

	// Use request values or fall back to config
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

	opts := launcher.LaunchOptions{
		VersionID:   req.VersionID,
		Username:    username,
		MaxMemoryMB: maxMem,
		MinMemoryMB: minMem,
		MCDir:       s.mcDir,
		Config:      s.config,
	}

	result, err := launcher.BuildAndLaunch(opts)
	if err != nil {
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}

	s.sessions.Add(result)

	// Update config with last used version
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
		writeError(w, http.StatusInternalServerError, "failed to kill process: "+err.Error())
		return
	}

	writeJSON(w, http.StatusOK, map[string]string{"status": "killed"})
}

func (s *Server) handleJava(w http.ResponseWriter, r *http.Request) {
	runtimes := minecraft.ListJavaRuntimes(s.mcDir)
	writeJSON(w, http.StatusOK, map[string]any{
		"runtimes": runtimes,
	})
}
