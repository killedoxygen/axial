package modloaders

import (
	"os"
	"path/filepath"

	"github.com/mateoltd/croopor/internal/minecraft"
)

// CombinedInstall holds the progress channel for a two-phase install:
// 1) loader-specific install (write version JSON + loader libraries)
// 2) base game install via the existing downloader (client JAR, vanilla libs, assets)
type CombinedInstall struct {
	ProgressCh chan minecraft.DownloadProgress
}

// NewCombinedInstall creates a combined installer with a buffered progress channel.
func NewCombinedInstall() *CombinedInstall {
	return &CombinedInstall{
		ProgressCh: make(chan minecraft.DownloadProgress, 64),
	}
}

// Run executes the full install: loader-specific setup then base game download.
// It closes ProgressCh when done.
func (ci *CombinedInstall) Run(loader Loader, mcDir, gameVersion, loaderVersion string) {
	defer close(ci.ProgressCh)

	if loader.NeedsBaseGameFirst() {
		ci.runBaseGameFirst(loader, mcDir, gameVersion, loaderVersion)
	} else {
		ci.runLoaderFirst(loader, mcDir, gameVersion, loaderVersion)
	}
}

// runLoaderFirst installs the loader (writes JSON + libs), then the base game.
// Used by Fabric and Quilt which don't need Java for installation.
func (ci *CombinedInstall) runLoaderFirst(loader Loader, mcDir, gameVersion, loaderVersion string) {
	// Phase 1: Loader install (writes version JSON + downloads loader libraries)
	result, err := ci.executeLoaderInstall(loader, mcDir, gameVersion, loaderVersion)
	if err != nil {
		ci.sendError(err.Error())
		return
	}

	// Phase 2: Base game install (vanilla version if not yet installed)
	if !ci.isBaseGameInstalled(mcDir, gameVersion) {
		if !ci.runDownloader(mcDir, gameVersion) {
			return
		}
	}

	minecraft.EnsureLauncherProfiles(mcDir, result.VersionID)
	ci.ProgressCh <- minecraft.DownloadProgress{Phase: "done", Current: 1, Total: 1, Done: true}
}

// runBaseGameFirst installs the vanilla base game first (to get Java runtime),
// then the loader. Used by Forge and NeoForge which need Java for processors.
func (ci *CombinedInstall) runBaseGameFirst(loader Loader, mcDir, gameVersion, loaderVersion string) {
	// Phase 1: Base game
	if !ci.isBaseGameInstalled(mcDir, gameVersion) {
		if !ci.runDownloader(mcDir, gameVersion) {
			return
		}
	}

	// Phase 2: Loader install (processors use Java from the base game install)
	result, err := ci.executeLoaderInstall(loader, mcDir, gameVersion, loaderVersion)
	if err != nil {
		ci.sendError(err.Error())
		return
	}

	minecraft.EnsureLauncherProfiles(mcDir, result.VersionID)
	ci.ProgressCh <- minecraft.DownloadProgress{Phase: "done", Current: 1, Total: 1, Done: true}
}

func (ci *CombinedInstall) executeLoaderInstall(loader Loader, mcDir, gameVersion, loaderVersion string) (*InstallResult, error) {
	loaderCh := make(chan Progress, 64)
	var result *InstallResult
	var installErr error
	done := make(chan struct{})

	go func() {
		defer close(done)
		defer close(loaderCh)
		result, installErr = loader.Install(mcDir, gameVersion, loaderVersion, loaderCh)
	}()

	for p := range loaderCh {
		ci.ProgressCh <- minecraft.DownloadProgress{
			Phase:   p.Phase,
			Current: p.Current,
			Total:   p.Total,
			File:    p.Detail,
			Error:   p.Error,
		}
		if p.Error != "" {
			<-done
			return nil, &loaderError{p.Error}
		}
	}
	<-done

	return result, installErr
}

// runDownloader runs the existing InstallVersion for a version ID, forwarding
// progress but suppressing the final "done" event (since we have more phases).
func (ci *CombinedInstall) runDownloader(mcDir, versionID string) bool {
	dl := minecraft.NewDownloader(mcDir)
	go dl.InstallVersion(versionID, "")
	for p := range dl.ProgressCh {
		if p.Phase == "error" {
			ci.ProgressCh <- p
			return false
		}
		if p.Done {
			continue // Suppress intermediate "done"
		}
		ci.ProgressCh <- p
	}
	return true
}

func (ci *CombinedInstall) isBaseGameInstalled(mcDir, gameVersion string) bool {
	jsonPath := filepath.Join(minecraft.VersionsDir(mcDir), gameVersion, gameVersion+".json")
	jarPath := filepath.Join(minecraft.VersionsDir(mcDir), gameVersion, gameVersion+".jar")
	markerPath := filepath.Join(minecraft.VersionsDir(mcDir), gameVersion, ".incomplete")

	_, jsonErr := os.Stat(jsonPath)
	_, jarErr := os.Stat(jarPath)
	_, markerErr := os.Stat(markerPath)

	return jsonErr == nil && jarErr == nil && os.IsNotExist(markerErr)
}

func (ci *CombinedInstall) sendError(msg string) {
	ci.ProgressCh <- minecraft.DownloadProgress{Phase: "error", Error: msg, Done: true}
}

type loaderError struct {
	msg string
}

func (e *loaderError) Error() string { return e.msg }
