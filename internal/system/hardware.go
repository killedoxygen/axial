package system

import (
	"bufio"
	"context"
	"log"
	"os/exec"
	"runtime"
	"strings"
	"sync"
	"time"
)

// GPUVendor identifies the GPU manufacturer.
type GPUVendor string

const (
	GPUVendorNVIDIA  GPUVendor = "nvidia"
	GPUVendorAMD     GPUVendor = "amd"
	GPUVendorIntel   GPUVendor = "intel"
	GPUVendorUnknown GPUVendor = "unknown"
)

// NVIDIAArch is the NVIDIA microarchitecture generation, used to gate Nvidium.
// Nvidium requires Turing (RTX 20xx / GTX 16xx) or newer.
type NVIDIAArch int

const (
	NVIDIAArchUnknown NVIDIAArch = 0
	NVIDIAArchPascal  NVIDIAArch = 1 // GTX 10xx
	NVIDIAArchTuring  NVIDIAArch = 2 // RTX 20xx / GTX 16xx — minimum for Nvidium
	NVIDIAArchAmpere  NVIDIAArch = 3 // RTX 30xx
	NVIDIAArchAda       NVIDIAArch = 4 // RTX 40xx
	NVIDIAArchBlackwell NVIDIAArch = 5 // RTX 50xx
)

// GPUProfile describes the primary GPU.
type GPUProfile struct {
	Vendor    GPUVendor
	ModelName string     // e.g. "NVIDIA GeForce RTX 3080"
	NVArch    NVIDIAArch // Only meaningful when Vendor == GPUVendorNVIDIA
}

// CPUProfile describes CPU capabilities relevant to GC preset selection.
type CPUProfile struct {
	LogicalCores  int  // runtime.NumCPU()
	PhysicalCores int  // best-effort; falls back to LogicalCores if unknown
	IsLowEnd      bool // true if LogicalCores <= 4
}

// JavaDistribution identifies the JVM vendor.
type JavaDistribution string

const (
	JavaDistributionGraalVM JavaDistribution = "graalvm"
	JavaDistributionTemurin JavaDistribution = "temurin"
	JavaDistributionOracle  JavaDistribution = "oracle"
	JavaDistributionOpenJDK JavaDistribution = "openjdk"
	JavaDistributionUnknown JavaDistribution = "unknown"
)

// HardwareTier classifies the system for composition and memory decisions.
type HardwareTier string

const (
	HardwareTierLow  HardwareTier = "low"  // <=4 cores OR <=8GB RAM
	HardwareTierMid  HardwareTier = "mid"  // everything between low and high
	HardwareTierHigh HardwareTier = "high" // >=8 cores AND >=16GB RAM
)

// HardwareProfile is the complete system snapshot consumed by later phases.
type HardwareProfile struct {
	CPU        CPUProfile
	GPU        GPUProfile
	TotalRAMMB int
	Tier       HardwareTier
}

var (
	hwOnce    sync.Once
	hwCached  HardwareProfile
	javaCache sync.Map // map[string]JavaDistribution
)

// nvidiaArchTable maps model name substrings to NVIDIA microarchitecture.
// Checked in order; first match wins.
var nvidiaArchTable = []struct {
	substr string
	arch   NVIDIAArch
}{
	{"RTX 50", NVIDIAArchBlackwell},
	{"RTX 40", NVIDIAArchAda},
	{"RTX 30", NVIDIAArchAmpere},
	{"RTX 20", NVIDIAArchTuring},
	{"RTX20", NVIDIAArchTuring},
	{"GTX 16", NVIDIAArchTuring},
	{"GTX16", NVIDIAArchTuring},
	{"GTX 10", NVIDIAArchPascal},
	{"GTX10", NVIDIAArchPascal},
}

// gpuPriority returns the preference order for GPU vendor selection.
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

// inferNVIDIAArch determines the NVIDIA microarchitecture from the GPU model name.
func inferNVIDIAArch(model string) NVIDIAArch {
	upper := strings.ToUpper(model)
	for _, entry := range nvidiaArchTable {
		if strings.Contains(upper, strings.ToUpper(entry.substr)) {
			return entry.arch
		}
	}
	return NVIDIAArchUnknown
}

// classifyTier derives HardwareTier from CPU and RAM.
func classifyTier(cpu CPUProfile, totalRAMMB int) HardwareTier {
	if cpu.LogicalCores <= 4 || totalRAMMB <= 8192 {
		return HardwareTierLow
	}
	if cpu.LogicalCores >= 8 && totalRAMMB >= 16384 {
		return HardwareTierHigh
	}
	return HardwareTierMid
}

// Detect returns the hardware profile for the current system.
// It is safe to call multiple times; the second call returns the cached result.
// Detection errors are non-fatal: the caller gets a best-effort profile.
func Detect() HardwareProfile {
	hwOnce.Do(func() {
		hwCached = detect()
	})
	return hwCached
}

func detect() HardwareProfile {
	logical := runtime.NumCPU()

	totalRAMMB := 0
	if mb, err := TotalMemoryMB(); err != nil {
		log.Printf("hardware detect: failed to read total RAM: %v", err)
	} else {
		totalRAMMB = mb
	}

	physical := detectPhysicalCores()
	if physical <= 0 {
		physical = logical
	}

	cpu := CPUProfile{
		LogicalCores:  logical,
		PhysicalCores: physical,
		IsLowEnd:      logical <= 4,
	}

	gpu := detectGPU()

	return HardwareProfile{
		CPU:        cpu,
		GPU:        gpu,
		TotalRAMMB: totalRAMMB,
		Tier:       classifyTier(cpu, totalRAMMB),
	}
}

// DetectJavaDistribution identifies the JVM vendor for the given java binary.
// Results are cached per javaPath. On error, returns JavaDistributionUnknown.
func DetectJavaDistribution(javaPath string) JavaDistribution {
	if cached, ok := javaCache.Load(javaPath); ok {
		return cached.(JavaDistribution)
	}

	dist := detectJavaDistribution(javaPath)
	javaCache.Store(javaPath, dist)
	return dist
}

func detectJavaDistribution(javaPath string) JavaDistribution {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, javaPath, "-XshowSettings:property", "-version")
	out, err := cmd.CombinedOutput()
	if err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			log.Printf("hardware detect: java distribution probe timed out for %s", javaPath)
		} else {
			log.Printf("hardware detect: failed to run java for distribution detection: %v", err)
		}
		return JavaDistributionUnknown
	}

	scanner := bufio.NewScanner(strings.NewReader(string(out)))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if strings.HasPrefix(line, "java.vendor") && strings.Contains(line, "=") {
			parts := strings.SplitN(line, "=", 2)
			if len(parts) < 2 {
				continue
			}
			vendor := strings.TrimSpace(parts[1])
			upper := strings.ToUpper(vendor)
			switch {
			case strings.Contains(upper, "GRAALVM"):
				return JavaDistributionGraalVM
			case strings.Contains(upper, "TEMURIN") || strings.Contains(upper, "ECLIPSE"):
				return JavaDistributionTemurin
			case strings.Contains(upper, "ORACLE"):
				return JavaDistributionOracle
			default:
				return JavaDistributionOpenJDK
			}
		}
	}

	return JavaDistributionUnknown
}
