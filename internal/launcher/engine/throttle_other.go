//go:build !windows

package engine

import (
	"fmt"
	"log"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"syscall"
)

// BootThrottle on Linux uses CPU affinity to pin the JVM to high-numbered
// cores plus I/O class and nice value to keep it from starving other apps.
type BootThrottle struct {
	pid    int
	cpus   int // total CPUs at creation time (for release)
	active bool
}

// NewBootThrottle creates a throttle. On Linux, the actual work happens in AssignProcess.
func NewBootThrottle(_ int) (*BootThrottle, error) {
	return &BootThrottle{cpus: runtime.NumCPU()}, nil
}

// AssignProcess restricts the process to high-numbered CPU cores, sets idle
// I/O class, and lowers priority. Every step is verified and failures are
// logged but never abort the launch.
func (bt *BootThrottle) AssignProcess(pid int) error {
	if bt == nil {
		return nil
	}
	bt.pid = pid

	cpus := bt.cpus
	if cpus > 64 {
		cpus = 64 // affinity mask is 64 bits
	}
	allowed := computeBootCores(cpus)

	if allowed > 0 && allowed < cpus {
		// Pin to high-numbered cores: (cpus-allowed) through (cpus-1).
		// This keeps low-numbered cores (where the OS and most user apps run) free.
		startCore := cpus - allowed
		var mask uint64
		for i := startCore; i < cpus; i++ {
			mask |= 1 << uint(i)
		}
		maskStr := fmt.Sprintf("0x%x", mask)

		if err := exec.Command("taskset", "-p", maskStr, strconv.Itoa(pid)).Run(); err != nil {
			log.Printf("boot throttle: taskset failed: %v (continuing without affinity)", err)
		} else {
			// Integrity check: read back and verify
			if actual, ok := readTasksetMask(pid); ok {
				if actual != mask {
					log.Printf("boot throttle: affinity mismatch (set=0x%x, got=0x%x)", mask, actual)
				} else {
					log.Printf("boot throttle: pinned to cores %d-%d (mask=0x%x, %d cores)",
						startCore, cpus-1, mask, allowed)
				}
			}
		}
	}

	// Set I/O to idle class so disk reads don't starve other apps
	if err := exec.Command("ionice", "-c", "3", "-p", strconv.Itoa(pid)).Run(); err != nil {
		log.Printf("boot throttle: ionice failed: %v (continuing without I/O throttle)", err)
	}

	// Lower CPU priority
	if err := syscall.Setpriority(syscall.PRIO_PROCESS, pid, 10); err != nil {
		log.Printf("boot throttle: nice failed: %v", err)
	}

	bt.active = true
	return nil
}

// Release restores full CPU affinity, normal I/O class, and default priority.
// Safe to call even if the process has already exited.
func (bt *BootThrottle) Release() error {
	if bt == nil || !bt.active {
		return nil
	}

	cpus := bt.cpus
	if cpus > 64 {
		cpus = 64
	}

	// Restore full affinity
	var fullMask uint64
	for i := 0; i < cpus; i++ {
		fullMask |= 1 << uint(i)
	}
	fullMaskStr := fmt.Sprintf("0x%x", fullMask)
	if err := exec.Command("taskset", "-p", fullMaskStr, strconv.Itoa(bt.pid)).Run(); err != nil {
		log.Printf("boot throttle: release taskset failed: %v", err)
	} else {
		// Integrity check: verify full affinity was restored
		if actual, ok := readTasksetMask(bt.pid); ok && actual != fullMask {
			log.Printf("boot throttle: release affinity mismatch (expected=0x%x, got=0x%x)", fullMask, actual)
		}
	}

	// Restore default I/O class (best-effort)
	if err := exec.Command("ionice", "-c", "0", "-p", strconv.Itoa(bt.pid)).Run(); err != nil {
		log.Printf("boot throttle: release ionice failed: %v", err)
	}

	// Restore normal priority
	if err := syscall.Setpriority(syscall.PRIO_PROCESS, bt.pid, 0); err != nil {
		log.Printf("boot throttle: release nice failed: %v", err)
	}

	bt.active = false
	log.Printf("boot throttle: released (affinity + I/O + priority)")
	return nil
}

// Close is a no-op on Linux (no handles to release).
func (bt *BootThrottle) Close() {}

// readTasksetMask runs `taskset -p <pid>` and parses the hex affinity mask.
// Output format: "pid 12345's current affinity mask: ff00\n"
func readTasksetMask(pid int) (uint64, bool) {
	out, err := exec.Command("taskset", "-p", strconv.Itoa(pid)).Output()
	if err != nil {
		return 0, false
	}
	s := strings.TrimSpace(string(out))
	idx := strings.LastIndex(s, ": ")
	if idx < 0 {
		return 0, false
	}
	hexStr := strings.TrimSpace(s[idx+2:])
	val, err := strconv.ParseUint(hexStr, 16, 64)
	if err != nil {
		return 0, false
	}
	return val, true
}

// computeBootCores decides how many cores to allocate for the JVM during boot.
// Returns 0 if the machine has too few cores to meaningfully partition.
func computeBootCores(cpus int) int {
	if cpus < 4 {
		return 0
	}
	allowed := cpus / 3
	if allowed < 2 {
		allowed = 2
	}
	if allowed > 8 {
		allowed = 8
	}
	return allowed
}

func bootCPUCap() int {
	return 70
}
