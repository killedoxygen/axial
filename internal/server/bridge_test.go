package server

import (
	"sync"
	"testing"
	"time"

	"github.com/mateoltd/croopor/internal/launcher"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/modloaders"
)

func TestBridgeInstallEvents(t *testing.T) {
	srv := &Server{installs: NewInstallManager()}
	dl := &minecraft.Downloader{ProgressCh: make(chan minecraft.DownloadProgress, 2)}
	srv.installs.Add("install-1", dl)

	var mu sync.Mutex
	var events []minecraft.DownloadProgress

	if err := srv.BridgeInstallEvents("install-1", func(eventType string, data any) {
		if eventType != "progress" {
			t.Fatalf("unexpected event type: %s", eventType)
		}
		progress, ok := data.(minecraft.DownloadProgress)
		if !ok {
			t.Fatalf("unexpected payload type: %T", data)
		}
		mu.Lock()
		events = append(events, progress)
		mu.Unlock()
	}); err != nil {
		t.Fatalf("BridgeInstallEvents returned error: %v", err)
	}

	dl.ProgressCh <- minecraft.DownloadProgress{Phase: "version_json", Current: 1, Total: 1}
	dl.ProgressCh <- minecraft.DownloadProgress{Phase: "done", Current: 1, Total: 1, Done: true}
	close(dl.ProgressCh)

	waitFor(func() bool {
		mu.Lock()
		defer mu.Unlock()
		return len(events) == 2
	})

	if got := events[0].Phase; got != "version_json" {
		t.Fatalf("first phase = %q, want version_json", got)
	}
	if !events[1].Done {
		t.Fatalf("second event should be marked done")
	}
}

func TestBridgeLoaderInstallEvents(t *testing.T) {
	srv := &Server{loaderInstalls: NewLoaderInstallManager()}
	ci := &modloaders.CombinedInstall{ProgressCh: make(chan minecraft.DownloadProgress, 1)}
	srv.loaderInstalls.Add("loader-1", ci)

	done := make(chan minecraft.DownloadProgress, 1)
	if err := srv.BridgeLoaderInstallEvents("loader-1", func(eventType string, data any) {
		if eventType != "progress" {
			t.Fatalf("unexpected event type: %s", eventType)
		}
		progress, ok := data.(minecraft.DownloadProgress)
		if !ok {
			t.Fatalf("unexpected payload type: %T", data)
		}
		done <- progress
	}); err != nil {
		t.Fatalf("BridgeLoaderInstallEvents returned error: %v", err)
	}

	ci.ProgressCh <- minecraft.DownloadProgress{Phase: "done", Current: 1, Total: 1, Done: true}
	close(ci.ProgressCh)

	select {
	case progress := <-done:
		if progress.Phase != "done" || !progress.Done {
			t.Fatalf("unexpected progress: %+v", progress)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for loader progress")
	}
}

func TestBridgeLaunchEvents(t *testing.T) {
	srv := &Server{sessions: NewSessionManager()}
	process := launcher.NewGameProcess(nil, "")
	process.State = launcher.StateRunning
	process.ExitCode = 17
	result := &launcher.LaunchResult{
		SessionID: "session-1",
		Process:   process,
	}
	srv.sessions.Add(result)

	type emittedEvent struct {
		Type string
		Data map[string]any
	}

	var (
		mu     sync.Mutex
		events []emittedEvent
	)

	if err := srv.BridgeLaunchEvents("session-1", func(eventType string, data any) {
		payload, ok := data.(map[string]any)
		if !ok {
			t.Fatalf("unexpected payload type: %T", data)
		}
		mu.Lock()
		events = append(events, emittedEvent{Type: eventType, Data: payload})
		mu.Unlock()
	}); err != nil {
		t.Fatalf("BridgeLaunchEvents returned error: %v", err)
	}

	process.LogChan <- launcher.LogLine{Source: "stdout", Text: "hello"}
	close(process.LogChan)

	waitFor(func() bool {
		mu.Lock()
		defer mu.Unlock()
		return len(events) == 3
	})

	if events[0].Type != "status" || events[0].Data["state"] != string(launcher.StateRunning) {
		t.Fatalf("unexpected initial status event: %+v", events[0])
	}
	if events[1].Type != "log" || events[1].Data["text"] != "hello" {
		t.Fatalf("unexpected log event: %+v", events[1])
	}
	if events[2].Type != "status" || events[2].Data["state"] != "exited" || events[2].Data["exit_code"] != 17 {
		t.Fatalf("unexpected exit event: %+v", events[2])
	}
}

func waitFor(check func() bool) {
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if check() {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
}
