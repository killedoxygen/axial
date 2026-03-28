package server

import (
	"encoding/json"
	"io/fs"
	"net/http"
	"sync"

	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/launcher"
	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
)

type Server struct {
	mcDir    string
	config   *config.Config
	sessions *SessionManager
	installs *InstallManager
	mux      *http.ServeMux
	frontend fs.FS
}

func NewServer(mcDir string, cfg *config.Config, frontend fs.FS) *Server {
	s := &Server{
		mcDir:    mcDir,
		config:   cfg,
		sessions: NewSessionManager(),
		installs: NewInstallManager(),
		mux:      http.NewServeMux(),
		frontend: frontend,
	}
	s.registerRoutes()
	return s
}

func (s *Server) registerRoutes() {
	s.mux.HandleFunc("GET /api/v1/status", s.handleStatus)
	s.mux.HandleFunc("GET /api/v1/system", s.handleSystem)
	s.mux.HandleFunc("GET /api/v1/versions", s.handleVersions)
	s.mux.HandleFunc("GET /api/v1/config", s.handleGetConfig)
	s.mux.HandleFunc("PUT /api/v1/config", s.handleUpdateConfig)
	s.mux.HandleFunc("POST /api/v1/onboarding/complete", s.handleOnboardingComplete)
	s.mux.HandleFunc("POST /api/v1/launch", s.handleLaunch)
	s.mux.HandleFunc("GET /api/v1/launch/{id}/events", s.handleLaunchEvents)
	s.mux.HandleFunc("GET /api/v1/launch/{id}/command", s.handleLaunchCommand)
	s.mux.HandleFunc("POST /api/v1/launch/{id}/kill", s.handleKillProcess)
	s.mux.HandleFunc("POST /api/v1/install", s.handleInstall)
	s.mux.HandleFunc("GET /api/v1/install/{id}/events", s.handleInstallEvents)
	s.mux.HandleFunc("GET /api/v1/java", s.handleJava)
	s.mux.Handle("/", http.FileServer(http.FS(s.frontend)))
}

func (s *Server) Handler() http.Handler {
	return s.mux
}

// SessionManager tracks active launch sessions.
type SessionManager struct {
	mu       sync.RWMutex
	sessions map[string]*launcher.LaunchResult
}

func NewSessionManager() *SessionManager {
	return &SessionManager{sessions: make(map[string]*launcher.LaunchResult)}
}

func (sm *SessionManager) Add(result *launcher.LaunchResult) {
	sm.mu.Lock()
	sm.sessions[result.SessionID] = result
	sm.mu.Unlock()
}

func (sm *SessionManager) Get(id string) (*launcher.LaunchResult, bool) {
	sm.mu.RLock()
	defer sm.mu.RUnlock()
	r, ok := sm.sessions[id]
	return r, ok
}

// InstallManager tracks active version installations.
type InstallManager struct {
	mu       sync.RWMutex
	installs map[string]*minecraft.Downloader
}

func NewInstallManager() *InstallManager {
	return &InstallManager{installs: make(map[string]*minecraft.Downloader)}
}

func (im *InstallManager) Add(id string, d *minecraft.Downloader) {
	im.mu.Lock()
	im.installs[id] = d
	im.mu.Unlock()
}

func (im *InstallManager) Get(id string) (*minecraft.Downloader, bool) {
	im.mu.RLock()
	defer im.mu.RUnlock()
	d, ok := im.installs[id]
	return d, ok
}

// JSON helpers

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

func writeError(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}
