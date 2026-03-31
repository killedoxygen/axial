//go:build windows

package system

import (
	"syscall"
	"unsafe"
)

var (
	kernel32               = syscall.NewLazyDLL("kernel32.dll")
	globalMemoryStatusExFn = kernel32.NewProc("GlobalMemoryStatusEx")
)

type memoryStatusEx struct {
	dwLength                uint32
	dwMemoryLoad            uint32
	ullTotalPhys            uint64
	ullAvailPhys            uint64
	ullTotalPageFile        uint64
	ullAvailPageFile        uint64
	ullTotalVirtual         uint64
	ullAvailVirtual         uint64
	ullAvailExtendedVirtual uint64
}

func totalMemoryBytes() (uint64, error) {
	var ms memoryStatusEx
	ms.dwLength = uint32(unsafe.Sizeof(ms))
	r1, _, err := globalMemoryStatusExFn.Call(uintptr(unsafe.Pointer(&ms)))
	if r1 == 0 {
		return 0, err
	}
	return ms.ullTotalPhys, nil
}
