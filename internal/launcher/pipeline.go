package launcher

import (
	"fmt"
	"os/exec"

	"github.com/mateoltd/croopor/internal/config"
	"github.com/mateoltd/croopor/internal/minecraft"
)

// LaunchContext carries state through the launch pipeline.
// Each step reads from and writes to it.
type LaunchContext struct {
	// Inputs (set before pipeline runs)
	Opts      LaunchOptions
	SessionID string
	ConfigDir string

	// Progressive state (set by steps as they execute)
	Version       *minecraft.VersionJSON
	Env           minecraft.Environment
	JavaPath      string
	JavaMajor     int
	Libraries     []minecraft.ResolvedLibrary
	ClientJarPath string
	Classpath     string
	NativesDir    string
	IsModded      bool
	Vars          *minecraft.LaunchVars
	GameDir       string

	// Argument groups (assembled in order)
	CDSArgs   []string
	BootArgs  []string
	JVMArgs   []string
	GCArgs    []string
	MemArgs   []string
	GameArgs  []string

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

// defaultPipeline returns the standard launch pipeline.
func defaultPipeline() []LaunchStep {
	return []LaunchStep{
		&resolveVersionStep{},
		&setupEnvironmentStep{},
		&resolveJavaStep{},
		&resolveLibrariesStep{},
		&extractNativesStep{},
		&buildLaunchVarsStep{},
		&resolveArgumentsStep{},
		&prepareCDSStep{},
		&computeMemoryStep{},
		&applyBootThrottleStep{},
		&applyGCPresetStep{},
		&prefetchStep{},
		&buildCommandStep{},
		&startProcessStep{},
		&startProfilerStep{},
		&scheduleCDSStep{},
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
