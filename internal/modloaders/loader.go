package modloaders

// LoaderType identifies a mod loader.
type LoaderType string

const (
	Fabric   LoaderType = "fabric"
	Quilt    LoaderType = "quilt"
	Forge    LoaderType = "forge"
	NeoForge LoaderType = "neoforge"
)

// LoaderInfo describes a loader for the frontend.
type LoaderInfo struct {
	Type        LoaderType `json:"type"`
	Name        string     `json:"name"`
	Description string     `json:"description"`
}

// GameVersion indicates that a loader supports a given Minecraft version.
type GameVersion struct {
	Version string `json:"version"`
	Stable  bool   `json:"stable"`
}

// LoaderVersion represents a specific version of a mod loader.
type LoaderVersion struct {
	Version     string `json:"version"`
	Stable      bool   `json:"stable"`
	Recommended bool   `json:"recommended,omitempty"`
}

// InstallResult is returned after a successful loader installation.
type InstallResult struct {
	VersionID   string     // Composite ID, e.g. "fabric-loader-0.16.9-1.21.4"
	GameVersion string     // e.g. "1.21.4"
	LoaderType  LoaderType // e.g. Fabric
}

// Progress reports installation status to the caller.
type Progress struct {
	Phase   string `json:"phase"`
	Current int    `json:"current"`
	Total   int    `json:"total"`
	Detail  string `json:"detail,omitempty"`
	Error   string `json:"error,omitempty"`
}

// Loader is the interface every mod loader backend implements.
type Loader interface {
	// Type returns the loader identifier.
	Type() LoaderType

	// Info returns display metadata for the frontend.
	Info() LoaderInfo

	// GameVersions returns Minecraft versions this loader supports.
	GameVersions() ([]GameVersion, error)

	// LoaderVersions returns available loader versions for a given Minecraft version.
	LoaderVersions(mcVersion string) ([]LoaderVersion, error)

	// Install installs the loader into mcDir for the given MC + loader version.
	// Progress is sent on the provided channel. The method blocks until done.
	// The caller is responsible for creating and reading from the channel.
	Install(mcDir, mcVersion, loaderVersion string, progress chan<- Progress) (*InstallResult, error)

	// VersionID returns the composite version ID without performing network calls.
	VersionID(mcVersion, loaderVersion string) string

	// NeedsBaseGameFirst returns true if the base game must be installed before
	// the loader (e.g. Forge needs Java from the runtime download).
	NeedsBaseGameFirst() bool
}
