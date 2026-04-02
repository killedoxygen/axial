package main

import (
	"flag"
	"io/fs"
	"log"
	"runtime"

	"github.com/mateoltd/croopor/frontend"
	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/instance"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/server"
	appupdate "github.com/mateoltd/croopor/internal/update"
	"github.com/wailsapp/wails/v2"
	"github.com/wailsapp/wails/v2/pkg/options"
	"github.com/wailsapp/wails/v2/pkg/options/assetserver"
)

var version = "0.3.1"

func main() {
	mcDir := flag.String("mc-dir", "", "Override .minecraft directory path")
	flag.Parse()

	cfg, err := config.Load()
	if err != nil {
		log.Printf("Warning: could not load config: %v (using defaults)", err)
		cfg = config.DefaultConfig()
	}

	dir := *mcDir
	if dir == "" && cfg.MCDir != "" {
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

	instances, err := instance.Load()
	if err != nil {
		log.Printf("Warning: could not load instances: %v (starting empty)", err)
		instances = &instance.InstanceStore{}
	}

	staticFS, err := fs.Sub(frontend.Static, "static")
	if err != nil {
		log.Fatalf("Failed to load frontend: %v", err)
	}

	updater := appupdate.NewService(appupdate.DefaultManifestURL, runtime.GOOS, runtime.GOARCH)
	srv := server.NewServer(dir, version, cfg, instances, staticFS, updater)
	app := NewApp(version, srv)

	err = wails.Run(&options.App{
		Title:     "Croopor",
		Width:     1100,
		Height:    720,
		MinWidth:  960,
		MinHeight: 640,
		AssetServer: &assetserver.Options{
			Assets:  staticFS,
			Handler: srv.Handler(),
		},
		OnStartup: app.startup,
		Bind:      []interface{}{app},
	})
	if err != nil {
		log.Fatalf("Failed to start Wails app: %v", err)
	}
}
