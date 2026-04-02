//go:build darwin

package system

import "runtime"

// detectGPU returns an unknown GPU profile on macOS.
// macOS GPU detection is not implemented yet.
func detectGPU() GPUProfile {
	return GPUProfile{Vendor: GPUVendorUnknown}
}

// detectPhysicalCores returns runtime.NumCPU() on macOS as a fallback.
func detectPhysicalCores() int {
	return runtime.NumCPU()
}
