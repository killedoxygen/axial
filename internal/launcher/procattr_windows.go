//go:build windows

package launcher

import (
	"os/exec"
	"syscall"
)

const _BELOW_NORMAL_PRIORITY_CLASS = 0x00004000

func setProcAttr(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{
		CreationFlags: syscall.CREATE_NEW_PROCESS_GROUP | _BELOW_NORMAL_PRIORITY_CLASS,
	}
}

// setLowPriority is a no-op on Windows because the priority is set via CreationFlags.
func setLowPriority(pid int) {}
