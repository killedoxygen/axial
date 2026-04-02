package server

import (
	"context"
	"encoding/json"
	"log"
	"net/http"
	"path/filepath"
	"strings"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/instance"
	"github.com/mateoltd/croopor/internal/performance"
	"github.com/mateoltd/croopor/internal/system"
)

func (s *Server) handlePerformancePlan(w http.ResponseWriter, r *http.Request) {
	gameVersion := strings.TrimSpace(r.URL.Query().Get("game_version"))
	if gameVersion == "" {
		writeError(w, http.StatusBadRequest, "game_version query parameter is required")
		return
	}
	if s.performanceManager == nil {
		writeJSON(w, http.StatusOK, map[string]any{"active": false})
		return
	}

	mode := resolveConfigPerformanceMode(s.config, r.URL.Query().Get("mode"))
	plan := s.performanceManager.GetPlan(composition.ResolutionRequest{
		GameVersion: gameVersion,
		Loader:      strings.TrimSpace(r.URL.Query().Get("loader")),
		Mode:        mode,
		Hardware:    system.Detect(),
	})
	writeJSON(w, http.StatusOK, plan)
}

func (s *Server) handlePerformanceHealth(w http.ResponseWriter, r *http.Request) {
	instanceID := strings.TrimSpace(r.URL.Query().Get("instance_id"))
	if instanceID == "" {
		writeError(w, http.StatusBadRequest, "instance_id query parameter is required")
		return
	}

	inst := s.instances.Get(instanceID)
	if inst == nil {
		writeError(w, http.StatusNotFound, "instance not found")
		return
	}

	modsDir := filepath.Join(instance.GameDir(inst.ID), "mods")
	state, err := performance.LoadState(modsDir)
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to load performance state: "+err.Error())
		return
	}

	effectiveMode := resolveInstancePerformanceMode(s.config, inst, "")
	var plan *composition.CompositionPlan
	if s.performanceManager != nil {
		plan = s.performanceManager.GetPlan(composition.ResolutionRequest{
			GameVersion:   extractServerBaseVersion(inst.VersionID),
			Loader:        inferLoaderFromVersionID(inst.VersionID),
			Mode:          effectiveMode,
			Hardware:      system.Detect(),
			InstalledMods: installedModIDsFromState(state),
		})
	}

	if effectiveMode != composition.ModeManaged {
		writeJSON(w, http.StatusOK, map[string]any{
			"active":          s.performanceManager != nil,
			"health":          performance.HealthDisabled,
			"composition_id":  "",
			"tier":            "",
			"installed_count": 0,
			"warnings":        []string(nil),
		})
		return
	}

	health, warnings := performance.DeriveHealth(state, plan, modsDir)
	response := map[string]any{
		"active":          s.performanceManager != nil,
		"health":          health,
		"composition_id":  "",
		"tier":            "",
		"installed_count": 0,
		"warnings":        warnings,
	}
	if state != nil {
		response["composition_id"] = state.CompositionID
		response["tier"] = state.Tier
		response["installed_count"] = len(state.InstalledMods)
	}
	writeJSON(w, http.StatusOK, response)
}

func (s *Server) handlePerformanceInstall(w http.ResponseWriter, r *http.Request) {
	if s.performanceManager == nil {
		writeJSON(w, http.StatusOK, map[string]any{"active": false, "status": "inactive"})
		return
	}

	var req struct {
		InstanceID  string `json:"instance_id"`
		GameVersion string `json:"game_version"`
		Loader      string `json:"loader"`
		Mode        string `json:"mode"`
	}
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
	if req.GameVersion == "" {
		req.GameVersion = extractServerBaseVersion(inst.VersionID)
	}
	if req.Loader == "" {
		req.Loader = inferLoaderFromVersionID(inst.VersionID)
	}
	mode := resolveInstancePerformanceMode(s.config, inst, req.Mode)

	plan := s.performanceManager.GetPlan(composition.ResolutionRequest{
		GameVersion:   req.GameVersion,
		Loader:        req.Loader,
		Mode:          mode,
		Hardware:      system.Detect(),
		InstalledMods: nil,
	})

	modsDir := filepath.Join(instance.GameDir(inst.ID), "mods")
	go func() {
		if mode != composition.ModeManaged {
			if err := s.performanceManager.RemoveManaged(modsDir); err != nil {
				log.Printf("performance cleanup failed for instance %s: %v", inst.ID, err)
			}
			return
		}
		if _, err := s.performanceManager.EnsureInstalled(context.Background(), plan, req.GameVersion, modsDir); err != nil {
			log.Printf("performance install failed for instance %s: %v", inst.ID, err)
		}
	}()

	writeJSON(w, http.StatusAccepted, map[string]any{"status": "installing"})
}

func parseCompositionMode(raw string) composition.CompositionMode {
	switch strings.ToLower(strings.TrimSpace(raw)) {
	case "", string(composition.ModeManaged):
		return composition.ModeManaged
	case string(composition.ModeVanilla):
		return composition.ModeVanilla
	case string(composition.ModeCustom):
		return composition.ModeCustom
	default:
		return composition.ModeManaged
	}
}

func normalizeConfigPerformanceMode(raw string) string {
	return string(resolveConfigPerformanceMode(nil, raw))
}

func normalizeInstancePerformanceMode(raw string) string {
	raw = strings.ToLower(strings.TrimSpace(raw))
	switch raw {
	case "":
		return ""
	case string(composition.ModeManaged), string(composition.ModeVanilla), string(composition.ModeCustom):
		return raw
	default:
		return ""
	}
}

func resolveConfigPerformanceMode(cfg *config.Config, raw string) composition.CompositionMode {
	mode := strings.ToLower(strings.TrimSpace(raw))
	if mode != "" {
		return parseCompositionMode(mode)
	}
	if cfg != nil {
		return parseCompositionMode(cfg.PerformanceMode)
	}
	return composition.ModeManaged
}

func resolveInstancePerformanceMode(cfg *config.Config, inst *instance.Instance, raw string) composition.CompositionMode {
	mode := strings.ToLower(strings.TrimSpace(raw))
	if mode != "" {
		return parseCompositionMode(mode)
	}
	if inst != nil && inst.PerformanceMode != "" {
		return parseCompositionMode(inst.PerformanceMode)
	}
	return resolveConfigPerformanceMode(cfg, "")
}

func inferLoaderFromVersionID(versionID string) string {
	v := strings.ToLower(versionID)
	switch {
	case strings.Contains(v, "neoforge"):
		return "neoforge"
	case strings.Contains(v, "fabric"):
		return "fabric"
	case strings.Contains(v, "forge"):
		return "forge"
	case strings.Contains(v, "quilt"):
		return "quilt"
	default:
		return "vanilla"
	}
}

func extractServerBaseVersion(versionID string) string {
	parts := strings.Split(versionID, "-")
	if len(parts) == 0 {
		return versionID
	}
	base := strings.TrimSpace(parts[0])
	if strings.Count(base, ".") >= 1 {
		return base
	}
	return versionID
}

func installedModIDsFromState(state *performance.CompositionState) []string {
	if state == nil {
		return nil
	}
	out := make([]string, 0, len(state.InstalledMods))
	for _, mod := range state.InstalledMods {
		out = append(out, mod.ProjectID)
	}
	return out
}
