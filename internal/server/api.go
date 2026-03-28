package server

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/launcher"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/system"
)

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	mcDir := s.GetMCDir()
	writeJSON(w, http.StatusOK, map[string]any{
		"status":         "ok",
		"mc_dir":         mcDir,
		"setup_required": mcDir == "",
		"app_name":       "Croopor",
		"version":        "1.0.0",
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

// handleVersions returns ONLY locally installed versions.
func (s *Server) handleVersions(w http.ResponseWriter, r *http.Request) {
	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}
	versions, err := minecraft.ScanVersions(mcDir)
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to scan versions: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"versions": versions})
}

// handleCatalog returns the remote Mojang version catalog for browsing/installing.
func (s *Server) handleCatalog(w http.ResponseWriter, r *http.Request) {
	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}
	manifest, err := minecraft.FetchVersionManifest()
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to fetch catalog: "+err.Error())
		return
	}

	// Build a set of locally installed version IDs for marking
	local, _ := minecraft.ScanVersions(mcDir)
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

	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}

	// Pre-launch integrity check: verify all critical files exist
	integrity := minecraft.VerifyIntegrity(mcDir, req.VersionID)
	if !integrity.OK {
		writeJSON(w, http.StatusConflict, map[string]any{
			"error":  integrity.FormatIssues(),
			"issues": integrity.Issues,
		})
		return
	}

	result, err := launcher.BuildAndLaunch(launcher.LaunchOptions{
		VersionID:   req.VersionID,
		Username:    username,
		MaxMemoryMB: maxMem,
		MinMemoryMB: minMem,
		MCDir:       mcDir,
		Config:      s.config,
	})
	if err != nil {
		writeError(w, http.StatusInternalServerError, err.Error())
		return
	}

	s.sessions.Add(result)
	if s.config.LastLaunched == nil {
		s.config.LastLaunched = map[string]string{}
	}
	launchedAt := time.Now().UTC().Format(time.RFC3339)
	s.config.LastVersionID = req.VersionID
	s.config.Username = username
	s.config.MaxMemoryMB = maxMem
	s.config.LastLaunched[req.VersionID] = launchedAt
	config.Save(s.config)

	writeJSON(w, http.StatusOK, map[string]any{
		"status":      "launching",
		"session_id":  result.SessionID,
		"pid":         result.Process.PID(),
		"launched_at": launchedAt,
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

	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}

	// manifest_url is now optional — the downloader resolves it if empty
	installID := randomID()
	dl := minecraft.NewDownloader(mcDir)
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

// handleVersionInfo returns metadata about a version for the delete wizard.
func (s *Server) handleVersionInfo(w http.ResponseWriter, r *http.Request) {
	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}
	versionID := r.PathValue("id")
	if versionID == "" {
		writeError(w, http.StatusBadRequest, "version id is required")
		return
	}

	versionDir := filepath.Join(minecraft.VersionsDir(mcDir), versionID)
	if _, err := os.Stat(versionDir); os.IsNotExist(err) {
		writeError(w, http.StatusNotFound, "version not found")
		return
	}

	// Calculate folder size
	var folderSize int64
	filepath.Walk(versionDir, func(_ string, info os.FileInfo, err error) error {
		if err == nil && !info.IsDir() {
			folderSize += info.Size()
		}
		return nil
	})

	// Find dependent modded versions (inheritsFrom == versionID)
	allVersions, _ := minecraft.ScanVersions(mcDir)
	var dependents []string
	for _, v := range allVersions {
		if v.InheritsFrom == versionID {
			dependents = append(dependents, v.ID)
		}
	}

	// Scan worlds in saves/ directory
	type worldInfo struct {
		Name       string `json:"name"`
		Size       int64  `json:"size"`
		LastPlayed string `json:"last_played,omitempty"`
	}
	var worlds []worldInfo
	savesDir := filepath.Join(mcDir, "saves")
	if entries, err := os.ReadDir(savesDir); err == nil {
		for _, e := range entries {
			if !e.IsDir() {
				continue
			}
			worldDir := filepath.Join(savesDir, e.Name())
			var worldSize int64
			filepath.Walk(worldDir, func(_ string, info os.FileInfo, err error) error {
				if err == nil && !info.IsDir() {
					worldSize += info.Size()
				}
				return nil
			})
			info, _ := e.Info()
			var lastMod string
			if info != nil {
				lastMod = info.ModTime().UTC().Format(time.RFC3339)
			}
			worlds = append(worlds, worldInfo{
				Name:       e.Name(),
				Size:       worldSize,
				LastPlayed: lastMod,
			})
		}
	}

	// Count shared data directories
	type sharedDataInfo struct {
		Name  string `json:"name"`
		Count int    `json:"count"`
		Size  int64  `json:"size"`
	}
	var sharedData []sharedDataInfo
	sharedDirs := []string{"mods", "resourcepacks", "shaderpacks"}
	for _, dir := range sharedDirs {
		dirPath := filepath.Join(mcDir, dir)
		entries, err := os.ReadDir(dirPath)
		if err != nil || len(entries) == 0 {
			continue
		}
		var totalSize int64
		count := 0
		for _, e := range entries {
			if e.Name() == "." || e.Name() == ".." {
				continue
			}
			count++
			if info, err := e.Info(); err == nil {
				totalSize += info.Size()
			}
		}
		if count > 0 {
			sharedData = append(sharedData, sharedDataInfo{Name: dir, Count: count, Size: totalSize})
		}
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"id":          versionID,
		"folder_size": folderSize,
		"dependents":  dependents,
		"worlds":      worlds,
		"shared_data": sharedData,
	})
}

