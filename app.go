package main

import (
	"context"
	"errors"
	"os"

	"github.com/mateoltd/croopor/internal/server"
	"github.com/wailsapp/wails/v2/pkg/runtime"
)

type App struct {
	ctx     context.Context
	version string
	server  *server.Server
}

func NewApp(version string, srv *server.Server) *App {
	return &App{version: version, server: srv}
}

func (a *App) startup(ctx context.Context) {
	a.ctx = ctx
}

func (a *App) Version() string {
	return a.version
}

func (a *App) BrowseDirectory(defaultPath string) (string, error) {
	if a.ctx == nil {
		return "", errors.New("wails app is not ready")
	}

	options := runtime.OpenDialogOptions{
		Title:                "Select your .minecraft folder",
		CanCreateDirectories: true,
	}

	if defaultPath != "" {
		if info, err := os.Stat(defaultPath); err == nil && info.IsDir() {
			options.DefaultDirectory = defaultPath
		}
	}

	return runtime.OpenDirectoryDialog(a.ctx, options)
}

func (a *App) OpenExternalURL(url string) {
	if a.ctx == nil || url == "" {
		return
	}
	runtime.BrowserOpenURL(a.ctx, url)
}

func (a *App) ShowNotice(title string, message string) error {
	if a.ctx == nil {
		return errors.New("wails app is not ready")
	}

	_, err := runtime.MessageDialog(a.ctx, runtime.MessageDialogOptions{
		Type:    runtime.InfoDialog,
		Title:   title,
		Message: message,
	})
	return err
}

func (a *App) StartInstallEvents(installID string) error {
	if a.ctx == nil {
		return errors.New("wails app is not ready")
	}
	return a.server.BridgeInstallEvents(installID, func(eventType string, data any) {
		runtime.EventsEmit(a.ctx, "croopor:install:"+installID+":"+eventType, data)
	})
}

func (a *App) StartLoaderInstallEvents(installID string) error {
	if a.ctx == nil {
		return errors.New("wails app is not ready")
	}
	return a.server.BridgeLoaderInstallEvents(installID, func(eventType string, data any) {
		runtime.EventsEmit(a.ctx, "croopor:loader-install:"+installID+":"+eventType, data)
	})
}

func (a *App) StartLaunchEvents(sessionID string) error {
	if a.ctx == nil {
		return errors.New("wails app is not ready")
	}
	return a.server.BridgeLaunchEvents(sessionID, func(eventType string, data any) {
		runtime.EventsEmit(a.ctx, "croopor:launch:"+sessionID+":"+eventType, data)
	})
}
