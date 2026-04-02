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

func readGPUVendor() GPUVendor {
	// Try card0 first, then card1
	for _, card := range []string{"card0", "card1"} {
		path := filepath.Join("/sys/class/drm", card, "device/vendor")
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		vendorID := strings.TrimSpace(string(data))
		switch vendorID {
		case "0x10de":
			return GPUVendorNVIDIA
		case "0x1002":
			return GPUVendorAMD
		case "0x8086":
			return GPUVendorIntel
		}
	}
	return GPUVendorUnknown
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
		path := filepath.Join("/sys/devices/system/cpu", "cpu"+strconv.Itoa(i), "topology/core_id")
		data, err := os.ReadFile(path)
		if err != nil {
			continue
		}
		seen[strings.TrimSpace(string(data))] = struct{}{}
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
