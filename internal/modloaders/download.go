package modloaders

import (
	"net/http"
	"os"
	"path/filepath"
	"sync"
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

	resolvable := make([]minecraft.Library, 0, len(filtered))
	for _, lib := range filtered {
		libPath, libURL, _ := minecraft.ResolveLibDownload(lib, mcDir)
		if libPath == "" || libURL == "" {
			continue
		}
		resolvable = append(resolvable, lib)
	}

	type dlJob struct {
		path string
		url  string
		sha1 string
		name string
	}

	var jobs []dlJob
	for _, lib := range resolvable {
		libPath, libURL, libSHA1 := minecraft.ResolveLibDownload(lib, mcDir)
		if !minecraft.FileExistsWithSHA1(libPath, libSHA1) {
			jobs = append(jobs, dlJob{path: libPath, url: libURL, sha1: libSHA1, name: filepath.Base(libPath)})
		}
	}

	total := len(jobs)
	if total > 0 {
		var mu sync.Mutex
		var completed int
		var dlErr error
		sem := make(chan struct{}, 4)
		var wg sync.WaitGroup

		for _, job := range jobs {
			wg.Add(1)
			sem <- struct{}{}
			go func(j dlJob) {
				defer wg.Done()
				defer func() { <-sem }()

				mu.Lock()
				failed := dlErr != nil
				mu.Unlock()
				if failed {
					return
				}

				if err := os.MkdirAll(filepath.Dir(j.path), 0755); err != nil {
					mu.Lock()
					if dlErr == nil {
						dlErr = err
					}
					mu.Unlock()
					return
				}
				if err := minecraft.DownloadFile(DefaultClient, j.url, j.path, j.sha1); err != nil {
					mu.Lock()
					if dlErr == nil {
						dlErr = err
					}
					mu.Unlock()
					return
				}

				mu.Lock()
				completed++
				progress <- Progress{
					Phase:   "loader_libraries",
					Current: completed,
					Total:   total,
					Detail:  j.name,
				}
				mu.Unlock()
			}(job)
		}

		wg.Wait()

		if dlErr != nil {
			return dlErr
		}
	}
	return nil
}
