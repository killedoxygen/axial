package launcher

import (
	"os/exec"

	"github.com/mateoltd/croopor/internal/launcher/engine"
	"github.com/mateoltd/croopor/internal/system"
)

const (
	PresetSmooth          = engine.PresetSmooth
	PresetPerformance     = engine.PresetPerformance
	PresetUltraLowLatency = engine.PresetUltraLowLatency
	PresetGraalVM         = engine.PresetGraalVM
	PresetLegacy          = engine.PresetLegacy
	PresetLegacyPvP       = engine.PresetLegacyPvP
	PresetLegacyHeavy     = engine.PresetLegacyHeavy
)

type LaunchAuthMode = engine.LaunchAuthMode

const (
	LaunchAuthOffline       = engine.LaunchAuthOffline
	LaunchAuthAuthenticated = engine.LaunchAuthAuthenticated
)

type LaunchOptions = engine.LaunchOptions
type LaunchResult = engine.LaunchResult
type HealingSummary = engine.HealingSummary
type LaunchError = engine.LaunchError
type LaunchFailureClass = engine.LaunchFailureClass
type ProcessState = engine.ProcessState
type GameProcess = engine.GameProcess
type LogLine = engine.LogLine

const (
	LaunchFailureUnknown                 = engine.LaunchFailureUnknown
	LaunchFailureJVMUnsupportedOption    = engine.LaunchFailureJVMUnsupportedOption
	LaunchFailureJVMExperimentalUnlock   = engine.LaunchFailureJVMExperimentalUnlock
	LaunchFailureJVMOptionOrdering       = engine.LaunchFailureJVMOptionOrdering
	LaunchFailureJavaRuntimeMismatch     = engine.LaunchFailureJavaRuntimeMismatch
	LaunchFailureClasspathModuleConflict = engine.LaunchFailureClasspathModuleConflict
	LaunchFailureAuthModeIncompatible    = engine.LaunchFailureAuthModeIncompatible
	LaunchFailureLoaderBootstrapFailure  = engine.LaunchFailureLoaderBootstrapFailure
)

const (
	StateStarting = engine.StateStarting
	StateRunning  = engine.StateRunning
	StateExited   = engine.StateExited
	StateFailed   = engine.StateFailed
)

func BuildAndLaunch(opts LaunchOptions) (*LaunchResult, error) {
	return engine.BuildAndLaunch(opts)
}

func NewGameProcess(cmd *exec.Cmd, nativesDir string) *GameProcess {
	return engine.NewGameProcess(cmd, nativesDir)
}

func AutoSelectPreset(profile system.HardwareProfile, javaMajor int, dist system.JavaDistribution) string {
	return engine.AutoSelectPreset(profile, javaMajor, dist)
}

func CDSArchivePath(configDir, versionID string) string {
	return engine.CDSArchivePath(configDir, versionID)
}

func CDSArchiveExists(configDir, versionID string) bool {
	return engine.CDSArchiveExists(configDir, versionID)
}

func GenerateCDSArchive(javaPath, classpath, archivePath string) error {
	return engine.GenerateCDSArchive(javaPath, classpath, archivePath)
}

func InvalidateCDSArchive(configDir, versionID string) {
	engine.InvalidateCDSArchive(configDir, versionID)
}
