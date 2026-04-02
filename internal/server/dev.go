//go:build dev

package server

import (
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/instance"
	"github.com/mateoltd/croopor/internal/launcher"
	"github.com/mateoltd/croopor/internal/minecraft"
)

const devMode = true

func registerDevRoutes(s *Server) {
	s.mux.HandleFunc("POST /api/v1/dev/cleanup-versions", s.handleDevCleanup)
	s.mux.HandleFunc("POST /api/v1/dev/flush", s.handleDevFlush)
	s.mux.HandleFunc("GET /api/v1/dev/boot-profiles", s.handleDevListProfiles)
	s.mux.HandleFunc("GET /api/v1/dev/boot-profiles/{name}", s.handleDevGetProfile)
	s.mux.HandleFunc("GET /api/v1/dev/boot-profile-live/{id}", s.handleDevLiveProfile)
}

// handleDevCleanup backs up instance data + shared MC data, then removes all versions and instances.
func (s *Server) handleDevCleanup(w http.ResponseWriter, r *http.Request) {
	mcDir := s.GetMCDir()
	if mcDir == "" {
		writeError(w, http.StatusPreconditionFailed, "minecraft directory not configured")
		return
	}

	// Create backup directory
	backupName := fmt.Sprintf("croopor-backup-%s", time.Now().Format("20060102-150405"))
	backupDir := filepath.Join(config.ConfigDir(), "backups", backupName)
	if err := os.MkdirAll(backupDir, 0755); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to create backup dir: "+err.Error())
		return
	}

	// Back up shared Minecraft data
	preserve := []string{"saves", "resourcepacks", "mods", "shaderpacks", "config", "options.txt", "servers.dat"}
	backed := []string{}
	for _, name := range preserve {
		src := filepath.Join(mcDir, name)
		if _, err := os.Stat(src); os.IsNotExist(err) {
			continue
		}
		dst := filepath.Join(backupDir, name)
		if err := copyPath(src, dst); err != nil {
			continue
		}
		backed = append(backed, name)
	}

	// Back up instance data (saves, mods, etc. per instance)
	allInstances := s.instances.List()
	instancesRemoved := 0
	instanceBackupDir := filepath.Join(backupDir, "instances")
	for _, inst := range allInstances {
		gameDir := instance.GameDir(inst.ID)
		if _, err := os.Stat(gameDir); os.IsNotExist(err) {
			continue
		}
		dst := filepath.Join(instanceBackupDir, inst.Name+" ("+inst.ID[:8]+")")
		copyPath(gameDir, dst)
	}

	// Remove all instance game directories and clear instance store
	os.RemoveAll(instance.InstancesBaseDir())
	instancesRemoved = s.instances.Len()
	s.instances.Clear()
	instance.Save(s.instances)

	// Remove versions directory contents
	versionsDir := minecraft.VersionsDir(mcDir)
	entries, _ := os.ReadDir(versionsDir)
	versionsRemoved := 0
	for _, e := range entries {
		if e.IsDir() {
			os.RemoveAll(filepath.Join(versionsDir, e.Name()))
			versionsRemoved++
		}
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"status":            "ok",
		"backup_dir":        backupDir,
		"backed_up":         backed,
		"versions_removed":  versionsRemoved,
		"instances_removed": instancesRemoved,
	})
}

// handleDevFlush deletes all Croopor config, instances, and cached runtimes, resetting to first-launch state.
func (s *Server) handleDevFlush(w http.ResponseWriter, r *http.Request) {
	configDir := config.ConfigDir()

	// Remove config file
	os.Remove(config.ConfigPath())

	// Remove cached runtimes
	os.RemoveAll(filepath.Join(configDir, "runtimes"))

	// Remove all instances (store + game directories)
	os.RemoveAll(instance.InstancesBaseDir())
	os.Remove(filepath.Join(configDir, "instances.json"))
	s.instances.Clear()

	// Reset in-memory config to defaults
	def := config.DefaultConfig()
	*s.config = *def

	writeJSON(w, http.StatusOK, map[string]string{"status": "flushed"})
}

