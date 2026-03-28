package server

import (
	"encoding/json"
	"io/fs"
	"net/http"

	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/launcher"
)

// Server is the HTTP server for the paralauncher.
type Server struct {
	mcDir    string
	config   *config.Config
	sessions *SessionManager
	mux      *http.ServeMux
	frontend fs.FS
}

// NewServer creates a new paralauncher HTTP server.
func NewServer(mcDir string, cfg *config.Config, frontend fs.FS) *Server {
	s := &Server{
		mcDir:    mcDir,
		config:   cfg,
		sessions: NewSessionManager(),
		mux:      http.NewServeMux(),
		frontend: frontend,
	}
	s.registerRoutes()
	return s
}

func (s *Server) registerRoutes() {
	// API routes
	s.mux.HandleFunc("GET /api/v1/status", s.handleStatus)
	s.mux.HandleFunc("GET /api/v1/versions", s.handleVersions)
	s.mux.HandleFunc("GET /api/v1/config", s.handleGetConfig)
	s.mux.HandleFunc("PUT /api/v1/config", s.handleUpdateConfig)
	s.mux.HandleFunc("POST /api/v1/launch", s.handleLaunch)
	s.mux.HandleFunc("GET /api/v1/launch/{id}/events", s.handleLaunchEvents)
	s.mux.HandleFunc("GET /api/v1/launch/{id}/command", s.handleLaunchCommand)
	s.mux.HandleFunc("POST /api/v1/launch/{id}/kill", s.handleKillProcess)
	s.mux.HandleFunc("GET /api/v1/java", s.handleJava)

	// Frontend static files
	s.mux.Handle("/", http.FileServer(http.FS(s.frontend)))
}

// Handler returns the HTTP handler for use with a custom listener.
func (s *Server) Handler() http.Handler {
	return s.mux
}

// SessionManager tracks active launch sessions.
type SessionManager struct {
	sessions map[string]*launcher.LaunchResult
}

func NewSessionManager() *SessionManager {
	return &SessionManager{
		sessions: make(map[string]*launcher.LaunchResult),
	}
}

func (sm *SessionManager) Add(result *launcher.LaunchResult) {
	sm.sessions[result.SessionID] = result
}

func (sm *SessionManager) Get(id string) (*launcher.LaunchResult, bool) {
	r, ok := sm.sessions[id]
	return r, ok
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
