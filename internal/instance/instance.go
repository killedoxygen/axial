package instance

// Instance represents a named play configuration tied to a Minecraft version.
type Instance struct {
	ID           string `json:"id"`
	Name         string `json:"name"`
	VersionID    string `json:"version_id"`
	CreatedAt    string `json:"created_at"`
	LastPlayedAt string `json:"last_played_at,omitempty"`

	// Setting overrides. Zero value means "use global config".
	MaxMemoryMB     int    `json:"max_memory_mb,omitempty"`
	MinMemoryMB     int    `json:"min_memory_mb,omitempty"`
	JavaPath        string `json:"java_path,omitempty"`
	WindowWidth     int    `json:"window_width,omitempty"`
	WindowHeight    int    `json:"window_height,omitempty"`
	JVMPreset       string `json:"jvm_preset,omitempty"`
	PerformanceMode string `json:"performance_mode,omitempty"`
	ExtraJVMArgs    string `json:"extra_jvm_args,omitempty"`
}
