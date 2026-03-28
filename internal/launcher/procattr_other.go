//go:build !windows

package launcher

import "os/exec"

func setProcAttr(cmd *exec.Cmd) {
	// No special process attributes needed on non-Windows
}
