//go:build !windows

package launcher

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
)

// Per-process CPU delta state.
var (
	lastProcTicks int64
	lastWallNano  int64
)

func readProcessStats(pid int) processStats {
	var ps processStats

	// /proc/<pid>/stat for thread count, CPU ticks
	statPath := fmt.Sprintf("/proc/%d/stat", pid)
	data, err := os.ReadFile(statPath)
	if err != nil {
		return ps
	}

	// The comm field (2nd) can contain spaces; find the closing paren first.
	s := string(data)
	closeIdx := strings.LastIndex(s, ")")
	if closeIdx < 0 || closeIdx+2 >= len(s) {
		return ps
	}
	fields := strings.Fields(s[closeIdx+2:])

	// Fields after comm: state(0) ... utime(11) stime(12) ... num_threads(17)
	if len(fields) > 17 {
		ps.threads, _ = strconv.Atoi(fields[17])
	}

	// Per-process CPU via utime+stime delta
	if len(fields) > 12 {
		utime, _ := strconv.ParseInt(fields[11], 10, 64)
		stime, _ := strconv.ParseInt(fields[12], 10, 64)
		totalTicks := utime + stime
		now := time.Now().UnixNano()

		if lastWallNano != 0 && now > lastWallNano {
			tickDelta := totalTicks - lastProcTicks
			wallDeltaSec := float64(now-lastWallNano) / 1e9
			if wallDeltaSec > 0 {
				cpuSec := float64(tickDelta) / 100.0 // 100 = typical _SC_CLK_TCK
				ps.cpuPct = cpuSec / wallDeltaSec * 100.0
			}
		}
		lastProcTicks = totalTicks
		lastWallNano = now
	}

	// /proc/<pid>/statm for memory
	statmData, err := os.ReadFile(fmt.Sprintf("/proc/%d/statm", pid))
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

	// /proc/<pid>/io for disk I/O counters
	ioData, err := os.ReadFile(fmt.Sprintf("/proc/%d/io", pid))
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

	return ps
}

func readSystemStats() (cpuPct float64, freeMemBytes int64) {
	// /proc/stat for system CPU
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

	// /proc/meminfo for free memory
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
