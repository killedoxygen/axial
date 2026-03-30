package main

import (
	"flag"
	"fmt"
	"io/fs"
	"log"
	"net"
	"net/http"

	"github.com/mateoltd/croopor/frontend"
	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/instance"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/server"
)

var version = "1.0.1"

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

	// Detect Minecraft installation — if not found, the UI will handle setup.
	dir := *mcDir
	if dir == "" && cfg.MCDir != "" {
		// Check config for a previously saved directory
		if err := minecraft.ValidateInstallation(cfg.MCDir); err == nil {
			dir = cfg.MCDir
		}
	}
	if dir == "" {
		dir, _ = minecraft.DetectMinecraftDir()
	}
	if dir != "" {
		if err := minecraft.ValidateInstallation(dir); err != nil {
			log.Printf("Invalid Minecraft installation at %s: %v", dir, err)
			dir = ""
		}
	}

	if dir != "" {
		log.Printf("Minecraft directory: %s", dir)
	} else {
		log.Printf("Minecraft directory not found — setup required")
	}

	// Load instance store
	instances, err := instance.Load()
	if err != nil {
		log.Printf("Warning: could not load instances: %v (starting empty)", err)
		instances = &instance.InstanceStore{}
	}

	// Set up embedded frontend filesystem
	staticFS, err := fs.Sub(frontend.Static, "static")
	if err != nil {
		log.Fatalf("Failed to load frontend: %v", err)
	}

	// Create HTTP server
	srv := server.NewServer(dir, cfg, instances, staticFS)

	// Bind to a port (0 = random available port)
	addr := fmt.Sprintf("127.0.0.1:%d", *port)
	listener, err := net.Listen("tcp", addr)
	if err != nil {
		log.Fatalf("Failed to start server: %v", err)
	}

	actualAddr := listener.Addr().String()
	appURL := fmt.Sprintf("http://%s", actualAddr)
	log.Printf("Croopor %s serving at %s", version, appURL)

	// Start HTTP server in background
	go func() {
		if err := http.Serve(listener, srv.Handler()); err != nil {
			log.Fatalf("Server error: %v", err)
		}
	}()

	// Open the app window (platform-specific)
	runApp(appURL)
}
