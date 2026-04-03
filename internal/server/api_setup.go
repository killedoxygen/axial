package server

import (
	"encoding/json"
	"log"
	"math"
	"net/http"
	"os/exec"
	"runtime"
	"strings"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/minecraft"
)

func (s *Server) handleGetConfig(w http.ResponseWriter, r *http.Request) {
	s.mu.RLock()
	cfg := *s.config
	s.mu.RUnlock()
	writeJSON(w, http.StatusOK, &cfg)
}

func (s *Server) handleUpdateConfig(w http.ResponseWriter, r *http.Request) {
	var updates map[string]any
	if err := json.NewDecoder(r.Body).Decode(&updates); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	if v, ok := updates["username"].(string); ok && v != "" {
		s.config.Username = v
	}
	if v, ok := updates["max_memory_mb"].(float64); ok && v > 0 {
		s.config.MaxMemoryMB = int(v)
	}
	if v, ok := updates["min_memory_mb"].(float64); ok && v > 0 {
		s.config.MinMemoryMB = int(v)
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
	if v, ok := updates["jvm_preset"].(string); ok {
		s.config.JVMPreset = v
	}
	if v, ok := updates["performance_mode"].(string); ok {
		normalized := normalizeConfigPerformanceMode(v)
		if strings.TrimSpace(v) != "" && normalized == "" {
			writeError(w, http.StatusBadRequest, "invalid performance_mode: "+v)
			return
		}
		s.config.PerformanceMode = normalized
	}
	if v, ok := updates["theme"].(string); ok {
		s.config.Theme = v
	}
	if v, ok := updates["custom_hue"].(float64); ok {
		i := int(v)
		s.config.CustomHue = &i
	}
	if v, ok := updates["custom_vibrancy"].(float64); ok {
		i := int(v)
		s.config.CustomVibrancy = &i
	}
	if v, ok := updates["lightness"].(float64); ok {
		i := int(v)
		s.config.Lightness = &i
	}
	if v, ok := updates["music_enabled"].(bool); ok {
		s.config.MusicEnabled = &v
	}
	if v, ok := updates["music_volume"].(float64); ok {
		i := int(v)
		s.config.MusicVolume = &i
	}
	if v, ok := updates["music_track"].(float64); ok {
		if len(musicTracks) > 0 && v == math.Trunc(v) {
			idx := int(v)
			if idx < 0 {
				idx = 0
			}
			if idx >= len(musicTracks) {
				idx = len(musicTracks) - 1
			}
			s.config.MusicTrack = idx
		}
	}

	if err := config.Save(s.config); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to save config: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, s.config)
}

func (s *Server) handleOnboardingComplete(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.config.OnboardingDone = true
	if err := config.Save(s.config); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to save: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

// ── Setup handlers ──

func (s *Server) handleSetupDefaults(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]string{
		"default_path": minecraft.DefaultMinecraftDir(),
		"os":           runtime.GOOS,
	})
}

func (s *Server) handleSetupValidate(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Path string `json:"path"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON")
		return
	}
	if req.Path == "" {
		writeJSON(w, http.StatusOK, map[string]any{"valid": false, "error": "path is empty"})
		return
	}
	if err := minecraft.ValidateInstallation(req.Path); err != nil {
		writeJSON(w, http.StatusOK, map[string]any{"valid": false, "error": err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"valid": true})
}

func (s *Server) handleSetupSetDir(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Path string `json:"path"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON")
		return
	}
	if err := minecraft.ValidateInstallation(req.Path); err != nil {
		writeError(w, http.StatusBadRequest, "invalid minecraft installation: "+err.Error())
		return
	}
	s.mu.Lock()
	s.mcDir = req.Path
	s.config.MCDir = req.Path
	if err := config.Save(s.config); err != nil {
		s.config.MCDir = ""
		s.mcDir = ""
		s.mu.Unlock()
		writeError(w, http.StatusInternalServerError, "failed to save config: "+err.Error())
		return
	}
	s.mu.Unlock()
	// Ensure launcher_profiles.json exists for mod loader compatibility
	minecraft.EnsureLauncherProfiles(req.Path, "")
	log.Printf("Minecraft directory set to: %s", req.Path)
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok", "mc_dir": req.Path})
}

func (s *Server) handleSetupInit(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Path string `json:"path"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON")
		return
	}
	if req.Path == "" {
		req.Path = minecraft.DefaultMinecraftDir()
	}
	if req.Path == "" {
		writeError(w, http.StatusInternalServerError, "could not determine default minecraft path")
		return
	}
	if err := minecraft.CreateMinecraftDir(req.Path); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to create directory: "+err.Error())
		return
	}
	// Create launcher_profiles.json for mod loader compatibility
	minecraft.EnsureLauncherProfiles(req.Path, "")
	s.mu.Lock()
	s.mcDir = req.Path
	s.config.MCDir = req.Path
	if err := config.Save(s.config); err != nil {
		s.config.MCDir = ""
		s.mcDir = ""
		s.mu.Unlock()
		writeError(w, http.StatusInternalServerError, "failed to save config: "+err.Error())
		return
	}
	s.mu.Unlock()
	log.Printf("Created new Minecraft directory at: %s", req.Path)
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok", "mc_dir": req.Path})
}

func (s *Server) handleSetupBrowse(w http.ResponseWriter, r *http.Request) {
	if runtime.GOOS != "windows" {
		writeJSON(w, http.StatusOK, map[string]string{"path": ""})
		return
	}
	cmd := exec.Command("powershell", "-NoProfile", "-Command",
		`Add-Type -AssemblyName System.Windows.Forms; $f = New-Object System.Windows.Forms.FolderBrowserDialog; $f.Description = 'Select your .minecraft folder'; if ($f.ShowDialog() -eq 'OK') { $f.SelectedPath }`)
	out, err := cmd.Output()
	if err != nil {
		writeJSON(w, http.StatusOK, map[string]string{"path": ""})
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"path": strings.TrimSpace(string(out))})
}
