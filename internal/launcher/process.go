package launcher

import (
	"bufio"
	"fmt"
	"io"
	"log"
	"os/exec"
	"strings"
	"sync"
	"time"
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
	cmd           *exec.Cmd
	State         ProcessState
	ExitCode      int
	Error         error
	LogChan       chan LogLine
	doneChan      chan struct{}
	mu            sync.RWMutex
	nativesDir    string
	bootCompleted bool
	bootDuration  time.Duration
	startTime     time.Time
	throttle      *BootThrottle
	Profile       *BootProfile
	CDSFailed     bool // set if JVM reports CDS archive errors
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
	gp.startTime = time.Now()
	gp.mu.Unlock()

	// Set low priority after start (no-op on Windows where CreationFlags handles it)
	setLowPriority(gp.cmd.Process.Pid)

	// Apply boot throttle: hard CPU cap (Windows Job Object) or affinity restriction (Linux)
	throttle, err := NewBootThrottle(bootCPUCap())
	if err == nil {
		if err := throttle.AssignProcess(gp.cmd.Process.Pid); err != nil {
			log.Printf("boot throttle assign failed: %v", err)
		} else {
			gp.throttle = throttle
		}
	}

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

		// Stop profiler if still running (process exited before boot completed)
		if gp.Profile != nil {
			gp.Profile.Stop()
			if !gp.bootCompleted {
				if path, err := gp.Profile.SaveReport(); err == nil {
					log.Printf("boot profile saved (process exited before boot): %s", path)
				}
			}
		}

		// Cleanup natives directory
		if gp.nativesDir != "" {
			CleanupNativesDir(gp.nativesDir)
		}

		// Cleanup throttle handle
		if gp.throttle != nil {
			gp.throttle.Close()
		}
	}()

	return nil
}

// Boot-complete marker strings. When any of these appear in game output,
// the game has finished loading and we can promote the process to normal priority.
var bootMarkers = []string{
	"Setting user:",  // Modern versions after login
	"LWJGL Version",  // Legacy versions during LWJGL init
	"[Render thread", // 1.13+ render thread initialization
}

func (gp *GameProcess) streamOutput(r io.Reader, source string) {
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := scanner.Text()

		// Detect boot completion and promote process priority
		gp.mu.RLock()
		booted := gp.bootCompleted
		gp.mu.RUnlock()
		if !booted {
			for _, marker := range bootMarkers {
				if strings.Contains(line, marker) {
					gp.markBootCompleted()
					break
				}
			}
		}

		// Detect CDS archive errors (e.g. corrupted or incompatible archive)
		if source == "stderr" && (strings.Contains(line, "[error][cds]") || strings.Contains(line, "Unable to use shared archive")) {
			gp.mu.Lock()
			gp.CDSFailed = true
			gp.mu.Unlock()
		}

		select {
		case gp.LogChan <- LogLine{Source: source, Text: line}:
		default:
			// Drop log lines if channel is full
		}
	}
}

func (gp *GameProcess) markBootCompleted() {
	gp.mu.Lock()
	if gp.bootCompleted {
		gp.mu.Unlock()
		return
	}
	gp.bootCompleted = true
	gp.bootDuration = time.Since(gp.startTime)
	throttle := gp.throttle
	profile := gp.Profile
	gp.mu.Unlock()

	// Stop profiler and save report
	if profile != nil {
		profile.Stop()
		if path, err := profile.SaveReport(); err != nil {
			log.Printf("failed to save boot profile: %v", err)
		} else {
			log.Printf("boot profile saved: %s (duration=%s, peak_threads=%d, peak_mem=%dMB)",
				path, gp.bootDuration, profile.PeakThreads, profile.PeakMemMB)
		}
	}

	// Release CPU throttle (Job Object cap on Windows, affinity on Linux)
	if throttle != nil {
		if err := throttle.Release(); err != nil {
			log.Printf("failed to release boot throttle: %v", err)
		}
	}

	// Promote from BELOW_NORMAL to NORMAL priority
	if err := promoteProcess(gp.cmd.Process.Pid); err != nil {
		log.Printf("failed to promote process priority: %v", err)
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
	if gp.cmd == nil || gp.cmd.Process == nil {
		return 0
	}
	return gp.cmd.Process.Pid
}

// BootCompleted returns whether the game has finished booting.
func (gp *GameProcess) BootCompleted() bool {
	gp.mu.RLock()
	defer gp.mu.RUnlock()
	return gp.bootCompleted
}

// BootDuration returns how long the game took to boot, or 0 if not yet booted.
func (gp *GameProcess) BootDuration() time.Duration {
	gp.mu.RLock()
	defer gp.mu.RUnlock()
	return gp.bootDuration
}
