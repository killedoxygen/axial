package minecraft

import (
	"path/filepath"
	"testing"
)

func TestResolveNativeDownloadFallsBackToMavenCoordinate(t *testing.T) {
	t.Parallel()

	lib := Library{
		Name:    "org.lwjgl.lwjgl:lwjgl-platform:2.9.1-nightly-20130708-debug3",
		URL:     "https://repo.example.com/maven",
		Natives: map[string]string{"linux": "natives-linux"},
	}

	path, url, sha1 := resolveNativeDownload(lib, "/tmp/mc", Environment{OSName: "linux"})

	wantRel := filepath.Join(
		"org", "lwjgl", "lwjgl", "lwjgl-platform", "2.9.1-nightly-20130708-debug3",
		"lwjgl-platform-2.9.1-nightly-20130708-debug3-natives-linux.jar",
	)
	if got := filepath.Clean(path); got != filepath.Join(LibrariesDir("/tmp/mc"), wantRel) {
		t.Fatalf("resolveNativeDownload path = %q, want %q", got, filepath.Join(LibrariesDir("/tmp/mc"), wantRel))
	}
	if url != "https://repo.example.com/maven/"+filepath.ToSlash(wantRel) {
		t.Fatalf("resolveNativeDownload url = %q, want %q", url, "https://repo.example.com/maven/"+filepath.ToSlash(wantRel))
	}
	if sha1 != "" {
		t.Fatalf("resolveNativeDownload sha1 = %q, want empty", sha1)
	}
}

func TestResolveLibDownloadAllowsMavenFallbackWhenDownloadsMissing(t *testing.T) {
	t.Parallel()

	lib := Library{
		Name:    "org.lwjgl:lwjgl:3.3.3",
		Natives: map[string]string{"linux": "natives-linux"},
	}

	path, url, sha1 := ResolveLibDownload(lib, "/tmp/mc")

	wantRel := filepath.Join("org", "lwjgl", "lwjgl", "3.3.3", "lwjgl-3.3.3.jar")
	if got := filepath.Clean(path); got != filepath.Join(LibrariesDir("/tmp/mc"), wantRel) {
		t.Fatalf("ResolveLibDownload path = %q, want %q", got, filepath.Join(LibrariesDir("/tmp/mc"), wantRel))
	}
	if url != "https://libraries.minecraft.net/"+filepath.ToSlash(wantRel) {
		t.Fatalf("ResolveLibDownload url = %q, want %q", url, "https://libraries.minecraft.net/"+filepath.ToSlash(wantRel))
	}
	if sha1 != "" {
		t.Fatalf("ResolveLibDownload sha1 = %q, want empty", sha1)
	}
}

func TestResolveLibrariesKeepsMainAndNativeFallbacksWhenDownloadsMissing(t *testing.T) {
	t.Parallel()

	v := &VersionJSON{
		Libraries: []Library{
			{
				Name:    "org.lwjgl:lwjgl:3.3.3",
				Natives: map[string]string{"linux": "natives-linux"},
			},
		},
	}

	resolved, err := ResolveLibraries(v, "/tmp/mc", Environment{OSName: "linux"})
	if err != nil {
		t.Fatalf("ResolveLibraries error = %v", err)
	}
	if len(resolved) != 2 {
		t.Fatalf("ResolveLibraries len = %d, want 2", len(resolved))
	}

	wantNative := filepath.Join(LibrariesDir("/tmp/mc"), "org", "lwjgl", "lwjgl", "3.3.3", "lwjgl-3.3.3-natives-linux.jar")
	wantMain := filepath.Join(LibrariesDir("/tmp/mc"), "org", "lwjgl", "lwjgl", "3.3.3", "lwjgl-3.3.3.jar")

	if resolved[0].AbsPath != wantNative || !resolved[0].IsNative || resolved[0].Name != "org.lwjgl:lwjgl:3.3.3:natives-linux" {
		t.Fatalf("ResolveLibraries native = %#v", resolved[0])
	}
	if resolved[1].AbsPath != wantMain || resolved[1].IsNative || resolved[1].Name != "org.lwjgl:lwjgl:3.3.3" {
		t.Fatalf("ResolveLibraries main = %#v", resolved[1])
	}
}
