package update

import (
	"io"
	"net/http"
	"strings"
	"testing"
	"time"
)

type roundTripFunc func(*http.Request) (*http.Response, error)

func (fn roundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return fn(req)
}

func newTestService(body string, platform string, arch string) *Service {
	svc := NewService("https://example.test/updates/stable.json", platform, arch)
	svc.client = &http.Client{
		Transport: roundTripFunc(func(req *http.Request) (*http.Response, error) {
			return &http.Response{
				StatusCode: http.StatusOK,
				Header:     http.Header{"Content-Type": []string{"application/json"}},
				Body:       io.NopCloser(strings.NewReader(body)),
			}, nil
		}),
	}
	return svc
}

func TestCheckWindowsReleasePage(t *testing.T) {
	updater := newTestService(`{
			"channel":"stable",
			"version":"v1.2.3",
			"published_at":"2026-04-01T10:00:00Z",
			"notes_url":"https://example.test/release/v1.2.3",
			"windows":{"amd64":{"release_url":"https://example.test/windows"}},
			"linux":{"amd64":{"appimage_url":"https://example.test/linux.AppImage"}}
		}`, "windows", "amd64")
	updater.now = func() time.Time { return time.Date(2026, 4, 1, 12, 0, 0, 0, time.UTC) }

	result, err := updater.Check("v1.2.2")
	if err != nil {
		t.Fatalf("Check returned error: %v", err)
	}
	if !result.Available {
		t.Fatalf("expected update to be available")
	}
	if result.Kind != "release-page" {
		t.Fatalf("kind = %q, want release-page", result.Kind)
	}
	if result.ActionURL != "https://example.test/windows" {
		t.Fatalf("action url = %q", result.ActionURL)
	}
	if result.CurrentVersion != "1.2.2" {
		t.Fatalf("current version = %q", result.CurrentVersion)
	}
}

func TestCheckLinuxAppImage(t *testing.T) {
	updater := newTestService(`{
			"channel":"stable",
			"version":"1.2.3",
			"notes_url":"https://example.test/release/v1.2.3",
			"windows":{"amd64":{"release_url":"https://example.test/windows"}},
			"linux":{"amd64":{"appimage_url":"https://example.test/linux.AppImage"}}
		}`, "linux", "amd64")
	result, err := updater.Check("1.2.2")
	if err != nil {
		t.Fatalf("Check returned error: %v", err)
	}
	if result.Kind != "appimage" {
		t.Fatalf("kind = %q, want appimage", result.Kind)
	}
	if result.ActionURL != "https://example.test/linux.AppImage" {
		t.Fatalf("action url = %q", result.ActionURL)
	}
}

func TestCheckLinuxIgnoresBlankAppImageURL(t *testing.T) {
	updater := newTestService(`{
			"channel":"stable",
			"version":"1.2.3",
			"notes_url":"https://example.test/release/v1.2.3",
			"linux":{"amd64":{"appimage_url":"   "}}
		}`, "linux", "amd64")
	result, err := updater.Check("1.2.2")
	if err != nil {
		t.Fatalf("Check returned error: %v", err)
	}
	if result.Available {
		t.Fatalf("expected no update to be available without a usable appimage url")
	}
	if result.Kind != "none" {
		t.Fatalf("kind = %q, want none", result.Kind)
	}
}

func TestCheckNoUpdateWhenCurrentIsLatest(t *testing.T) {
	updater := newTestService(`{
			"channel":"stable",
			"version":"1.2.3",
			"notes_url":"https://example.test/release/v1.2.3",
			"windows":{"amd64":{"release_url":"https://example.test/windows"}}
		}`, "windows", "amd64")
	result, err := updater.Check("v1.2.3")
	if err != nil {
		t.Fatalf("Check returned error: %v", err)
	}
	if result.Available {
		t.Fatalf("expected no update to be available")
	}
	if result.Kind != "none" {
		t.Fatalf("kind = %q, want none", result.Kind)
	}
}

func TestCheckUnsupportedPlatform(t *testing.T) {
	updater := newTestService(`{
			"channel":"stable",
			"version":"1.2.3",
			"notes_url":"https://example.test/release/v1.2.3"
		}`, "darwin", "arm64")
	result, err := updater.Check("1.2.2")
	if err != nil {
		t.Fatalf("Check returned error: %v", err)
	}
	if result.Available {
		t.Fatalf("expected no update to be available")
	}
}

func TestCheckRejectsBadManifest(t *testing.T) {
	updater := newTestService(`{"channel":"stable","version":"wat"}`, "windows", "amd64")
	if _, err := updater.Check("1.2.2"); err == nil {
		t.Fatal("expected malformed manifest to fail")
	}
}

func TestCheckRejectsTrailingVersionGarbage(t *testing.T) {
	updater := newTestService(`{"channel":"stable","version":"1.2.3"}`, "windows", "amd64")
	if _, err := updater.Check("1.2.3foo"); err == nil {
		t.Fatal("expected partially parsed version to fail")
	}
}

func TestCheckRejectsTooManyVersionSegments(t *testing.T) {
	updater := newTestService(`{"channel":"stable","version":"1.2.3.4"}`, "windows", "amd64")
	if _, err := updater.Check("1.2.2"); err == nil {
		t.Fatal("expected invalid manifest version to fail")
	}
}
