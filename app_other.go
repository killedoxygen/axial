//go:build !windows

package main

import (
	"fmt"
	"os"
	"os/signal"
	"syscall"
)

// runApp on non-Windows opens a browser and blocks until interrupted.
// This is the development/fallback mode.
func runApp(appURL string) {
	fmt.Printf("\n  Croopor %s\n  %s\n\n", version, appURL)

	cmd := newBrowserCmd(appURL)
	cmd.Start()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	<-sigCh

	fmt.Println("\nShutting down...")
	os.Exit(0)
}
