package system

import (
	"bufio"
	"context"
	"log"
	"os/exec"
	"regexp"
	"runtime"
	"strconv"
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
	NVIDIAArchUnknown   NVIDIAArch = 0
	NVIDIAArchPascal    NVIDIAArch = 1 // GTX 10xx
	NVIDIAArchTuring    NVIDIAArch = 2 // RTX 20xx / GTX 16xx — minimum for Nvidium
	NVIDIAArchAmpere    NVIDIAArch = 3 // RTX 30xx
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
	JavaDistributionOpenJ9  JavaDistribution = "openj9"
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

// JavaRuntimeInfo describes the detected runtime behind a java binary path.
type JavaRuntimeInfo struct {
	Distribution JavaDistribution
	Major        int
	Update       int
	Version      string
}

var (
	hwOnce    sync.Once
	hwCached  HardwareProfile
	javaCache sync.Map // map[string]JavaRuntimeInfo
)

var javaVersionPattern = regexp.MustCompile(`(?i)(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:[_\.](\d+))?`)

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
	return DetectJavaRuntimeInfo(javaPath).Distribution
}

// DetectJavaRuntimeInfo identifies the JVM vendor and version details for the given java binary.
// Results are cached per javaPath. On error, returns conservative defaults.
func DetectJavaRuntimeInfo(javaPath string) JavaRuntimeInfo {
	if cached, ok := javaCache.Load(javaPath); ok {
		return cached.(JavaRuntimeInfo)
	}

	info := detectJavaRuntimeInfo(javaPath)
	javaCache.Store(javaPath, info)
	return info
}

func detectJavaRuntimeInfo(javaPath string) JavaRuntimeInfo {
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
		return JavaRuntimeInfo{Distribution: JavaDistributionUnknown}
	}

	info := JavaRuntimeInfo{Distribution: JavaDistributionUnknown}
	scanner := bufio.NewScanner(strings.NewReader(string(out)))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		parts := strings.SplitN(line, "=", 2)
		if len(parts) < 2 {
			continue
		}
		key := strings.TrimSpace(parts[0])
		value := strings.TrimSpace(parts[1])

		if key == "java.vendor" {
			vendor := value
			upper := strings.ToUpper(vendor)
			switch {
			case strings.Contains(upper, "GRAALVM"):
				info.Distribution = JavaDistributionGraalVM
			case strings.Contains(upper, "OPENJ9") || strings.Contains(upper, "SEMERU") || strings.Contains(upper, "IBM"):
				info.Distribution = JavaDistributionOpenJ9
			case strings.Contains(upper, "TEMURIN") || strings.Contains(upper, "ECLIPSE"):
				info.Distribution = JavaDistributionTemurin
			case strings.Contains(upper, "ORACLE"):
				info.Distribution = JavaDistributionOracle
			default:
				info.Distribution = JavaDistributionOpenJDK
			}
			continue
		}
		if key == "java.version" || key == "java.runtime.version" {
			version := value
			if info.Version == "" {
				info.Version = version
			}
			major, update := parseJavaVersion(version)
			if major > 0 {
				info.Major = major
			}
			if update > 0 {
				info.Update = update
			}
		}
	}

	return info
}

func parseJavaVersion(version string) (major int, update int) {
	version = strings.Trim(version, `"`)
	match := javaVersionPattern.FindStringSubmatch(version)
	if match == nil {
		return 0, 0
	}

	parts := make([]int, 0, 4)
	for _, raw := range match[1:] {
		if raw == "" {
			continue
		}
		n, err := strconv.Atoi(raw)
		if err != nil {
			continue
		}
		parts = append(parts, n)
	}
	if len(parts) == 0 {
		return 0, 0
	}
	if parts[0] == 1 && len(parts) >= 2 {
		major = parts[1]
		if len(parts) >= 4 {
			update = parts[3]
		} else if len(parts) >= 3 {
			update = parts[2]
		}
		return major, update
	}

	major = parts[0]
	if major == 8 && len(parts) >= 4 {
		update = parts[3]
	}
	return major, update
}
