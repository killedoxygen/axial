package launcher

import (
	"fmt"
	"log"
	"os/exec"

	"github.com/mateoltd/croopor/internal/composition"
	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/minecraft"
	"github.com/mateoltd/croopor/internal/performance"
)

// LaunchContext carries state through the launch pipeline.
// Each step reads from and writes to it.
type LaunchContext struct {
	// Inputs (set before pipeline runs)
	Opts      LaunchOptions
	SessionID string
	ConfigDir string

	// Progressive state (set by steps as they execute)
	Version         *minecraft.VersionJSON
	Env             minecraft.Environment
	JavaPath        string
	JavaMajor       int
	Libraries       []minecraft.ResolvedLibrary
	ClientJarPath   string
	Classpath       string
	NativesDir      string
	IsModded        bool
	Vars            *minecraft.LaunchVars
	GameDir         string
	CompositionPlan *composition.CompositionPlan

	// Effective values (computed by steps, used by profiler and downstream)
	EffectiveMaxMemoryMB int
	EffectiveMinMemoryMB int
	EffectivePreset      string // GC preset actually applied (auto-selected or user-configured)

	// Argument groups (assembled in order)
	CDSArgs  []string
	BootArgs []string
	JVMArgs  []string
	GCArgs   []string
	MemArgs  []string
	GameArgs []string

	// Final command
	CmdArgs []string
	Cmd     *exec.Cmd
	Process *GameProcess
}

// LaunchStep is a single composable unit in the launch pipeline.
type LaunchStep interface {
	Name() string
	Execute(ctx *LaunchContext) error
}

// runPipeline executes a sequence of launch steps in order.
func runPipeline(ctx *LaunchContext, steps []LaunchStep) error {
	for _, step := range steps {
		if err := step.Execute(ctx); err != nil {
			return fmt.Errorf("%s: %w", step.Name(), err)
		}
	}
	return nil
}

// bestEffort wraps a step so failures are logged but don't abort the pipeline.
// Used for post-start steps where the game process is already running.
type bestEffort struct{ inner LaunchStep }

func (b *bestEffort) Name() string { return b.inner.Name() }
func (b *bestEffort) Execute(ctx *LaunchContext) error {
	if err := b.inner.Execute(ctx); err != nil {
		log.Printf("warning: %s: %v", b.inner.Name(), err)
	}
	return nil
}

// defaultPipeline returns the standard launch pipeline.
func defaultPipeline(manager *performance.PerformanceManager) []LaunchStep {
	return []LaunchStep{
		&resolveVersionStep{},
		&setupEnvironmentStep{},
		&resolveJavaStep{},
		&resolveLibrariesStep{},
		&resolveCompositionStep{manager: manager},
		&extractNativesStep{},
		&buildLaunchVarsStep{},
		&resolveArgumentsStep{},
		&prepareCDSStep{},
		&computeMemoryStep{},
		&applyBootThrottleStep{},
		&applyGCPresetStep{},
		&applyCompositionJVMStep{},
		&prefetchStep{},
		&buildCommandStep{},
		&startProcessStep{},
		&bestEffort{&startProfilerStep{}},
		&bestEffort{&scheduleCDSStep{}},
	}
}

// newLaunchContext initializes a LaunchContext from options.
func newLaunchContext(opts LaunchOptions) *LaunchContext {
	return &LaunchContext{
		Opts:      opts,
		SessionID: generateSessionID(),
		ConfigDir: config.ConfigDir(),
	}
}
