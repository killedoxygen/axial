package modloaders

import (
	"archive/zip"
	"bytes"
	"fmt"
	"io"
)

// extractInstallerJSONs reads version.json and install_profile.json from an installer JAR.
// Shared by Forge and NeoForge since both use the same installer format.
func extractInstallerJSONs(jarData []byte) (versionJSON []byte, installProfile []byte, err error) {
	r, err := zip.NewReader(bytes.NewReader(jarData), int64(len(jarData)))
	if err != nil {
		return nil, nil, fmt.Errorf("opening installer JAR: %w", err)
	}

	for _, f := range r.File {
		switch f.Name {
		case "version.json":
			rc, err := f.Open()
			if err != nil {
				return nil, nil, err
			}
			versionJSON, err = io.ReadAll(rc)
			rc.Close()
			if err != nil {
				return nil, nil, err
			}
		case "install_profile.json":
			rc, err := f.Open()
			if err != nil {
				return nil, nil, err
			}
			installProfile, err = io.ReadAll(rc)
			rc.Close()
			if err != nil {
				return nil, nil, err
			}
		}
	}

	if versionJSON == nil {
		return nil, nil, fmt.Errorf("version.json not found in installer JAR")
	}

	return versionJSON, installProfile, nil
}
