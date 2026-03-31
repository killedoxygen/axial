//go:build !windows

package launcher

import (
	"encoding/json"
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
)

// Per-process CPU delta state (Linux: utime+stime from /proc/pid/stat in clock ticks).
var (
	lastProcTicks int64 // cumulative user+system ticks
	lastWallNano  int64 // time.Now().UnixNano() at last sample
)

func readProcessStats(pid int) processStats {
	var ps processStats
	clkTck := int64(100) // sysconf(_SC_CLK_TCK), almost always 100 on Linux

	// Read /proc/<pid>/stat for thread count, CPU ticks, and memory
	statPath := fmt.Sprintf("/proc/%d/stat", pid)
	data, err := os.ReadFile(statPath)
	if err != nil {
		return ps
	}

	// /proc/pid/stat fields are space-separated. The comm field (2nd) can contain
	// spaces and is enclosed in parentheses, so find the closing paren first.
	s := string(data)
	closeIdx := strings.LastIndex(s, ")")
	if closeIdx < 0 || closeIdx+2 >= len(s) {
		return ps
	}
	fields := strings.Fields(s[closeIdx+2:])
	// After the comm field: state(0), ppid(1), pgrp(2), session(3), tty(4),
	// tpgid(5), flags(6), minflt(7), cminflt(8), majflt(9), cmajflt(10),
	// utime(11), stime(12), cutime(13), cstime(14), priority(15), nice(16),
	// num_threads(17), ...
	if len(fields) > 17 {
		ps.threads, _ = strconv.Atoi(fields[17])
	}

	// Per-process CPU via utime+stime delta
	if len(fields) > 12 {
		utime, _ := strconv.ParseInt(fields[11], 10, 64)
		stime, _ := strconv.ParseInt(fields[12], 10, 64)
		totalTicks := utime + stime

		now := int64(0)
		// Use monotonic-ish wall clock
		now = monotonicNano()

		if lastWallNano != 0 && now > lastWallNano {
			tickDelta := totalTicks - lastProcTicks
			wallDeltaSec := float64(now-lastWallNano) / 1e9
			if wallDeltaSec > 0 {
				// Each tick = 1/clkTck seconds of CPU time
				cpuSec := float64(tickDelta) / float64(clkTck)
				ps.cpuPct = cpuSec / wallDeltaSec * 100.0
			}
		}
		lastProcTicks = totalTicks
		lastWallNano = now
	}

	// Read /proc/<pid>/statm for memory
	statmPath := fmt.Sprintf("/proc/%d/statm", pid)
	statmData, err := os.ReadFile(statmPath)
	if err == nil {
		statmFields := strings.Fields(string(statmData))
		pageSize := int64(os.Getpagesize())
		if len(statmFields) >= 2 {
			totalPages, _ := strconv.ParseInt(statmFields[0], 10, 64)
			resPages, _ := strconv.ParseInt(statmFields[1], 10, 64)
			ps.virt = totalPages * pageSize
			ps.rss = resPages * pageSize
		}
	}

	// Read /proc/<pid>/io for disk I/O counters
	ioPath := fmt.Sprintf("/proc/%d/io", pid)
	ioData, err := os.ReadFile(ioPath)
	if err == nil {
		for _, line := range strings.Split(string(ioData), "\n") {
			parts := strings.SplitN(line, ": ", 2)
			if len(parts) != 2 {
				continue
			}
			val, _ := strconv.ParseInt(strings.TrimSpace(parts[1]), 10, 64)
			switch parts[0] {
			case "read_bytes":
				ps.ioReadBytes = val
			case "write_bytes":
				ps.ioWriteBytes = val
			case "syscr":
				ps.ioReadOps = val
			case "syscw":
				ps.ioWriteOps = val
			}
		}
	}

	_ = runtime.NumCPU() // ensure imported

	return ps
}

func monotonicNano() int64 {
	// /proc/self/stat would give us a clock, but simplest is just time.Now
	// which in Go includes a monotonic component.
	// We import time indirectly, use a simple approach.
	var ts [2]int64
	// Fallback: read /proc/uptime and convert. But simpler to just use
	// the wall clock (close enough for 250ms intervals).
	data, err := os.ReadFile("/proc/uptime")
	if err != nil {
		return 0
	}
	fields := strings.Fields(string(data))
	if len(fields) < 1 {
		return 0
	}
	// uptime is in seconds with fractional part
	secs, _ := strconv.ParseFloat(fields[0], 64)
	ts[0] = int64(secs * 1e9)
	return ts[0]
}

func readSystemStats() (cpuPct float64, freeMemBytes int64) {
	// Read /proc/stat for CPU usage
	data, err := os.ReadFile("/proc/stat")
	if err == nil {
		lines := strings.Split(string(data), "\n")
		if len(lines) > 0 && strings.HasPrefix(lines[0], "cpu ") {
			fields := strings.Fields(lines[0])
			if len(fields) >= 5 {
				user, _ := strconv.ParseFloat(fields[1], 64)
				nice, _ := strconv.ParseFloat(fields[2], 64)
				system, _ := strconv.ParseFloat(fields[3], 64)
				idle, _ := strconv.ParseFloat(fields[4], 64)
				total := user + nice + system + idle
				if total > 0 {
					cpuPct = (total - idle) / total * 100.0
				}
			}
		}
	}

	// Read /proc/meminfo for free memory
	memData, err := os.ReadFile("/proc/meminfo")
	if err == nil {
		for _, line := range strings.Split(string(memData), "\n") {
			if strings.HasPrefix(line, "MemAvailable:") {
				fields := strings.Fields(line)
				if len(fields) >= 2 {
					kb, _ := strconv.ParseInt(fields[1], 10, 64)
					freeMemBytes = kb * 1024
				}
				break
			}
		}
	}

	return cpuPct, freeMemBytes
}

func marshalJSON(v any) ([]byte, error) {
	return json.MarshalIndent(v, "", "  ")
}
