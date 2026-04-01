package update

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"time"
)

const DefaultManifestURL = "https://mateoltd.github.io/croopor/updates/stable.json"
const maxManifestSize = 1 << 20

type Manifest struct {
	Channel     string                 `json:"channel"`
	Version     string                 `json:"version"`
	PublishedAt string                 `json:"published_at"`
	NotesURL    string                 `json:"notes_url"`
	Windows     map[string]PlatformBin `json:"windows"`
	Linux       map[string]PlatformBin `json:"linux"`
}

type PlatformBin struct {
	ReleaseURL      string `json:"release_url,omitempty"`
	AppInstallerURL string `json:"appinstaller_url,omitempty"`
	AppImageURL     string `json:"appimage_url,omitempty"`
}

type Result struct {
	CurrentVersion string `json:"current_version"`
	LatestVersion  string `json:"latest_version"`
	Available      bool   `json:"available"`
	Platform       string `json:"platform"`
	Arch           string `json:"arch"`
	Kind           string `json:"kind"`
	NotesURL       string `json:"notes_url"`
	ActionURL      string `json:"action_url"`
	ActionLabel    string `json:"action_label"`
	CheckedAt      string `json:"checked_at"`
}

type Service struct {
	client      *http.Client
	manifestURL string
	platform    string
	arch        string
	now         func() time.Time
}

func NewService(manifestURL, platform, arch string) *Service {
	return &Service{
		client:      &http.Client{},
		manifestURL: manifestURL,
		platform:    platform,
		arch:        arch,
		now:         time.Now,
	}
}

func (s *Service) Check(currentVersion string) (Result, error) {
	normalizedCurrent := normalizeVersion(currentVersion)
	result := Result{
		CurrentVersion: normalizedCurrent,
		Platform:       s.platform,
		Arch:           s.arch,
		Kind:           "none",
		CheckedAt:      s.now().UTC().Format(time.RFC3339),
	}

	current, err := parseStableVersion(currentVersion)
	if err != nil {
		return result, fmt.Errorf("invalid current version: %w", err)
	}

	manifest, err := s.fetchManifest()
	if err != nil {
		return result, err
	}

	latest, err := parseStableVersion(manifest.Version)
	if err != nil {
		return result, fmt.Errorf("invalid latest version: %w", err)
	}

	result.LatestVersion = normalizeVersion(manifest.Version)
	result.NotesURL = strings.TrimSpace(manifest.NotesURL)
	if compareVersions(latest, current) <= 0 {
		return result, nil
	}

	kind, actionURL, actionLabel := resolveAction(manifest, s.platform, s.arch)
	if actionURL == "" {
		return result, nil
	}

	result.Available = true
	result.Kind = kind
	result.ActionURL = actionURL
	result.ActionLabel = actionLabel
	return result, nil
}

func (s *Service) fetchManifest() (*Manifest, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, s.manifestURL, nil)
	if err != nil {
		return nil, fmt.Errorf("build update request: %w", err)
	}

	resp, err := s.client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("fetch update manifest: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("update manifest returned status %d", resp.StatusCode)
	}

	var manifest Manifest
	if err := json.NewDecoder(io.LimitReader(resp.Body, maxManifestSize)).Decode(&manifest); err != nil {
		return nil, fmt.Errorf("decode update manifest: %w", err)
	}
	if manifest.Channel != "" && manifest.Channel != "stable" {
		return nil, fmt.Errorf("unsupported update channel %q", manifest.Channel)
	}
	if strings.TrimSpace(manifest.Version) == "" {
		return nil, fmt.Errorf("update manifest missing version")
	}
	return &manifest, nil
}

func resolveAction(manifest *Manifest, platform, arch string) (string, string, string) {
	switch platform {
	case "windows":
		entry, ok := manifest.Windows[arch]
		if !ok {
			return "none", "", ""
		}
		actionURL := strings.TrimSpace(entry.ReleaseURL)
		if actionURL == "" {
			actionURL = strings.TrimSpace(manifest.NotesURL)
		}
		return "release-page", actionURL, "Open Windows download"
	case "linux":
		entry, ok := manifest.Linux[arch]
		if !ok {
			return "none", "", ""
		}
		actionURL := strings.TrimSpace(entry.AppImageURL)
		if actionURL == "" {
			return "none", "", ""
		}
		return "appimage", actionURL, "Download AppImage"
	default:
		return "none", "", ""
	}
}

type stableVersion struct {
	major int
	minor int
	patch int
}

func parseStableVersion(raw string) (stableVersion, error) {
	raw = normalizeVersion(raw)
	if raw == "" {
		return stableVersion{}, fmt.Errorf("empty version")
	}
	if strings.ContainsAny(raw, "+-") {
		return stableVersion{}, fmt.Errorf("pre-release versions are not supported")
	}
	parts := strings.Split(raw, ".")
	if len(parts) != 3 {
		return stableVersion{}, fmt.Errorf("expected version X.Y.Z or vX.Y.Z")
	}
	major, err := strconv.Atoi(parts[0])
	if err != nil {
		return stableVersion{}, fmt.Errorf("expected version X.Y.Z or vX.Y.Z")
	}
	minor, err := strconv.Atoi(parts[1])
	if err != nil {
		return stableVersion{}, fmt.Errorf("expected version X.Y.Z or vX.Y.Z")
	}
	patch, err := strconv.Atoi(parts[2])
	if err != nil {
		return stableVersion{}, fmt.Errorf("expected version X.Y.Z or vX.Y.Z")
	}
	v := stableVersion{major: major, minor: minor, patch: patch}
	return v, nil
}

func normalizeVersion(raw string) string {
	raw = strings.TrimSpace(raw)
	raw = strings.TrimPrefix(raw, "v")
	return raw
}

func compareVersions(a, b stableVersion) int {
	if a.major != b.major {
		if a.major > b.major {
			return 1
		}
		return -1
	}
	if a.minor != b.minor {
		if a.minor > b.minor {
			return 1
		}
		return -1
	}
	if a.patch != b.patch {
		if a.patch > b.patch {
			return 1
		}
		return -1
	}
	return 0
}
