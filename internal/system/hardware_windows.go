//go:build windows

package system

import (
	"log"
	"runtime"
	"strings"

	"golang.org/x/sys/windows/registry"
)

// detectGPU reads GPU information from the Windows registry.
func detectGPU() GPUProfile {
	const videoKeyPath = `SYSTEM\CurrentControlSet\Control\Video`

	videoKey, err := registry.OpenKey(registry.LOCAL_MACHINE, videoKeyPath, registry.ENUMERATE_SUB_KEYS)
	if err != nil {
		log.Printf("hardware detect: failed to open Video registry key: %v", err)
		return GPUProfile{Vendor: GPUVendorUnknown}
	}
	defer videoKey.Close()

	guids, err := videoKey.ReadSubKeyNames(-1)
	if err != nil {
		log.Printf("hardware detect: failed to enumerate Video subkeys: %v", err)
		return GPUProfile{Vendor: GPUVendorUnknown}
	}

	// Collect all GPUs, then pick the best one (NVIDIA > AMD > Intel).
	var best GPUProfile
	best.Vendor = GPUVendorUnknown

	for _, guid := range guids {
		subKeyPath := videoKeyPath + `\` + guid + `\0000`
		subKey, err := registry.OpenKey(registry.LOCAL_MACHINE, subKeyPath, registry.QUERY_VALUE)
		if err != nil {
			continue
		}

		desc, _, err := subKey.GetStringValue("DriverDesc")
		subKey.Close()
		if err != nil || desc == "" {
			continue
		}

		gpu := parseGPUFromDesc(desc)
		if gpuPriority(gpu.Vendor) > gpuPriority(best.Vendor) {
			best = gpu
		}
	}

	return best
}

func parseGPUFromDesc(desc string) GPUProfile {
	upper := strings.ToUpper(desc)
	var vendor GPUVendor
	switch {
	case strings.Contains(upper, "NVIDIA"):
		vendor = GPUVendorNVIDIA
	case strings.Contains(upper, "AMD") || strings.Contains(upper, "RADEON"):
		vendor = GPUVendorAMD
	case strings.Contains(upper, "INTEL"):
		vendor = GPUVendorIntel
	default:
		vendor = GPUVendorUnknown
	}

	gpu := GPUProfile{
		Vendor:    vendor,
		ModelName: desc,
	}

	if vendor == GPUVendorNVIDIA {
		gpu.NVArch = inferNVIDIAArch(desc)
	}

	return gpu
}

// gpuPriority returns the preference order for GPU vendors.
// Higher value = more preferred when multiple GPUs are found.
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

// detectPhysicalCores returns the physical core count.
// On Windows, we use runtime.NumCPU() as a safe fallback since
// SYSTEM_LOGICAL_PROCESSOR_INFORMATION is complex.
func detectPhysicalCores() int {
	return runtime.NumCPU()
}
