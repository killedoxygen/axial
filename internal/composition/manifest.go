package composition

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"path/filepath"
)

// Load returns the manifest to use: remote cache if valid, built-in otherwise.
func Load(cacheDir string) (*Manifest, error) {
	cachePath := filepath.Join(cacheDir, "performance", "manifest.json")
	if data, err := os.ReadFile(cachePath); err == nil {
		var cached Manifest
		if err := json.Unmarshal(data, &cached); err == nil {
			if err := validate(&cached); err == nil {
				return &cached, nil
			}
			log.Printf("performance manifest cache invalid: %v", err)
		}
	}

	builtin := &Manifest{}
	if err := json.Unmarshal(builtinCatalog, builtin); err != nil {
		return nil, fmt.Errorf("builtin performance manifest invalid: %w", err)
	}
	if err := validate(builtin); err != nil {
		return nil, fmt.Errorf("builtin performance manifest failed validation: %w", err)
	}
	return builtin, nil
}

// Refresh is a stub until remote rule refresh ships.
func Refresh() error {
	return nil
}

func validate(m *Manifest) error {
	if m == nil {
		return os.ErrInvalid
	}
	if m.SchemaVersion != 1 {
		return errString("unsupported schema_version")
	}
	ids := make(map[string]struct{}, len(m.Compositions))
	for _, composition := range m.Compositions {
		if composition.ID == "" {
			return errString("composition id is required")
		}
		if _, exists := ids[composition.ID]; exists {
			return errString("duplicate composition id: " + composition.ID)
		}
		ids[composition.ID] = struct{}{}
	}
	for _, composition := range m.Compositions {
		if composition.FallbackTo == "" {
			continue
		}
		if _, exists := ids[composition.FallbackTo]; !exists {
			return errString("fallback_to references unknown composition: " + composition.FallbackTo)
		}
	}
	return nil
}

type errString string

func (e errString) Error() string { return string(e) }
