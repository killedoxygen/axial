//go:build windows

package engine

import (
	"fmt"

	"golang.org/x/sys/windows"
)

// promoteProcess raises a process from BELOW_NORMAL to NORMAL priority.
func promoteProcess(pid int) error {
	handle, err := windows.OpenProcess(windows.PROCESS_SET_INFORMATION, false, uint32(pid))
	if err != nil {
		return fmt.Errorf("open process %d: %w", pid, err)
	}
	defer windows.CloseHandle(handle)
	return windows.SetPriorityClass(handle, windows.NORMAL_PRIORITY_CLASS)
}
