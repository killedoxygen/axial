package minecraft

import (
	"os"
	"path/filepath"
	"testing"
)

func TestScanVersionsUsesResolvedJavaVersion(t *testing.T) {
	mcDir := t.TempDir()
	if err := CreateMinecraftDir(mcDir); err != nil {
		t.Fatalf("CreateMinecraftDir: %v", err)
	}

	parentDir := filepath.Join(VersionsDir(mcDir), "1.20.1")
	if err := os.MkdirAll(parentDir, 0o755); err != nil {
		t.Fatalf("mkdir parent: %v", err)
	}
	if err := os.WriteFile(filepath.Join(parentDir, "1.20.1.json"), []byte(`{
		"id":"1.20.1",
		"type":"release",
		"mainClass":"net.minecraft.client.main.Main",
		"assetIndex":{"id":"1.20.1"},
		"downloads":{},
		"javaVersion":{"component":"jre-legacy","majorVersion":8},
		"libraries":[]
	}`), 0o644); err != nil {
		t.Fatalf("write parent json: %v", err)
	}
	if err := os.WriteFile(filepath.Join(parentDir, "1.20.1.jar"), []byte("jar"), 0o644); err != nil {
		t.Fatalf("write parent jar: %v", err)
	}

	childDir := filepath.Join(VersionsDir(mcDir), "example-modpack")
	if err := os.MkdirAll(childDir, 0o755); err != nil {
		t.Fatalf("mkdir child: %v", err)
	}
	if err := os.WriteFile(filepath.Join(childDir, "example-modpack.json"), []byte(`{
		"id":"example-modpack",
		"type":"release",
		"inheritsFrom":"1.20.1",
		"mainClass":"net.minecraft.client.main.Main",
		"assetIndex":{"id":"1.20.1"},
		"downloads":{},
		"javaVersion":{"majorVersion":25},
		"libraries":[]
	}`), 0o644); err != nil {
		t.Fatalf("write child json: %v", err)
	}

	versions, err := ScanVersions(mcDir)
	if err != nil {
		t.Fatalf("ScanVersions: %v", err)
	}

	for _, version := range versions {
		if version.ID != "example-modpack" {
			continue
		}
		if version.JavaMajor != 25 {
			t.Fatalf("expected Java 25, got %d", version.JavaMajor)
		}
		if version.JavaComponent != "jre-legacy" {
			t.Fatalf("expected inherited component %q, got %q", "jre-legacy", version.JavaComponent)
		}
		return
	}

	t.Fatal("example-modpack not found in scan output")
}
