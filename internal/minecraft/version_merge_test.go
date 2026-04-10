package minecraft

import "testing"

func TestMergeVersionsMergesJavaVersionFieldsIndependently(t *testing.T) {
	parent := &VersionJSON{
		ID: "parent",
		JavaVersion: JavaVersion{
			Component:    "jre-legacy",
			MajorVersion: 8,
		},
	}
	child := &VersionJSON{
		ID: "child",
		JavaVersion: JavaVersion{
			MajorVersion: 25,
		},
	}

	merged := mergeVersions(parent, child)

	if merged.JavaVersion.Component != "jre-legacy" {
		t.Fatalf("expected inherited component %q, got %q", "jre-legacy", merged.JavaVersion.Component)
	}
	if merged.JavaVersion.MajorVersion != 25 {
		t.Fatalf("expected overridden major version 25, got %d", merged.JavaVersion.MajorVersion)
	}
}
