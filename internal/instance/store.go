package instance

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/mateoltd/croopor/internal/config"
)

// InstanceStore holds all instances and persists to instances.json.
type InstanceStore struct {
	mu             sync.RWMutex
	Instances      []Instance `json:"instances"`
	LastInstanceID string     `json:"last_instance_id,omitempty"`
}

func storePath() string {
	return filepath.Join(config.ConfigDir(), "instances.json")
}

// InstancesBaseDir returns the parent directory for all instance game dirs.
func InstancesBaseDir() string {
	return filepath.Join(config.ConfigDir(), "instances")
}

// GameDir returns the game directory for a specific instance.
func GameDir(id string) string {
	return filepath.Join(InstancesBaseDir(), id)
}

// Load reads the instance store from disk. Returns an empty store if the file doesn't exist.
func Load() (*InstanceStore, error) {
	store := &InstanceStore{}
	data, err := os.ReadFile(storePath())
	if err != nil {
		if os.IsNotExist(err) {
			return store, nil
		}
		return nil, err
	}
	if err := json.Unmarshal(data, store); err != nil {
		return nil, err
	}
	return store, nil
}

// Save writes the instance store to disk atomically (temp file + rename).
func Save(store *InstanceStore) error {
	dir := config.ConfigDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(store, "", "  ")
	if err != nil {
		return err
	}
	tmp := storePath() + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, storePath())
}

// Get returns a copy of the instance with the given ID, or nil if not found.
func (s *InstanceStore) Get(id string) *Instance {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for i := range s.Instances {
		if s.Instances[i].ID == id {
			inst := s.Instances[i]
			return &inst
		}
	}
	return nil
}

// NameExists returns true if any instance already uses this name (case-sensitive).
func (s *InstanceStore) NameExists(name, excludeID string) bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, inst := range s.Instances {
		if inst.Name == name && inst.ID != excludeID {
			return true
		}
	}
	return false
}

// Add creates an instance, sets up its game directory, and persists.
func (s *InstanceStore) Add(name, versionID, mcDir string) (*Instance, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if name == "" {
		return nil, errors.New("instance name is required")
	}
	if versionID == "" {
		return nil, errors.New("version_id is required")
	}
	for _, inst := range s.Instances {
		if inst.Name == name {
			return nil, errors.New("an instance with this name already exists")
		}
	}

	inst := Instance{
		ID:        generateID(),
		Name:      name,
		VersionID: versionID,
		CreatedAt: time.Now().UTC().Format(time.RFC3339),
	}

	// Create game directory with standard subdirs
	gameDir := GameDir(inst.ID)
	for _, sub := range []string{"saves", "mods", "resourcepacks", "shaderpacks", "config"} {
		if err := os.MkdirAll(filepath.Join(gameDir, sub), 0755); err != nil {
			return nil, fmt.Errorf("creating instance dir: %w", err)
		}
	}

	// Copy options.txt from .minecraft so the user inherits keybinds/video settings
	if mcDir != "" {
		src := filepath.Join(mcDir, "options.txt")
		if data, err := os.ReadFile(src); err == nil {
			os.WriteFile(filepath.Join(gameDir, "options.txt"), data, 0644)
		}
	}

	s.Instances = append(s.Instances, inst)
	if err := Save(s); err != nil {
		return nil, err
	}
	return &inst, nil
}

// Update replaces an instance by ID and persists.
func (s *InstanceStore) Update(inst Instance) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i := range s.Instances {
		if s.Instances[i].ID == inst.ID {
			s.Instances[i] = inst
			return Save(s)
		}
	}
	return errors.New("instance not found")
}

// Remove deletes an instance from the store. If deleteFiles is true, also removes the game directory.
func (s *InstanceStore) Remove(id string, deleteFiles bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	idx := -1
	for i := range s.Instances {
		if s.Instances[i].ID == id {
			idx = i
			break
		}
	}
	if idx == -1 {
		return errors.New("instance not found")
	}

	s.Instances = append(s.Instances[:idx], s.Instances[idx+1:]...)

	if s.LastInstanceID == id {
		s.LastInstanceID = ""
	}

	if deleteFiles {
		os.RemoveAll(GameDir(id))
	}

	return Save(s)
}

// Duplicate creates a copy of an instance with a new name. If copyFiles is true,
// deep-copies the game directory; otherwise creates a fresh one.
func (s *InstanceStore) Duplicate(id, newName, mcDir string, copyFiles bool) (*Instance, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if newName == "" {
		return nil, errors.New("instance name is required")
	}
	for _, inst := range s.Instances {
		if inst.Name == newName {
			return nil, errors.New("an instance with this name already exists")
		}
	}

	var src *Instance
	for i := range s.Instances {
		if s.Instances[i].ID == id {
			cpy := s.Instances[i]
			src = &cpy
			break
		}
	}
	if src == nil {
		return nil, errors.New("instance not found")
	}

	inst := *src
	inst.ID = generateID()
	inst.Name = newName
	inst.CreatedAt = time.Now().UTC().Format(time.RFC3339)
	inst.LastPlayedAt = ""

	gameDir := GameDir(inst.ID)

	if copyFiles {
		if err := copyDir(GameDir(id), gameDir); err != nil {
			return nil, fmt.Errorf("copying instance files: %w", err)
		}
	} else {
		for _, sub := range []string{"saves", "mods", "resourcepacks", "shaderpacks", "config"} {
			if err := os.MkdirAll(filepath.Join(gameDir, sub), 0755); err != nil {
				return nil, fmt.Errorf("creating instance dir: %w", err)
			}
		}
	}

	s.Instances = append(s.Instances, inst)
	if err := Save(s); err != nil {
		return nil, err
	}
	return &inst, nil
}

// List returns a snapshot copy of all instances.
func (s *InstanceStore) List() []Instance {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := make([]Instance, len(s.Instances))
	copy(out, s.Instances)
	return out
}

// GetLastInstanceID returns the last selected instance ID.
func (s *InstanceStore) GetLastInstanceID() string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.LastInstanceID
}

// SetLastInstanceID sets the last selected instance ID in memory.
// The caller is responsible for calling Save to persist the change.
func (s *InstanceStore) SetLastInstanceID(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.LastInstanceID = id
}

// Clear removes all instances and resets the last instance ID.
func (s *InstanceStore) Clear() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.Instances = nil
	s.LastInstanceID = ""
}

// Len returns the number of instances.
func (s *InstanceStore) Len() int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return len(s.Instances)
}

func generateID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}

func copyDir(src, dst string) error {
	return filepath.Walk(src, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(src, path)
		target := filepath.Join(dst, rel)
		if info.IsDir() {
			return os.MkdirAll(target, 0755)
		}
		// Skip very large files (>100MB)
		if info.Size() > 100*1024*1024 {
			return nil
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		return os.WriteFile(target, data, info.Mode())
	})
}
