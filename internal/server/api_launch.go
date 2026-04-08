package server

import (
	"encoding/json"
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/instance"
	"github.com/mateoltd/croopor/internal/launcher"
	"github.com/mateoltd/croopor/internal/minecraft"
)

type launchRequest struct {
	InstanceID  string `json:"instance_id"`
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
	if req.InstanceID == "" {
		writeError(w, http.StatusBadRequest, "instance_id is required")
		return
	}

	inst := s.instances.Get(req.InstanceID)
	if inst == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}

	// Resolve settings: instance overrides > request overrides > global config
	s.mu.RLock()
	username := req.Username
	if username == "" {
		username = s.config.Username
	}
	maxMem := inst.MaxMemoryMB
	if maxMem <= 0 && req.MaxMemoryMB > 0 {
		maxMem = req.MaxMemoryMB
	}
	if maxMem <= 0 {
		maxMem = s.config.MaxMemoryMB
	}
	minMem := inst.MinMemoryMB
	if minMem <= 0 && req.MinMemoryMB > 0 {
		minMem = req.MinMemoryMB
	}
	if minMem <= 0 {
		minMem = s.config.MinMemoryMB
	}
	s.mu.RUnlock()

	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}

	// Block launch if the version is being deleted
	if s.sessions.IsVersionDeleting(inst.VersionID) {
		writeError(w, http.StatusConflict, "version is being deleted")
		return
	}

	// Pre-launch integrity check: verify all critical files exist
	integrity := minecraft.VerifyIntegrity(mcDir, inst.VersionID)
	if !integrity.OK {
		writeJSON(w, http.StatusConflict, map[string]any{
			"error":  integrity.FormatIssues(),
			"issues": integrity.Issues,
		})
		return
	}

	// Build effective config with instance overrides
	s.mu.RLock()
	effectiveConfig := *s.config
	s.mu.RUnlock()
	if inst.JavaPath != "" {
		effectiveConfig.JavaPathOverride = inst.JavaPath
	}
	if inst.WindowWidth > 0 && inst.WindowHeight > 0 {
		effectiveConfig.WindowWidth = inst.WindowWidth
		effectiveConfig.WindowHeight = inst.WindowHeight
	}
	if inst.JVMPreset != "" {
		effectiveConfig.JVMPreset = inst.JVMPreset
	}

	// Parse extra JVM args from instance
	var extraJVMArgs []string
	if inst.ExtraJVMArgs != "" {
		extraJVMArgs = strings.Fields(inst.ExtraJVMArgs)
	}
	effectiveMode, err := resolveInstancePerformanceMode(s.config, inst, "")
	if err != nil {
		writeError(w, http.StatusBadRequest, err.Error())
		return
	}

	result, err := launcher.BuildAndLaunch(launcher.LaunchOptions{
		VersionID:          inst.VersionID,
		InstanceID:         inst.ID,
		Username:           username,
		AuthMode:           launcher.LaunchAuthOffline,
		AdvancedOverrides:  inst.JavaPath != "" || len(extraJVMArgs) > 0,
		MaxMemoryMB:        maxMem,
		MinMemoryMB:        minMem,
		MCDir:              mcDir,
		GameDir:            instance.GameDir(inst.ID),
		ExtraJVMArgs:       extraJVMArgs,
		CompositionMode:    effectiveMode,
		PerformanceManager: s.performanceManager,
		Config:             &effectiveConfig,
	})
	if err != nil {
		var launchErr *launcher.LaunchError
		if errors.As(err, &launchErr) {
			writeJSON(w, http.StatusInternalServerError, map[string]any{
				"error":   launchErr.Error(),
				"healing": launchErr.Healing,
			})
			return
		}
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}

	s.sessions.Add(result)

	// Update instance last-played and store selection
	launchedAt := time.Now().UTC().Format(time.RFC3339)
	inst.LastPlayedAt = launchedAt
	s.instances.Update(*inst)
	s.instances.SetLastInstanceID(inst.ID)
	instance.Save(s.instances)

	s.mu.Lock()
	s.config.Username = username
	s.config.MaxMemoryMB = maxMem
	config.Save(s.config)
	s.mu.Unlock()

	writeJSON(w, http.StatusOK, map[string]any{
		"status":      "launching",
		"session_id":  result.SessionID,
		"instance_id": inst.ID,
		"pid":         result.Process.PID(),
		"launched_at": launchedAt,
		"healing":     result.Healing,
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
		"healing":    result.Healing,
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

// handleLaunchEvents streams real-time launch events via Server-Sent Events (SSE).
func (s *Server) handleLaunchEvents(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	result, ok := s.sessions.Get(id)
	if !ok {
		writeError(w, http.StatusNotFound, "session not found")
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
	w.Header().Set("Access-Control-Allow-Origin", "*")

	// Send initial state
	sendSSE(w, flusher, "status", map[string]any{
		"state":   string(result.Process.GetState()),
		"pid":     result.Process.PID(),
		"healing": result.Healing,
	})

	ctx := r.Context()

	for {
		select {
		case <-ctx.Done():
			return
		case line, ok := <-result.Process.LogChan:
			if !ok {
				// Channel closed, process exited
				sendSSE(w, flusher, "status", map[string]any{
					"state":          "exited",
					"exit_code":      result.Process.ExitCode,
					"failure_class":  result.Process.GetFailureClass(),
					"failure_detail": result.Process.GetFailureDetail(),
				})
				return
			}
			sendSSE(w, flusher, "log", map[string]any{
				"source": line.Source,
				"text":   line.Text,
			})
		}
	}
}
