package server

import (
	"encoding/json"
	"fmt"
	"net/http"
)

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
		"state": string(result.Process.GetState()),
		"pid":   result.Process.PID(),
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
					"state":     "exited",
					"exit_code": result.Process.ExitCode,
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

func sendSSE(w http.ResponseWriter, flusher http.Flusher, eventType string, data any) {
	jsonData, err := json.Marshal(data)
	if err != nil {
		return
	}
	fmt.Fprintf(w, "event: %s\ndata: %s\n\n", eventType, string(jsonData))
	flusher.Flush()
}
