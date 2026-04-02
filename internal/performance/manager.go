package performance

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"time"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/modrinth"
	"github.com/mateoltd/croopor/internal/system"
)

// PerformanceManager is the single point of contact for the launch pipeline and API layer.
type PerformanceManager struct {
	manifest *composition.Manifest
	hardware system.HardwareProfile
	modrinth modrinth.Client
}

// New creates a PerformanceManager. Call once at startup.
func New(configDir string, modrinthClient modrinth.Client) *PerformanceManager {
	return &PerformanceManager{
		manifest: composition.Load(configDir),
		hardware: system.Detect(),
		modrinth: modrinthClient,
	}
}

// GetPlan returns the effective CompositionPlan for the given instance parameters.
func (m *PerformanceManager) GetPlan(req composition.ResolutionRequest) *composition.CompositionPlan {
	if req.Mode == "" {
		req.Mode = composition.ModeManaged
	}
	if req.Hardware.CPU.LogicalCores == 0 && req.Hardware.TotalRAMMB == 0 {
		req.Hardware = m.hardware
	}
	return composition.Resolve(m.manifest, req)
}

// EnsureInstalled ensures all managed mods in the plan are present in instanceModsDir.
func (m *PerformanceManager) EnsureInstalled(
	ctx context.Context,
	plan *composition.CompositionPlan,
	gameVersion string,
	instanceModsDir string,
) (*CompositionState, error) {
	if plan == nil {
		return nil, errors.New("composition plan is required")
	}
	if m.modrinth == nil {
		return nil, errors.New("mod downloads are unavailable")
	}
	if err := os.MkdirAll(instanceModsDir, 0755); err != nil {
		return nil, err
	}

	if err := m.removeStaleManaged(instanceModsDir, plan.Mods); err != nil {
		return nil, err
	}

	state := &CompositionState{
		CompositionID: plan.CompositionID,
		Tier:          plan.Tier,
		InstalledAt:   time.Now().UTC(),
		InstalledMods: make([]InstalledMod, 0, len(plan.Mods)),
	}
	if len(plan.Mods) == 0 {
		return state, SaveState(instanceModsDir, state)
	}

	type result struct {
		mod *InstalledMod
		err error
	}

	sem := make(chan struct{}, 4)
	results := make(chan result, len(plan.Mods))
	var wg sync.WaitGroup

	for _, mod := range plan.Mods {
		mod := mod
		wg.Add(1)
		go func() {
			defer wg.Done()
			select {
			case sem <- struct{}{}:
			case <-ctx.Done():
				results <- result{err: ctx.Err()}
				return
			}
			defer func() { <-sem }()

			installed, err := installMod(ctx, m.modrinth, mod, gameVersion, plan.Loader, instanceModsDir)
			results <- result{mod: installed, err: err}
		}()
	}

	wg.Wait()
	close(results)

	var errs []error
	for res := range results {
		if res.err != nil {
			errs = append(errs, res.err)
			state.FailureCount++
			state.LastFailure = res.err.Error()
			continue
		}
		if res.mod != nil {
			state.InstalledMods = append(state.InstalledMods, *res.mod)
		}
	}

	slices.SortFunc(state.InstalledMods, func(a, b InstalledMod) int {
		switch {
		case a.ProjectID < b.ProjectID:
			return -1
		case a.ProjectID > b.ProjectID:
			return 1
		default:
			return 0
		}
	})

	if err := SaveState(instanceModsDir, state); err != nil {
		return nil, err
	}
	return state, errors.Join(errs...)
}

// RemoveManaged removes all launcher-managed mods from instanceModsDir.
func (m *PerformanceManager) RemoveManaged(instanceModsDir string) error {
	state, err := LoadState(instanceModsDir)
	if err != nil || state == nil {
		return err
	}

	for _, mod := range state.InstalledMods {
		if err := os.Remove(filepath.Join(instanceModsDir, mod.Filename)); err != nil && !os.IsNotExist(err) {
			return err
		}
	}
	if err := os.Remove(lockFilePath(instanceModsDir)); err != nil && !os.IsNotExist(err) {
		return err
	}
	return nil
}

func (m *PerformanceManager) removeStaleManaged(instanceModsDir string, mods []composition.ManagedMod) error {
	state, err := LoadState(instanceModsDir)
	if err != nil || state == nil {
		return err
	}

	keep := make(map[string]struct{}, len(mods))
	for _, mod := range mods {
		keep[mod.ProjectID] = struct{}{}
	}
	for _, installed := range state.InstalledMods {
		if _, ok := keep[installed.ProjectID]; ok {
			continue
		}
		if err := os.Remove(filepath.Join(instanceModsDir, installed.Filename)); err != nil && !os.IsNotExist(err) {
			return err
		}
	}
	return nil
}
