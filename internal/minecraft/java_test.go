package minecraft

import (
	"path/filepath"
	"testing"
)

func TestRuntimeConfigCandidatesCoverLegacyAndModernLayouts(t *testing.T) {
	javaExe := filepath.Join("runtime", "jre-legacy", "bin", "javaw.exe")
	candidates := runtimeConfigCandidates(javaExe)

	expected := map[string]bool{
		filepath.Join("runtime", "jre-legacy", "lib", "jvm.cfg"):                 true,
		filepath.Join("runtime", "jre-legacy", "lib", "amd64", "jvm.cfg"):        true,
		filepath.Join("runtime", "jre-legacy", "jre", "lib", "jvm.cfg"):          true,
		filepath.Join("runtime", "jre-legacy", "jre", "lib", "amd64", "jvm.cfg"): true,
	}

	if len(candidates) != len(expected) {
		t.Fatalf("expected %d candidates, got %d", len(expected), len(candidates))
	}
	for _, candidate := range candidates {
		if !expected[candidate] {
			t.Fatalf("unexpected candidate path %q", candidate)
		}
		delete(expected, candidate)
	}
	if len(expected) > 0 {
		t.Fatalf("missing candidate paths: %v", expected)
	}
}
