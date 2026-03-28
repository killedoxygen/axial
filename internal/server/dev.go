//go:build dev

package server

import (
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
)

const devMode = true

func registerDevRoutes(s *Server) {
	s.mux.HandleFunc("POST /api/v1/dev/cleanup-versions", s.handleDevCleanup)
	s.mux.HandleFunc("POST /api/v1/dev/flush", s.handleDevFlush)
}

// handleDevCleanup backs up worlds, resourcepacks, mods, then removes all versions.
func (s *Server) handleDevCleanup(w http.ResponseWriter, r *http.Request) {
	mcDir := s.mcDir

	// Create backup directory
	backupName := fmt.Sprintf("croopor-backup-%s", time.Now().Format("20060102-150405"))
	backupDir := filepath.Join(config.ConfigDir(), "backups", backupName)
	if err := os.MkdirAll(backupDir, 0755); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to create backup dir: "+err.Error())
		return
	}

	// Back up important user data
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

	// Remove versions directory contents
	versionsDir := minecraft.VersionsDir(mcDir)
	entries, _ := os.ReadDir(versionsDir)
	removed := 0
	for _, e := range entries {
		if e.IsDir() {
			os.RemoveAll(filepath.Join(versionsDir, e.Name()))
			removed++
		}
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"status":     "ok",
		"backup_dir": backupDir,
		"backed_up":  backed,
		"removed":    removed,
	})
}

// handleDevFlush deletes all Croopor config and cached runtimes, resetting to first-launch state.
func (s *Server) handleDevFlush(w http.ResponseWriter, r *http.Request) {
	configDir := config.ConfigDir()

	// Remove config file
	os.Remove(config.ConfigPath())

	// Remove cached runtimes
	runtimesDir := filepath.Join(configDir, "runtimes")
	os.RemoveAll(runtimesDir)

	// Reset in-memory config to defaults
	def := config.DefaultConfig()
	*s.config = *def

	writeJSON(w, http.StatusOK, map[string]string{"status": "flushed"})
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
