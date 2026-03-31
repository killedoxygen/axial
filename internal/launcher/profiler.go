package launcher

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"time"

	"github.com/mateoltd/croopor/internal/config"
)

// BootProfile captures diagnostic data during Minecraft boot to identify
// what causes system lag. Samples are taken every 500ms until boot completes.
type BootProfile struct {
	// Metadata
	SessionID  string    `json:"session_id"`
	VersionID  string    `json:"version_id"`
	StartedAt  time.Time `json:"started_at"`
	FinishedAt time.Time `json:"finished_at,omitempty"`

	// System info
	SystemCPUs int    `json:"system_cpus"`
	SystemOS   string `json:"system_os"`
	SystemArch string `json:"system_arch"`

	// Configuration applied
	JVMPreset   string `json:"jvm_preset"`
	MaxMemoryMB int    `json:"max_memory_mb"`
	ThrottleCap int    `json:"throttle_cap_pct"`
	CDS         bool   `json:"cds_enabled"`

	// Samples collected during boot
	Samples []BootSample `json:"samples"`

	// Summary (computed at end)
	BootDurationMs int64   `json:"boot_duration_ms,omitempty"`
	PeakCPUPct     float64 `json:"peak_cpu_pct,omitempty"`
	PeakMemMB      int64   `json:"peak_mem_mb,omitempty"`
	PeakThreads    int     `json:"peak_threads,omitempty"`

	mu      sync.Mutex
	pid     int
	stopCh  chan struct{}
	stopped bool
}

// BootSample is a single point-in-time snapshot of the process.
type BootSample struct {
	ElapsedMs     int64   `json:"elapsed_ms"`
	CPUPct        float64 `json:"cpu_pct"`         // Process CPU usage (% of one core)
	MemResidentMB int64   `json:"mem_resident_mb"` // RSS in MB
	MemVirtualMB  int64   `json:"mem_virtual_mb"`  // Virtual memory in MB
	ThreadCount   int     `json:"thread_count"`
	// Disk I/O metrics (cumulative since process start)
	IOReadMB   float64 `json:"io_read_mb"`   // Total bytes read from disk
	IOWriteMB  float64 `json:"io_write_mb"`  // Total bytes written to disk
	IOReadOps  int64   `json:"io_read_ops"`  // Number of read syscalls
	IOWriteOps int64   `json:"io_write_ops"` // Number of write syscalls
	// System-wide metrics
	SystemCPUPct float64 `json:"system_cpu_pct"` // Total system CPU usage
	SystemFreeMB int64   `json:"system_free_mb"` // Free physical memory
}

// NewBootProfile creates a profiler for a game launch session.
func NewBootProfile(sessionID, versionID string, pid int, jvmPreset string, maxMem int, throttleCap int, cds bool) *BootProfile {
	return &BootProfile{
		SessionID:   sessionID,
		VersionID:   versionID,
		StartedAt:   time.Now(),
		SystemCPUs:  runtime.NumCPU(),
		SystemOS:    runtime.GOOS,
		SystemArch:  runtime.GOARCH,
		JVMPreset:   jvmPreset,
		MaxMemoryMB: maxMem,
		ThrottleCap: throttleCap,
		CDS:         cds,
		pid:         pid,
		stopCh:      make(chan struct{}),
	}
}

// Start begins sampling in a background goroutine. Call Stop() when boot completes.
func (bp *BootProfile) Start() {
	go bp.sampleLoop()
}

// Stop ends sampling and computes summary statistics.
func (bp *BootProfile) Stop() {
	bp.mu.Lock()
	if bp.stopped {
		bp.mu.Unlock()
		return
	}
	bp.stopped = true
	bp.mu.Unlock()

	close(bp.stopCh)
	bp.FinishedAt = time.Now()
	bp.BootDurationMs = bp.FinishedAt.Sub(bp.StartedAt).Milliseconds()

	// Compute peaks
	for _, s := range bp.Samples {
		if s.CPUPct > bp.PeakCPUPct {
			bp.PeakCPUPct = s.CPUPct
		}
		if s.MemResidentMB > bp.PeakMemMB {
			bp.PeakMemMB = s.MemResidentMB
		}
		if s.ThreadCount > bp.PeakThreads {
			bp.PeakThreads = s.ThreadCount
		}
	}
}

func (bp *BootProfile) sampleLoop() {
	ticker := time.NewTicker(250 * time.Millisecond)
	defer ticker.Stop()

	for {
		select {
		case <-bp.stopCh:
			return
		case <-ticker.C:
			sample := bp.takeSample()
			bp.mu.Lock()
			bp.Samples = append(bp.Samples, sample)
			bp.mu.Unlock()
		}
	}
}

func (bp *BootProfile) takeSample() BootSample {
	elapsed := time.Since(bp.StartedAt).Milliseconds()

	ps := readProcessStats(bp.pid)
	sysCPU, sysFree := readSystemStats()

	return BootSample{
		ElapsedMs:     elapsed,
		CPUPct:        ps.cpuPct,
		MemResidentMB: ps.rss / (1024 * 1024),
		MemVirtualMB:  ps.virt / (1024 * 1024),
		ThreadCount:   ps.threads,
		IOReadMB:      float64(ps.ioReadBytes) / (1024 * 1024),
		IOWriteMB:     float64(ps.ioWriteBytes) / (1024 * 1024),
		IOReadOps:     ps.ioReadOps,
		IOWriteOps:    ps.ioWriteOps,
		SystemCPUPct:  sysCPU,
		SystemFreeMB:  sysFree / (1024 * 1024),
	}
}

// processStats holds raw metrics returned by the platform-specific readProcessStats.
type processStats struct {
	cpuPct       float64
	rss          int64
	virt         int64
	threads      int
	ioReadBytes  int64
	ioWriteBytes int64
	ioReadOps    int64
	ioWriteOps   int64
}

// SaveReport writes the profile as JSON to the config directory.
// Returns the file path.
func (bp *BootProfile) SaveReport() (string, error) {
	dir := filepath.Join(config.ConfigDir(), "boot-profiles")
	if err := os.MkdirAll(dir, 0755); err != nil {
		return "", fmt.Errorf("create profile dir: %w", err)
	}

	filename := fmt.Sprintf("%s_%s_%s.json",
		bp.StartedAt.Format("20060102-150405"),
		bp.VersionID,
		bp.SessionID[:8],
	)
	path := filepath.Join(dir, filename)

	data, err := marshalJSON(bp)
	if err != nil {
		return "", err
	}

	if err := os.WriteFile(path, data, 0644); err != nil {
		return "", err
	}
	return path, nil
}

// GetSamples returns a copy of the current samples (safe for concurrent access).
func (bp *BootProfile) GetSamples() []BootSample {
	bp.mu.Lock()
	defer bp.mu.Unlock()
	out := make([]BootSample, len(bp.Samples))
	copy(out, bp.Samples)
	return out
}