// handleDeleteVersion removes a version directory and optionally its dependents.
func (s *Server) handleDeleteVersion(w http.ResponseWriter, r *http.Request) {
	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}
	versionID := r.PathValue("id")
	if versionID == "" {
		writeError(w, http.StatusBadRequest, "version id is required")
		return
	}

	// Block deletion if the version is currently running
	s.sessions.mu.RLock()
	for _, sess := range s.sessions.sessions {
		if sess.VersionID == versionID && sess.Process.GetState() == launcher.StateRunning {
			s.sessions.mu.RUnlock()
			writeError(w, http.StatusConflict, "cannot delete a running version — stop the game first")
			return
		}
	}
	s.sessions.mu.RUnlock()

	var req struct {
		CascadeDependents bool `json:"cascade_dependents"`
	}
	json.NewDecoder(r.Body).Decode(&req)

	versionDir := filepath.Join(minecraft.VersionsDir(mcDir), versionID)
	if _, err := os.Stat(versionDir); os.IsNotExist(err) {
		writeError(w, http.StatusNotFound, "version not found")
		return
	}

	deleted := []string{}

	// If cascade, delete dependents first
	if req.CascadeDependents {
		allVersions, _ := minecraft.ScanVersions(mcDir)
		for _, v := range allVersions {
			if v.InheritsFrom == versionID {
				depDir := filepath.Join(minecraft.VersionsDir(mcDir), v.ID)
				if err := os.RemoveAll(depDir); err == nil {
					deleted = append(deleted, v.ID)
					log.Printf("Deleted dependent version: %s", v.ID)
				}
			}
		}
	}

	// Delete the version itself
	if err := os.RemoveAll(versionDir); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to delete version: "+err.Error())
		return
	}
	deleted = append(deleted, versionID)

	// Clean up last_launched reference
	if s.config.LastLaunched != nil {
		for _, id := range deleted {
			delete(s.config.LastLaunched, id)
		}
	}
	if s.config.LastVersionID == versionID {
		s.config.LastVersionID = ""
	}
	config.Save(s.config)

	log.Printf("Deleted version(s): %v", deleted)
	writeJSON(w, http.StatusOK, map[string]any{
		"status":  "ok",
		"deleted": deleted,
	})
}

// handleOpenVersionFolder opens the version folder in the system file manager.
func (s *Server) handleOpenVersionFolder(w http.ResponseWriter, r *http.Request) {
	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}
	versionID := r.PathValue("id")
	if versionID == "" {
		writeError(w, http.StatusBadRequest, "version id is required")
		return
	}

	versionDir := filepath.Join(minecraft.VersionsDir(mcDir), versionID)
	if _, err := os.Stat(versionDir); os.IsNotExist(err) {
		writeError(w, http.StatusNotFound, "version not found")
		return
	}

	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "windows":
		cmd = exec.Command("explorer", versionDir)
	case "darwin":
		cmd = exec.Command("open", versionDir)
	default:
		cmd = exec.Command("xdg-open", versionDir)
	}
	cmd.Start()

	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
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
	s.SetMCDir(req.Path)
	s.config.MCDir = req.Path
	if err := config.Save(s.config); err != nil {
		log.Printf("Warning: failed to save config: %v", err)
	}
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
	s.SetMCDir(req.Path)
	s.config.MCDir = req.Path
	if err := config.Save(s.config); err != nil {
		log.Printf("Warning: failed to save config: %v", err)
	}
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

// handleVersionWatch is an SSE endpoint that detects new versions added by
// third-party tools (Fabric/Forge/NeoForge installers) and pushes updates.
// Designed for low-end devices: uses a single Stat call on the versions/
// directory per tick instead of scanning every subdirectory.
func (s *Server) handleVersionWatch(w http.ResponseWriter, r *http.Request) {
	mcDir := s.GetMCDir()
	if mcDir == "" {
		writeError(w, http.StatusBadRequest, "minecraft directory not configured")
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
	lastMod := dirModTime(minecraft.VersionsDir(mcDir))
	lastCount := dirCount(minecraft.VersionsDir(mcDir))

	// 5s is plenty — mod loaders take seconds to install, users won't
	// notice a few seconds delay. Costs 1 Stat syscall per tick.
	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			dir := minecraft.VersionsDir(s.GetMCDir())
			mod := dirModTime(dir)
			count := dirCount(dir)
			if mod == lastMod && count == lastCount {
				continue
			}
			lastMod = mod
			lastCount = count
			versions, err := minecraft.ScanVersions(s.GetMCDir())
			if err != nil {
				continue
			}
			sendSSE(w, flusher, "versions_changed", map[string]any{"versions": versions})
		}
	}
}

// dirModTime returns the modification time of a directory (1 syscall).
// On most filesystems, this changes when entries are added or removed.
func dirModTime(path string) int64 {
	info, err := os.Stat(path)
	if err != nil {
		return 0
	}
	return info.ModTime().UnixNano()
}

// dirCount returns the number of entries in a directory.
// Used as a fallback for filesystems where dir mtime doesn't update reliably.
func dirCount(path string) int {
	entries, err := os.ReadDir(path)
	if err != nil {
		return -1
	}
	return len(entries)
}
