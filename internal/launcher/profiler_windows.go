//go:build windows

package launcher

import (
	"encoding/json"
	"sync"
	"unsafe"

	"golang.org/x/sys/windows"
)

var (
	modpsapi                    = windows.NewLazySystemDLL("psapi.dll")
	procGetProcessMemoryInfo    = modpsapi.NewProc("GetProcessMemoryInfo")

	modntdll                    = windows.NewLazySystemDLL("ntdll.dll")
	procNtQuerySystemInformation = modntdll.NewProc("NtQuerySystemInformation")
	procNtQueryInformationProcess = modntdll.NewProc("NtQueryInformationProcess")
)

// PROCESS_MEMORY_COUNTERS_EX
type processMemoryCounters struct {
	CB                         uint32
	PageFaultCount             uint32
	PeakWorkingSetSize         uintptr
	WorkingSetSize             uintptr
	QuotaPeakPagedPoolUsage    uintptr
	QuotaPagedPoolUsage        uintptr
	QuotaPeakNonPagedPoolUsage uintptr
	QuotaNonPagedPoolUsage     uintptr
	PagefileUsage              uintptr
	PeakPagefileUsage          uintptr
	PrivateUsage               uintptr
}

// IO_COUNTERS from GetProcessIoCounters
type ioCounters struct {
	ReadOperationCount  uint64
	WriteOperationCount uint64
	OtherOperationCount uint64
	ReadTransferCount   uint64
	WriteTransferCount  uint64
	OtherTransferCount  uint64
}

type memoryStatusEx struct {
	Length               uint32
	MemoryLoad           uint32
	TotalPhys            uint64
	AvailPhys            uint64
	TotalPageFile        uint64
	AvailPageFile        uint64
	TotalVirtual         uint64
	AvailVirtual         uint64
	AvailExtendedVirtual uint64
}

var (
	procGlobalMemoryStatusEx  = modkernel32.NewProc("GlobalMemoryStatusEx")
	procGetSystemTimes        = modkernel32.NewProc("GetSystemTimes")
	procGetProcessIoCounters  = modkernel32.NewProc("GetProcessIoCounters")
)

// Per-process CPU delta state (guarded by cpuMu since profiler runs in a goroutine).
var (
	cpuMu             sync.Mutex
	lastProcKernel    uint64
	lastProcUser      uint64
	lastProcWallTicks uint64 // wall-clock in 100ns units
)

// System CPU delta state
var (
	lastIdleTime   uint64
	lastKernelTime uint64
	lastUserTime   uint64
)

func readProcessStats(pid int) processStats {
	var ps processStats

	handle, err := windows.OpenProcess(
		windows.PROCESS_QUERY_INFORMATION|windows.PROCESS_VM_READ,
		false, uint32(pid),
	)
	if err != nil {
		return ps
	}
	defer windows.CloseHandle(handle)

	// Memory info
	var memInfo processMemoryCounters
	memInfo.CB = uint32(unsafe.Sizeof(memInfo))
	ret, _, _ := procGetProcessMemoryInfo.Call(
		uintptr(handle),
		uintptr(unsafe.Pointer(&memInfo)),
		uintptr(memInfo.CB),
	)
	if ret != 0 {
		ps.rss = int64(memInfo.WorkingSetSize)
		ps.virt = int64(memInfo.PrivateUsage)
	}

	// Thread count via toolhelp snapshot
	snapshot, err := windows.CreateToolhelp32Snapshot(windows.TH32CS_SNAPTHREAD, 0)
	if err == nil {
		defer windows.CloseHandle(snapshot)
		var te windows.ThreadEntry32
		te.Size = uint32(unsafe.Sizeof(te))
		err = windows.Thread32First(snapshot, &te)
		for err == nil {
			if te.OwnerProcessID == uint32(pid) {
				ps.threads++
			}
			err = windows.Thread32Next(snapshot, &te)
		}
	}

	// Per-process CPU via GetProcessTimes delta
	var creationTime, exitTime, kernelTime, userTime windows.Filetime
	err = windows.GetProcessTimes(handle, &creationTime, &exitTime, &kernelTime, &userTime)
	if err == nil {
		k := uint64(kernelTime.HighDateTime)<<32 | uint64(kernelTime.LowDateTime)
		u := uint64(userTime.HighDateTime)<<32 | uint64(userTime.LowDateTime)

		// Wall clock in 100ns units (FILETIME epoch)
		var now windows.Filetime
		windows.GetSystemTimeAsFileTime(&now)
		wall := uint64(now.HighDateTime)<<32 | uint64(now.LowDateTime)

		cpuMu.Lock()
		if lastProcWallTicks != 0 {
			cpuDelta := (k - lastProcKernel) + (u - lastProcUser)
			wallDelta := wall - lastProcWallTicks
			if wallDelta > 0 {
				// cpuDelta and wallDelta are both in 100ns units.
				// Result is % of one core; multiply by nothing since
				// a single core at 100% = wallDelta of CPU time.
				ps.cpuPct = float64(cpuDelta) / float64(wallDelta) * 100.0
			}
		}
		lastProcKernel = k
		lastProcUser = u
		lastProcWallTicks = wall
		cpuMu.Unlock()
	}

	// Disk I/O via GetProcessIoCounters
	var ioc ioCounters
	ret, _, _ = procGetProcessIoCounters.Call(
		uintptr(handle),
		uintptr(unsafe.Pointer(&ioc)),
	)
	if ret != 0 {
		ps.ioReadBytes = int64(ioc.ReadTransferCount)
		ps.ioWriteBytes = int64(ioc.WriteTransferCount)
		ps.ioReadOps = int64(ioc.ReadOperationCount)
		ps.ioWriteOps = int64(ioc.WriteOperationCount)
	}

	return ps
}

func readSystemStats() (cpuPct float64, freeMemBytes int64) {
	// System memory
	var memStatus memoryStatusEx
	memStatus.Length = uint32(unsafe.Sizeof(memStatus))
	ret, _, _ := procGlobalMemoryStatusEx.Call(uintptr(unsafe.Pointer(&memStatus)))
	if ret != 0 {
		freeMemBytes = int64(memStatus.AvailPhys)
	}

	// System CPU via GetSystemTimes delta
	var idleTime, kernelTime, userTime windows.Filetime
	ret, _, _ = procGetSystemTimes.Call(
		uintptr(unsafe.Pointer(&idleTime)),
		uintptr(unsafe.Pointer(&kernelTime)),
		uintptr(unsafe.Pointer(&userTime)),
	)
	if ret != 0 {
		idle := uint64(idleTime.HighDateTime)<<32 | uint64(idleTime.LowDateTime)
		kernel := uint64(kernelTime.HighDateTime)<<32 | uint64(kernelTime.LowDateTime)
		user := uint64(userTime.HighDateTime)<<32 | uint64(userTime.LowDateTime)

		if lastKernelTime != 0 {
			idleDelta := idle - lastIdleTime
			kernelDelta := kernel - lastKernelTime
			userDelta := user - lastUserTime
			total := kernelDelta + userDelta
			if total > 0 {
				cpuPct = float64(total-idleDelta) / float64(total) * 100.0
			}
		}
		lastIdleTime = idle
		lastKernelTime = kernel
		lastUserTime = user
	}

	return cpuPct, freeMemBytes
}

func marshalJSON(v any) ([]byte, error) {
	return json.MarshalIndent(v, "", "  ")
}
