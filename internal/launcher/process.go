package launcher

import (
	"bufio"
	"fmt"
	"io"
	"os/exec"
	"sync"
)

// ProcessState represents the current state of a launched game process.
type ProcessState string

const (
	StateStarting ProcessState = "starting"
	StateRunning  ProcessState = "running"
	StateExited   ProcessState = "exited"
	StateFailed   ProcessState = "failed"
)

// LogLine represents a single line of game output.
type LogLine struct {
	Source string // "stdout" or "stderr"
	Text   string
}

// GameProcess wraps a running Minecraft process.
type GameProcess struct {
	cmd       *exec.Cmd
	State     ProcessState
	ExitCode  int
	Error     error
	LogChan   chan LogLine
	doneChan  chan struct{}
	mu        sync.RWMutex
	nativesDir string
}

// NewGameProcess creates a game process from an exec.Cmd.
func NewGameProcess(cmd *exec.Cmd, nativesDir string) *GameProcess {
	return &GameProcess{
		cmd:        cmd,
		State:      StateStarting,
		LogChan:    make(chan LogLine, 256),
		doneChan:   make(chan struct{}),
		nativesDir: nativesDir,
	}
}

// Start launches the game process and begins streaming output.
func (gp *GameProcess) Start() error {
	stdout, err := gp.cmd.StdoutPipe()
	if err != nil {
		return fmt.Errorf("stdout pipe: %w", err)
	}
	stderr, err := gp.cmd.StderrPipe()
	if err != nil {
		return fmt.Errorf("stderr pipe: %w", err)
	}

	if err := gp.cmd.Start(); err != nil {
		gp.mu.Lock()
		gp.State = StateFailed
		gp.Error = err
		gp.mu.Unlock()
		close(gp.LogChan)
		close(gp.doneChan)
		return fmt.Errorf("starting game: %w", err)
	}

	gp.mu.Lock()
	gp.State = StateRunning
	gp.mu.Unlock()

	// Stream output in goroutines
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		gp.streamOutput(stdout, "stdout")
	}()
	go func() {
		defer wg.Done()
		gp.streamOutput(stderr, "stderr")
	}()

	// Wait for process to finish
	go func() {
		wg.Wait() // wait for output streaming to finish
		err := gp.cmd.Wait()

		gp.mu.Lock()
		if err != nil {
			gp.State = StateExited
			if exitErr, ok := err.(*exec.ExitError); ok {
				gp.ExitCode = exitErr.ExitCode()
			} else {
				gp.State = StateFailed
				gp.Error = err
				gp.ExitCode = -1
			}
		} else {
			gp.State = StateExited
			gp.ExitCode = 0
		}
		gp.mu.Unlock()

		close(gp.LogChan)
		close(gp.doneChan)

		// Cleanup natives directory
		if gp.nativesDir != "" {
			CleanupNativesDir(gp.nativesDir)
		}
	}()

	return nil
}

func (gp *GameProcess) streamOutput(r io.Reader, source string) {
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := scanner.Text()
		select {
		case gp.LogChan <- LogLine{Source: source, Text: line}:
		default:
			// Drop log lines if channel is full
		}
	}
}

// Kill terminates the game process.
func (gp *GameProcess) Kill() error {
	if gp.cmd.Process == nil {
		return nil
	}
	return gp.cmd.Process.Kill()
}

// Done returns a channel that's closed when the process exits.
func (gp *GameProcess) Done() <-chan struct{} {
	return gp.doneChan
}

// GetState returns the current process state.
func (gp *GameProcess) GetState() ProcessState {
	gp.mu.RLock()
	defer gp.mu.RUnlock()
	return gp.State
}

// PID returns the process ID, or 0 if not running.
func (gp *GameProcess) PID() int {
	if gp.cmd.Process == nil {
		return 0
	}
	return gp.cmd.Process.Pid
}
