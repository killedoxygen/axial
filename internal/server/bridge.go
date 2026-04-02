package server

import "fmt"

func (s *Server) BridgeInstallEvents(id string, emit func(eventType string, data any)) error {
	dl, ok := s.installs.Get(id)
	if !ok {
		return fmt.Errorf("install session not found")
	}

	go func() {
		defer s.installs.Remove(id)
		for progress := range dl.ProgressCh {
			emit("progress", progress)
			if progress.Done {
				return
			}
		}
	}()

	return nil
}

func (s *Server) BridgeLoaderInstallEvents(id string, emit func(eventType string, data any)) error {
	ci, ok := s.loaderInstalls.Get(id)
	if !ok {
		return fmt.Errorf("loader install session not found")
	}

	go func() {
		for progress := range ci.ProgressCh {
			emit("progress", progress)
			if progress.Done {
				return
			}
		}
	}()

	return nil
}

func (s *Server) BridgeLaunchEvents(id string, emit func(eventType string, data any)) error {
	result, ok := s.sessions.Get(id)
	if !ok {
		return fmt.Errorf("session not found")
	}

	go func() {
		emit("status", map[string]any{
			"state": string(result.Process.GetState()),
			"pid":   result.Process.PID(),
		})

		for line := range result.Process.LogChan {
			emit("log", map[string]any{
				"source": line.Source,
				"text":   line.Text,
			})
		}

		emit("status", map[string]any{
			"state":     "exited",
			"exit_code": result.Process.ExitCode,
		})
	}()

	return nil
}