// handleDevListProfiles lists all saved boot profile reports.
func (s *Server) handleDevListProfiles(w http.ResponseWriter, r *http.Request) {
	dir := filepath.Join(config.ConfigDir(), "boot-profiles")
	entries, err := os.ReadDir(dir)
	if err != nil {
		writeJSON(w, http.StatusOK, []string{})
		return
	}

	profiles := []map[string]string{}
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".json") {
			continue
		}
		info, _ := e.Info()
		profiles = append(profiles, map[string]string{
			"name": e.Name(),
			"size": fmt.Sprintf("%d", info.Size()),
		})
	}
	writeJSON(w, http.StatusOK, profiles)
}

// handleDevGetProfile returns a single boot profile by filename.
func (s *Server) handleDevGetProfile(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("name")
	if strings.Contains(name, "..") || strings.Contains(name, "/") || strings.Contains(name, "\\") {
		writeError(w, http.StatusBadRequest, "invalid name")
		return
	}

	path := filepath.Join(config.ConfigDir(), "boot-profiles", name)
	data, err := os.ReadFile(path)
	if err != nil {
		writeError(w, http.StatusNotFound, "profile not found")
		return
	}

	w.Header().Set("Content-Type", "application/json")
	w.Write(data)
}

// handleDevLiveProfile streams real-time profiler samples via SSE for a running session.
func (s *Server) handleDevLiveProfile(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	result, ok := s.sessions.Get(id)
	if !ok {
		writeError(w, http.StatusNotFound, "session not found")
		return
	}
	if result.Process.Profile == nil {
		writeError(w, http.StatusNotFound, "no profiler attached")
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
	lastSent := 0
	ticker := time.NewTicker(250 * time.Millisecond)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			samples := result.Process.Profile.GetSamples()
			for i := lastSent; i < len(samples); i++ {
				sendSSE(w, flusher, "sample", samples[i])
			}
			lastSent = len(samples)

			// Send boot complete event and stop
			if result.Process.BootCompleted() {
				sendSSE(w, flusher, "boot_complete", map[string]any{
					"duration_ms":   result.Process.BootDuration().Milliseconds(),
					"peak_threads":  result.Process.Profile.PeakThreads,
					"peak_mem_mb":   result.Process.Profile.PeakMemMB,
					"peak_cpu_pct":  result.Process.Profile.PeakCPUPct,
					"total_samples": len(samples),
				})
				return
			}

			// Also stop if process exited
			if result.Process.GetState() == launcher.StateExited || result.Process.GetState() == launcher.StateFailed {
				sendSSE(w, flusher, "process_exited", map[string]any{
					"state":     string(result.Process.GetState()),
					"exit_code": result.Process.ExitCode,
				})
				return
			}
		}
	}
}

func copyPath(src, dst string) error {
	info, err := os.Stat(src)
	if err != nil {
		return err
	}
	if info.IsDir() {
		return copyDir(src, dst)
	}
	return copyFile(src, dst)
}

func copyDir(src, dst string) error {
	if err := os.MkdirAll(dst, 0755); err != nil {
		return err
	}
	entries, err := os.ReadDir(src)
	if err != nil {
		return err
	}
	for _, e := range entries {
		s := filepath.Join(src, e.Name())
		d := filepath.Join(dst, e.Name())
		if e.IsDir() {
			if err := copyDir(s, d); err != nil {
				return err
			}
		} else {
			if err := copyFile(s, d); err != nil {
				return err
			}
		}
	}
	return nil
}

func copyFile(src, dst string) error {
	// Skip very large files (>100MB) to keep backups manageable
	info, err := os.Stat(src)
	if err != nil {
		return err
	}
	if info.Size() > 100*1024*1024 {
		return nil
	}
	// Skip .jar files in mods to save space
	if strings.HasSuffix(src, ".jar") && strings.Contains(src, "mods") {
		return nil
	}
	data, err := os.ReadFile(src)
	if err != nil {
		return err
	}
	os.MkdirAll(filepath.Dir(dst), 0755)
	return os.WriteFile(dst, data, info.Mode())
}
