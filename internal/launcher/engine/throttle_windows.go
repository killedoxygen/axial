//go:build windows

package engine

import (
	"fmt"
	"log"
	"runtime"
	"unsafe"

	"golang.org/x/sys/windows"
)

// BootThrottle uses a Windows Job Object to hard-cap total CPU usage and pin
// the Minecraft process to a subset of high-numbered cores during boot.
// The CPU rate cap prevents overwhelming the system even if threads burst,
// while affinity isolates cache and memory-bus pressure to cores the OS and
// other apps are not using.
type BootThrottle struct {
	job         windows.Handle
	cpuActive   bool
	affinitySet bool
	fullMask    uintptr // all-cores mask for restore
}

// Windows constants for Job Object control.
const (
	jobObjectBasicLimitInfo            = 2
	jobObjectExtendedLimitInformation  = 9
	jobObjectCPURateControlInformation = 15

	jobObjectCPURateControlEnable  = 0x1
	jobObjectCPURateControlHardCap = 0x4
	jobObjectLimitAffinity         = 0x00000010
)

// JOBOBJECT_CPU_RATE_CONTROL_INFORMATION
type cpuRateControlInfo struct {
	ControlFlags uint32
	CpuRate      uint32 // 1-10000 where 10000 = 100%
	_            [4]byte
}

// JOBOBJECT_BASIC_LIMIT_INFORMATION (64-bit layout).
// Go's implicit padding matches the C ABI: uint32 before uintptr gets 4 bytes
// of padding on amd64, so the struct is 64 bytes total.
type jobBasicLimitInfo struct {
	PerProcessUserTimeLimit int64
	PerJobUserTimeLimit     int64
	LimitFlags              uint32
	MinimumWorkingSetSize   uintptr
	MaximumWorkingSetSize   uintptr
	ActiveProcessLimit      uint32
	Affinity                uintptr
	PriorityClass           uint32
	SchedulingClass         uint32
}

var (
	modkernel32                   = windows.NewLazySystemDLL("kernel32.dll")
	procSetInformationJobObject   = modkernel32.NewProc("SetInformationJobObject")
	procAssignProcessToJobObject  = modkernel32.NewProc("AssignProcessToJobObject")
	procQueryInformationJobObject = modkernel32.NewProc("QueryInformationJobObject")
)

// NewBootThrottle creates a Job Object with a hard CPU rate cap and an
// affinity mask that pins processes to high-numbered cores.
// capPercent is the maximum total CPU usage (e.g. 50 for 50%).
func NewBootThrottle(capPercent int) (*BootThrottle, error) {
	if capPercent <= 0 || capPercent > 100 {
		capPercent = 60
	}

	job, err := windows.CreateJobObject(nil, nil)
	if err != nil {
		return nil, fmt.Errorf("create job object: %w", err)
	}

	bt := &BootThrottle{job: job}

	// --- CPU rate control (hard cap) ---
	rateInfo := cpuRateControlInfo{
		ControlFlags: jobObjectCPURateControlEnable | jobObjectCPURateControlHardCap,
		CpuRate:      uint32(capPercent * 100), // basis points: 5000 = 50%
	}
	ret, _, err := procSetInformationJobObject.Call(
		uintptr(job),
		uintptr(jobObjectCPURateControlInformation),
		uintptr(unsafe.Pointer(&rateInfo)),
		uintptr(unsafe.Sizeof(rateInfo)),
	)
	if ret == 0 {
		windows.CloseHandle(job)
		return nil, fmt.Errorf("set CPU rate control: %w", err)
	}
	bt.cpuActive = true

	// --- CPU affinity (pin to high cores) ---
	cpus := runtime.NumCPU()
	if cpus > 64 {
		cpus = 64 // affinity mask is 64 bits max (single processor group)
	}
	allowed := computeBootCores(cpus)
	if allowed > 0 && allowed < cpus {
		startCore := cpus - allowed
		var bootMask uintptr
		for i := startCore; i < cpus; i++ {
			bootMask |= 1 << uint(i)
		}
		var fullMask uintptr
		for i := 0; i < cpus; i++ {
			fullMask |= 1 << uint(i)
		}

		limitInfo := jobBasicLimitInfo{
			LimitFlags: jobObjectLimitAffinity,
			Affinity:   bootMask,
		}
		ret, _, affinityErr := procSetInformationJobObject.Call(
			uintptr(job),
			uintptr(jobObjectBasicLimitInfo),
			uintptr(unsafe.Pointer(&limitInfo)),
			uintptr(unsafe.Sizeof(limitInfo)),
		)
		if ret != 0 {
			bt.affinitySet = true
			bt.fullMask = fullMask

			// Integrity check: read back and verify the mask was applied
			if actual, ok := queryJobAffinity(job); ok {
				if actual != bootMask {
					log.Printf("boot throttle: affinity readback mismatch (set=0x%x, got=0x%x)", bootMask, actual)
				} else {
					log.Printf("boot throttle: pinned to cores %d-%d (mask=0x%x, %d cores)",
						startCore, cpus-1, bootMask, allowed)
				}
			}
		} else {
			// Non-fatal: game still launches, just without core isolation
			log.Printf("boot throttle: affinity set failed: %v (continuing without)", affinityErr)
		}
	}

	return bt, nil
}

