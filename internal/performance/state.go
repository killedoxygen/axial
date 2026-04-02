package performance

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"

	"github.com/mateoltd/croopor/internal/composition"
)

const lockFileName = ".croopor-lock.json"

// CompositionState records what is currently installed for one instance.
type CompositionState struct {
	CompositionID string                      `json:"composition_id"`
	Tier          composition.CompositionTier `json:"tier"`
	InstalledMods []InstalledMod              `json:"installed_mods"`
	InstalledAt   time.Time                   `json:"installed_at"`
	FailureCount  int                         `json:"failure_count"`
	LastFailure   string                      `json:"last_failure,omitempty"`
}

// InstalledMod records a single managed mod that has been downloaded.
type InstalledMod struct {
	ProjectID string `json:"project_id"`
	VersionID string `json:"version_id"`
	Filename  string `json:"filename"`
	SHA512    string `json:"sha512"`
}

func lockFilePath(instanceModsDir string) string {
	return filepath.Join(instanceModsDir, lockFileName)
}

func LoadState(instanceModsDir string) (*CompositionState, error) {
	data, err := os.ReadFile(lockFilePath(instanceModsDir))
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	var state CompositionState
	if err := json.Unmarshal(data, &state); err != nil {
		return nil, err
	}
	return &state, nil
}

func SaveState(instanceModsDir string, state *CompositionState) error {
	if err := os.MkdirAll(instanceModsDir, 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	tmpPath := lockFilePath(instanceModsDir) + ".tmp"
	if err := os.WriteFile(tmpPath, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmpPath, lockFilePath(instanceModsDir))
}
