package main

import (
	"flag"
	"fmt"
	"io/fs"
	"log"
	"net"
	"net/http"

	"github.com/mateoltd/mc-paralauncher/frontend"
	"github.com/mateoltd/mc-paralauncher/internal/config"
	"github.com/mateoltd/mc-paralauncher/internal/minecraft"
	"github.com/mateoltd/mc-paralauncher/internal/server"
)

var version = "1.0.0"

func main() {
	port := flag.Int("port", 0, "HTTP server port (0 = auto)")
	mcDir := flag.String("mc-dir", "", "Override .minecraft directory path")
	flag.Parse()

	// Load config
	cfg, err := config.Load()
	if err != nil {
		log.Printf("Warning: could not load config: %v (using defaults)", err)
		cfg = config.DefaultConfig()
	}

	// Detect Minecraft installation
	dir := *mcDir
	if dir == "" {
		dir, err = minecraft.DetectMinecraftDir()
		if err != nil {
			log.Fatalf("Minecraft installation not found: %v\n"+
				"Make sure Minecraft is installed, or use --mc-dir to specify the path.", err)
		}
	}

	if err := minecraft.ValidateInstallation(dir); err != nil {
		log.Fatalf("Invalid Minecraft installation at %s: %v", dir, err)
	}

	log.Printf("Minecraft directory: %s", dir)

	// Set up embedded frontend filesystem
	staticFS, err := fs.Sub(frontend.Static, "static")
	if err != nil {
		log.Fatalf("Failed to load frontend: %v", err)
	}

	// Create HTTP server
	srv := server.NewServer(dir, cfg, staticFS)

	// Bind to a port (0 = random available port)
	addr := fmt.Sprintf("127.0.0.1:%d", *port)
	listener, err := net.Listen("tcp", addr)
	if err != nil {
		log.Fatalf("Failed to start server: %v", err)
	}

	actualAddr := listener.Addr().String()
	appURL := fmt.Sprintf("http://%s", actualAddr)
	log.Printf("ParaLauncher %s serving at %s", version, appURL)

	// Start HTTP server in background
	go func() {
		if err := http.Serve(listener, srv.Handler()); err != nil {
			log.Fatalf("Server error: %v", err)
		}
	}()

	// Open the app window (platform-specific)
	runApp(appURL)
}