// AssignProcess adds a process to the Job Object, applying CPU cap and affinity.
func (bt *BootThrottle) AssignProcess(pid int) error {
	if bt == nil || bt.job == 0 {
		return nil
	}

	handle, err := windows.OpenProcess(
		windows.PROCESS_SET_QUOTA|windows.PROCESS_TERMINATE,
		false, uint32(pid),
	)
	if err != nil {
		return fmt.Errorf("open process for job assignment: %w", err)
	}
	defer windows.CloseHandle(handle)

	ret, _, err := procAssignProcessToJobObject.Call(uintptr(bt.job), uintptr(handle))
	if ret == 0 {
		return fmt.Errorf("assign process to job: %w", err)
	}
	return nil
}

// Release lifts the CPU cap (set to 100%) and restores full-core affinity.
// Safe to call even if the process has already exited.
func (bt *BootThrottle) Release() error {
	if bt == nil || bt.job == 0 {
		return nil
	}

	// Lift CPU rate cap
	if bt.cpuActive {
		rateInfo := cpuRateControlInfo{
			ControlFlags: jobObjectCPURateControlEnable | jobObjectCPURateControlHardCap,
			CpuRate:      10000, // 100%
		}
		ret, _, err := procSetInformationJobObject.Call(
			uintptr(bt.job),
			uintptr(jobObjectCPURateControlInformation),
			uintptr(unsafe.Pointer(&rateInfo)),
			uintptr(unsafe.Sizeof(rateInfo)),
		)
		if ret == 0 {
			return fmt.Errorf("release CPU rate control: %w", err)
		}
		bt.cpuActive = false
	}

	// Restore full affinity
	if bt.affinitySet {
		limitInfo := jobBasicLimitInfo{
			LimitFlags: jobObjectLimitAffinity,
			Affinity:   bt.fullMask,
		}
		ret, _, err := procSetInformationJobObject.Call(
			uintptr(bt.job),
			uintptr(jobObjectBasicLimitInfo),
			uintptr(unsafe.Pointer(&limitInfo)),
			uintptr(unsafe.Sizeof(limitInfo)),
		)
		if ret == 0 {
			log.Printf("boot throttle: release affinity failed: %v", err)
		} else {
			// Integrity check: verify full affinity was restored
			if actual, ok := queryJobAffinity(bt.job); ok && actual != bt.fullMask {
				log.Printf("boot throttle: affinity release verification failed (expected=0x%x, got=0x%x)",
					bt.fullMask, actual)
			}
		}
		bt.affinitySet = false
	}

	log.Printf("boot throttle: released (CPU cap + affinity)")
	return nil
}

// Close releases the Job Object handle.
func (bt *BootThrottle) Close() {
	if bt != nil && bt.job != 0 {
		windows.CloseHandle(bt.job)
		bt.job = 0
	}
}

// queryJobAffinity reads the current affinity mask from the Job Object.
func queryJobAffinity(job windows.Handle) (uintptr, bool) {
	var info jobBasicLimitInfo
	var returnLen uint32
	ret, _, _ := procQueryInformationJobObject.Call(
		uintptr(job),
		uintptr(jobObjectBasicLimitInfo),
		uintptr(unsafe.Pointer(&info)),
		uintptr(unsafe.Sizeof(info)),
		uintptr(unsafe.Pointer(&returnLen)),
	)
	if ret == 0 {
		return 0, false
	}
	return info.Affinity, true
}

// computeBootCores decides how many cores to allocate for the JVM during boot.
// Returns 0 if the machine has too few cores to meaningfully partition.
func computeBootCores(cpus int) int {
	if cpus < 4 {
		return 0 // not enough to partition
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

// bootCPUCap returns a reasonable CPU cap percentage based on core count.
func bootCPUCap() int {
	cpus := runtime.NumCPU()
	switch {
	case cpus >= 16:
		return 50
	case cpus >= 8:
		return 60
	case cpus >= 4:
		return 70
	default:
		return 80
	}
}
