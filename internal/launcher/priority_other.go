//go:build !windows

package launcher

import "syscall"

// promoteProcess resets a process from low priority (nice 10) back to default (nice 0).
func promoteProcess(pid int) error {
	return syscall.Setpriority(syscall.PRIO_PROCESS, pid, 0)
}
