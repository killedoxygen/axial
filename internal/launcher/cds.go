package launcher

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
)

// cdsGenerating prevents concurrent archive generation for the same path.
var cdsGenerating sync.Map

// CDSArchivePath returns the path to the CDS archive for a version.
func CDSArchivePath(configDir, versionID string) string {
	return filepath.Join(configDir, "cds", versionID+".jsa")
}

// CDSArchiveExists checks if a CDS archive exists.
func CDSArchiveExists(configDir, versionID string) bool {
	_, err := os.Stat(CDSArchivePath(configDir, versionID))
	return err == nil
}

// GenerateCDSArchive runs a one-time JVM class data sharing dump.
// The archive speeds up subsequent launches by caching class metadata.
// Concurrent calls for the same archive path are safely deduplicated.
func GenerateCDSArchive(javaPath, classpath, archivePath string) error {
	// Prevent concurrent generation of the same archive
	if _, loaded := cdsGenerating.LoadOrStore(archivePath, true); loaded {
		return nil
	}
	defer cdsGenerating.Delete(archivePath)

	dir := filepath.Dir(archivePath)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("creating CDS dir: %w", err)
	}

	cmd := exec.Command(javaPath,
		"-Xshare:dump",
		"-XX:SharedArchiveFile="+archivePath,
		"-cp", classpath,
		"-version",
	)
	output, err := cmd.CombinedOutput()
	if err != nil {
		// Clean up partial archive
		os.Remove(archivePath)
		return fmt.Errorf("CDS dump failed: %w\noutput: %s", err, string(output))
	}
	return nil
}

// InvalidateCDSArchive removes a CDS archive for a version.
func InvalidateCDSArchive(configDir, versionID string) {
	path := CDSArchivePath(configDir, versionID)
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		log.Printf("failed to remove CDS archive %s: %v", path, err)
	}
}
