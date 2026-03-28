package minecraft

import (
	"encoding/json"
	"fmt"
	"net/http"
	"sync"
	"time"
)

const manifestURL = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"

type VersionManifest struct {
	Latest   LatestVersions  `json:"latest"`
	Versions []ManifestEntry `json:"versions"`
}

type LatestVersions struct {
	Release  string `json:"release"`
	Snapshot string `json:"snapshot"`
}

type ManifestEntry struct {
	ID              string `json:"id"`
	Type            string `json:"type"`
	URL             string `json:"url"`
	Time            string `json:"time"`
	ReleaseTime     string `json:"releaseTime"`
	SHA1            string `json:"sha1"`
	ComplianceLevel int    `json:"complianceLevel"`
}

var (
	cachedManifest *VersionManifest
	manifestMu     sync.Mutex
	manifestTime   time.Time
)

// FetchVersionManifest fetches the Mojang version manifest.
// Results are cached for 10 minutes.
func FetchVersionManifest() (*VersionManifest, error) {
	manifestMu.Lock()
	defer manifestMu.Unlock()

	if cachedManifest != nil && time.Since(manifestTime) < 10*time.Minute {
		return cachedManifest, nil
	}

	client := &http.Client{Timeout: 15 * time.Second}
	resp, err := client.Get(manifestURL)
	if err != nil {
		if cachedManifest != nil {
			return cachedManifest, nil // return stale cache on network error
		}
		return nil, fmt.Errorf("fetching version manifest: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		if cachedManifest != nil {
			return cachedManifest, nil
		}
		return nil, fmt.Errorf("manifest returned status %d", resp.StatusCode)
	}

	var m VersionManifest
	if err := json.NewDecoder(resp.Body).Decode(&m); err != nil {
		return nil, fmt.Errorf("parsing manifest: %w", err)
	}

	cachedManifest = &m
	manifestTime = time.Now()
	return &m, nil
}
