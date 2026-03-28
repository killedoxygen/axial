//go:build windows

package main

import (
	"log"
	"os"
	"os/signal"
	"syscall"

	webview2 "github.com/jchv/go-webview2"
)

// runApp creates a native WebView2 window and navigates to the app URL.
// Blocks until the window is closed, then exits the process.
func runApp(appURL string) {
	w := webview2.NewWithOptions(webview2.WebViewOptions{
		Debug:     false,
		AutoFocus: true,
		WindowOptions: webview2.WindowOptions{
			Title:  "Croopor",
			Width:  1100,
			Height: 720,
			IconId: 2,
			Center: true,
		},
	})
	if w == nil {
		log.Println("WebView2 unavailable — falling back to browser")
		fallbackBrowser(appURL)
		return
	}
	defer w.Destroy()

	w.SetSize(1100, 720, webview2.HintMin)
	w.Navigate(appURL)

	// Handle OS signals for clean shutdown
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigCh
		w.Terminate()
	}()

	w.Run()
	os.Exit(0)
}

func fallbackBrowser(url string) {
	cmd := newBrowserCmd(url)
	cmd.Run()
	// Keep the process alive for the server
	select {}
}
