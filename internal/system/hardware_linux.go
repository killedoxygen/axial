//go:build linux

package system

import (
	"log"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
)

// detectGPU reads GPU vendor from sysfs and attempts model detection for NVIDIA.
func detectGPU() GPUProfile {
	vendor := readGPUVendor()
	if vendor == GPUVendorUnknown {
		return GPUProfile{Vendor: GPUVendorUnknown}
	}

	gpu := GPUProfile{Vendor: vendor}

	if vendor == GPUVendorNVIDIA {
		if model := readNVIDIAModel(); model != "" {
			gpu.ModelName = model
			gpu.NVArch = inferNVIDIAArch(model)
		}
	}

	return gpu
}

// gpuPriority returns a sort weight for GPU vendor selection.
// Higher is better. Matches the Windows detection preference order.
func gpuPriority(v GPUVendor) int {
	switch v {
	case GPUVendorNVIDIA:
		return 3
	case GPUVendorAMD:
		return 2
	case GPUVendorIntel:
		return 1
	default:
		return 0
	}
}

func readGPUVendor() GPUVendor {
	entries, err := os.ReadDir("/sys/class/drm")
	if err != nil {
		return GPUVendorUnknown
	}

	best := GPUVendorUnknown
	for _, entry := range entries {
		if !strings.HasPrefix(entry.Name(), "card") {
			continue
		}
		// Skip render nodes like card0-HDMI-A-1
		if strings.Contains(entry.Name(), "-") {
			continue
		}
		path := filepath.Join("/sys/class/drm", entry.Name(), "device/vendor")
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		var v GPUVendor
		switch strings.TrimSpace(string(data)) {
		case "0x10de":
			v = GPUVendorNVIDIA
		case "0x1002":
			v = GPUVendorAMD
		case "0x8086":
			v = GPUVendorIntel
		default:
			continue
		}
		if gpuPriority(v) > gpuPriority(best) {
			best = v
		}
	}
	return best
}

func readNVIDIAModel() string {
	matches, err := filepath.Glob("/proc/driver/nvidia/gpus/*/information")
	if err != nil || len(matches) == 0 {
		return ""
	}

	data, err := os.ReadFile(matches[0])
	if err != nil {
		return ""
	}

	for _, line := range strings.Split(string(data), "\n") {
		if strings.HasPrefix(line, "Model:") {
			return strings.TrimSpace(strings.TrimPrefix(line, "Model:"))
		}
	}
	return ""
}

// detectPhysicalCores counts unique physical core IDs from sysfs topology.
// Falls back to runtime.NumCPU() if topology info is unavailable.
func detectPhysicalCores() int {
	// Read the possible CPU range to know which CPUs to check
	possibleData, err := os.ReadFile("/sys/devices/system/cpu/possible")
	if err != nil {
		log.Printf("hardware detect: cannot read CPU possible range: %v", err)
		return runtime.NumCPU()
	}

	maxCPU := parseCPURange(strings.TrimSpace(string(possibleData)))
	if maxCPU < 0 {
		return runtime.NumCPU()
	}

	seen := make(map[string]struct{})
	for i := 0; i <= maxCPU; i++ {
		cpuDir := filepath.Join("/sys/devices/system/cpu", "cpu"+strconv.Itoa(i), "topology")
		coreData, err := os.ReadFile(filepath.Join(cpuDir, "core_id"))
		if err != nil {
			continue
		}
		pkgData, _ := os.ReadFile(filepath.Join(cpuDir, "physical_package_id"))
		pkg := strings.TrimSpace(string(pkgData))
		core := strings.TrimSpace(string(coreData))
		seen[pkg+":"+core] = struct{}{}
	}

	if len(seen) == 0 {
		return runtime.NumCPU()
	}
	return len(seen)
}

// parseCPURange parses "0-N" format from /sys/devices/system/cpu/possible and returns N.
func parseCPURange(s string) int {
	// Format is typically "0-N" or just "0"
	parts := strings.Split(s, "-")
	if len(parts) == 0 {
		return -1
	}
	last := parts[len(parts)-1]
	n, err := strconv.Atoi(last)
	if err != nil {
		return -1
	}
	return n
}
