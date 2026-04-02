package server

import (
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"

	"github.com/mateoltd/croopor/internal/instance"
	"github.com/mateoltd/croopor/internal/launcher"
	"github.com/mateoltd/croopor/internal/minecraft"
)

// enrichedInstance adds version metadata and folder stats to an instance for the frontend.
type enrichedInstance struct {
	instance.Instance
	VersionType   string `json:"version_type,omitempty"`
	Launchable    bool   `json:"launchable"`
	StatusDetail  string `json:"status_detail,omitempty"`
	NeedsInstall  string `json:"needs_install,omitempty"`
	JavaMajor     int    `json:"java_major,omitempty"`
	SavesCount    int    `json:"saves_count"`
	ModsCount     int    `json:"mods_count"`
	ResourceCount int    `json:"resource_count"`
	ShaderCount   int    `json:"shader_count"`
}

func (s *Server) handleListInstances(w http.ResponseWriter, r *http.Request) {
	mcDir := s.GetMCDir()

	// Build version lookup for enrichment
	var versions []minecraft.VersionEntry
	if mcDir != "" {
		versions, _ = minecraft.ScanVersions(mcDir)
	}
	versionMap := make(map[string]minecraft.VersionEntry, len(versions))
	for _, v := range versions {
		versionMap[v.ID] = v
	}

	allInstances := s.instances.List()
	enriched := make([]enrichedInstance, 0, len(allInstances))
	for _, inst := range allInstances {
		ei := enrichedInstance{Instance: inst}

		if v, ok := versionMap[inst.VersionID]; ok {
			ei.VersionType = v.Type
			ei.Launchable = v.Launchable
			ei.StatusDetail = v.StatusDetail
			ei.NeedsInstall = v.NeedsInstall
			ei.JavaMajor = v.JavaMajor
		} else {
			ei.StatusDetail = "version not installed"
		}

		// Folder stats (lightweight dir reads)
		gameDir := instance.GameDir(inst.ID)
		ei.SavesCount = countEntries(filepath.Join(gameDir, "saves"))
		ei.ModsCount = countEntries(filepath.Join(gameDir, "mods"))
		ei.ResourceCount = countEntries(filepath.Join(gameDir, "resourcepacks"))
		ei.ShaderCount = countEntries(filepath.Join(gameDir, "shaderpacks"))

		enriched = append(enriched, ei)
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"instances":        enriched,
		"last_instance_id": s.instances.GetLastInstanceID(),
	})
}

func (s *Server) handleCreateInstance(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Name      string `json:"name"`
		VersionID string `json:"version_id"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	mcDir := s.GetMCDir()
	inst, err := s.instances.Add(req.Name, req.VersionID, mcDir)
	if err != nil {
		writeError(w, http.StatusBadRequest, err.Error())
		return
	}

	writeJSON(w, http.StatusOK, inst)
}

func (s *Server) handleGetInstance(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	inst := s.instances.Get(id)
	if inst == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}
	writeJSON(w, http.StatusOK, inst)
}

func (s *Server) handleUpdateInstance(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	inst := s.instances.Get(id)
	if inst == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}

	var updates map[string]any
	if err := json.NewDecoder(r.Body).Decode(&updates); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	if v, ok := updates["name"].(string); ok && v != "" {
		if v != inst.Name && s.instances.NameExists(v, inst.ID) {
			writeError(w, http.StatusConflict, "an instance with this name already exists")
			return
		}
		inst.Name = v
	}
	if v, ok := updates["version_id"].(string); ok && v != "" {
		inst.VersionID = v
	}
	if v, ok := updates["max_memory_mb"].(float64); ok {
		inst.MaxMemoryMB = int(v)
	}
	if v, ok := updates["min_memory_mb"].(float64); ok {
		inst.MinMemoryMB = int(v)
	}
	if v, ok := updates["java_path"].(string); ok {
		inst.JavaPath = v
	}
	if v, ok := updates["window_width"].(float64); ok {
		inst.WindowWidth = int(v)
	}
	if v, ok := updates["window_height"].(float64); ok {
		inst.WindowHeight = int(v)
	}
	if v, ok := updates["jvm_preset"].(string); ok {
		inst.JVMPreset = v
	}
	if v, ok := updates["extra_jvm_args"].(string); ok {
		inst.ExtraJVMArgs = v
	}

	if err := s.instances.Update(*inst); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to save: "+err.Error())
		return
	}
	writeJSON(w, http.StatusOK, inst)
}

func (s *Server) handleDeleteInstance(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	inst := s.instances.Get(id)
	if inst == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}

	// Block deletion if instance is currently running
	s.sessions.mu.RLock()
	for _, sess := range s.sessions.sessions {
		if sess.InstanceID == id && sess.Process.GetState() == launcher.StateRunning {
			s.sessions.mu.RUnlock()
			writeError(w, http.StatusConflict, "cannot delete a running instance — stop the game first")
			return
		}
	}
	s.sessions.mu.RUnlock()

	keepFiles := r.URL.Query().Get("keep_files") == "true"
	if err := s.instances.Remove(id, !keepFiles); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to delete: "+err.Error())
		return
	}

	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

func (s *Server) handleDuplicateInstance(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")

	var req struct {
		Name string `json:"name"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil && !errors.Is(err, io.EOF) {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	src := s.instances.Get(id)
	if src == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}

	if req.Name == "" {
		req.Name = src.Name + " (copy)"
	}

	copyFiles := r.URL.Query().Get("copy_files") == "true"
	mcDir := s.GetMCDir()
	inst, err := s.instances.Duplicate(id, req.Name, mcDir, copyFiles)
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to duplicate: "+err.Error())
		return
	}

	writeJSON(w, http.StatusOK, inst)
}

func (s *Server) handleOpenInstanceFolder(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	inst := s.instances.Get(id)
	if inst == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}

	dir := instance.GameDir(id)

	// Optionally open a subfolder
	sub := r.URL.Query().Get("sub")
	if sub == "mods" || sub == "saves" || sub == "resourcepacks" || sub == "shaderpacks" || sub == "config" {
		dir = filepath.Join(dir, sub)
	}

	if _, err := os.Stat(dir); os.IsNotExist(err) {
		if err := os.MkdirAll(dir, 0755); err != nil {
			writeError(w, http.StatusInternalServerError, "failed to create folder: "+err.Error())
			return
		}
	}

	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "windows":
		cmd = exec.Command("explorer", dir)
	case "darwin":
		cmd = exec.Command("open", dir)
	default:
		cmd = exec.Command("xdg-open", dir)
	}
	if err := cmd.Start(); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to open folder: "+err.Error())
		return
	}
	go func() {
		if err := cmd.Wait(); err != nil {
			log.Printf("open instance folder command failed: %v", err)
		}
	}()

	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

// countEntries returns the number of entries in a directory, or 0 if it doesn't exist.
func countEntries(dir string) int {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return 0
	}
	return len(entries)
}
