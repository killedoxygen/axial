package launcher

import (
	"testing"
	"time"
)

func TestWaitForStartupReturnsStalledWithoutBootOrLogs(t *testing.T) {
	gp := &GameProcess{
		doneChan: make(chan struct{}),
		bootCh:   make(chan struct{}),
	}

	outcome := gp.WaitForStartup(5 * time.Millisecond)
	if outcome != startupStalled {
		t.Fatalf("expected startupStalled, got %v", outcome)
	}
}

func TestWaitForStartupTimeoutWithLogs(t *testing.T) {
	gp := &GameProcess{
		doneChan:   make(chan struct{}),
		bootCh:     make(chan struct{}),
		recentLogs: []LogLine{{Source: "stderr", Text: "starting"}},
	}

	outcome := gp.WaitForStartup(5 * time.Millisecond)
	if outcome != startupTimedOut {
		t.Fatalf("expected startupTimedOut, got %v", outcome)
	}
}
