package main

import (
	"os/exec"
	"runtime"
)

// newBrowserCmd returns a command that opens the given URL in the default browser.
func newBrowserCmd(url string) *exec.Cmd {
	switch runtime.GOOS {
	case "windows":
		return exec.Command("rundll32", "url.dll,FileProtocolHandler", url)
	case "darwin":
		return exec.Command("open", url)
	default:
		return exec.Command("xdg-open", url)
	}
}
