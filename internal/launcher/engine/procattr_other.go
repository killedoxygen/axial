//go:build !windows

package engine

import (
	"os/exec"
	"syscall"
)

func setProcAttr(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setpgid: true,
	}
}

// setLowPriority sets the process to nice value 10 after it has started.
func setLowPriority(pid int) {
	_ = syscall.Setpriority(syscall.PRIO_PROCESS, pid, 10)
}
