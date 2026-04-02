package server

import (
	"encoding/json"
	"net/http"
	"sync"

	"github.com/mateoltd/croopor/internal/modloaders"
)

// LoaderInstallManager tracks active loader installations.
type LoaderInstallManager struct {
	mu       sync.RWMutex
	installs map[string]*modloaders.CombinedInstall
}

func NewLoaderInstallManager() *LoaderInstallManager {
	return &LoaderInstallManager{installs: make(map[string]*modloaders.CombinedInstall)}
}

func (lim *LoaderInstallManager) Add(id string, ci *modloaders.CombinedInstall) {
	lim.Lock()
	lim.installs[id] = ci
	lim.Unlock()
}

func (lim *LoaderInstallManager) Get(id string) (*modloaders.CombinedInstall, bool) {
	lim.RLock()
	defer lim.RUnlock()
	ci, ok := lim.installs[id]
	return ci, ok
}

func (lim *LoaderInstallManager) Remove(id string) {
	lim.mu.Lock()
	delete(lim.installs, id)
	lim.mu.Unlock()
}

func (lim *LoaderInstallManager) Lock()    { lim.mu.Lock() }
func (lim *LoaderInstallManager) Unlock()  { lim.mu.Unlock() }
func (lim *LoaderInstallManager) RLock()   { lim.mu.RLock() }
func (lim *LoaderInstallManager) RUnlock() { lim.mu.RUnlock() }

// handleLoaderGameVersions returns Minecraft versions supported by a given loader.
func (s *Server) handleLoaderGameVersions(w http.ResponseWriter, r *http.Request) {
	loaderType := modloaders.LoaderType(r.PathValue("type"))
	loader, ok := modloaders.Get(loaderType)
	if !ok {
		writeError(w, http.StatusNotFound, "unknown loader type: "+string(loaderType))
		return
	}

	versions, err := loader.GameVersions()
	if err != nil {
		// Graceful degradation: return empty list with error message
		writeJSON(w, http.StatusOK, map[string]any{
			"game_versions": []modloaders.GameVersion{},
			"error":         err.Error(),
		})
		return
	}

	writeJSON(w, http.StatusOK, map[string]any{"game_versions": versions})
}

// handleLoaderVersions returns available loader versions for a Minecraft version.
func (s *Server) handleLoaderVersions(w http.ResponseWriter, r *http.Request) {
	loaderType := modloaders.LoaderType(r.PathValue("type"))
	loader, ok := modloaders.Get(loaderType)
	if !ok {
		writeError(w, http.StatusNotFound, "unknown loader type: "+string(loaderType))
		return
	}

	mcVersion := r.URL.Query().Get("mc_version")
	if mcVersion == "" {
		writeError(w, http.StatusBadRequest, "mc_version query parameter is required")
		return
	}

	versions, err := loader.LoaderVersions(mcVersion)
	if err != nil {
		writeJSON(w, http.StatusOK, map[string]any{
			"loader_versions": []modloaders.LoaderVersion{},
			"error":           err.Error(),
		})
		return
	}

	writeJSON(w, http.StatusOK, map[string]any{"loader_versions": versions})
}

type loaderInstallRequest struct {
	LoaderType    string `json:"loader_type"`
	GameVersion   string `json:"game_version"`
	LoaderVersion string `json:"loader_version"`
}

// handleLoaderInstall starts a combined loader + base game installation.
func (s *Server) handleLoaderInstall(w http.ResponseWriter, r *http.Request) {
	var req loaderInstallRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON: "+err.Error())
		return
	}

	if req.LoaderType == "" || req.GameVersion == "" || req.LoaderVersion == "" {
		writeError(w, http.StatusBadRequest, "loader_type, game_version, and loader_version are required")
		return
	}

	loader, ok := modloaders.Get(modloaders.LoaderType(req.LoaderType))
	if !ok {
		writeError(w, http.StatusNotFound, "unknown loader type: "+req.LoaderType)
		return
	}

	mcDir := s.requireMCDir(w)
	if mcDir == "" {
		return
	}

	installID := randomID()
	ci := modloaders.NewCombinedInstall()
	s.loaderInstalls.Add(installID, ci)

	go ci.Run(loader, mcDir, req.GameVersion, req.LoaderVersion)

	writeJSON(w, http.StatusOK, map[string]string{"install_id": installID})
}

// handleLoaderInstallEvents streams SSE progress for a loader installation.
func (s *Server) handleLoaderInstallEvents(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	ci, ok := s.loaderInstalls.Get(id)
	if !ok {
		writeError(w, http.StatusNotFound, "loader install session not found")
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

	defer s.loaderInstalls.Remove(id)

	ctx := r.Context()
	for {
		select {
		case <-ctx.Done():
			return
		case p, ok := <-ci.ProgressCh:
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
