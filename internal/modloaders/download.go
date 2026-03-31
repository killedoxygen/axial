package modloaders

import (
	"net/http"
	"os"
	"path/filepath"
	"time"

	"github.com/mateoltd/croopor/internal/minecraft"
)

// DefaultClient is a shared HTTP client for loader metadata and file downloads.
var DefaultClient = &http.Client{Timeout: 5 * time.Minute}

// DownloadLibraries downloads a list of Minecraft-format libraries into the
// shared libraries directory. It skips files that already exist with matching
// SHA1 hashes and reports progress on the provided channel.
func DownloadLibraries(libs []minecraft.Library, mcDir string, progress chan<- Progress) error {
	env := minecraft.DefaultEnvironment()
	filtered := minecraft.FilterLibraries(libs, env)

	total := len(filtered)
	for i, lib := range filtered {
		libPath, libURL, libSHA1 := minecraft.ResolveLibDownload(lib, mcDir)
		if libPath == "" || libURL == "" {
			continue
		}

		progress <- Progress{
			Phase:   "loader_libraries",
			Current: i + 1,
			Total:   total,
			Detail:  filepath.Base(libPath),
		}

		if minecraft.FileExistsWithSHA1(libPath, libSHA1) {
			continue
		}

		os.MkdirAll(filepath.Dir(libPath), 0755)
		if err := minecraft.DownloadFile(DefaultClient, libURL, libPath, libSHA1); err != nil {
			return err
		}
	}
	return nil
}
