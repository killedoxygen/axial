package config

import (
	"encoding/json"
	"os"
	"path/filepath"
	"runtime"
	"sync"
)

type Config struct {
	Username         string `json:"username"`
	MaxMemoryMB      int    `json:"max_memory_mb"`
	MinMemoryMB      int    `json:"min_memory_mb"`
	JavaPathOverride string `json:"java_path_override,omitempty"`
	WindowWidth      int    `json:"window_width,omitempty"`
	WindowHeight     int    `json:"window_height,omitempty"`
	JVMPreset        string `json:"jvm_preset,omitempty"`
	OnboardingDone   bool   `json:"onboarding_done"`
	MCDir            string `json:"mc_dir,omitempty"`
}

var (
	configDir  string
	configOnce sync.Once
)

func DefaultConfig() *Config {
	return &Config{
		Username:    "Player",
		MaxMemoryMB: 4096,
		MinMemoryMB: 512,
	}
}

func ConfigDir() string {
	configOnce.Do(func() {
		if runtime.GOOS == "windows" {
			appdata := os.Getenv("APPDATA")
			if appdata != "" {
				configDir = filepath.Join(appdata, "croopor")
			}
		}
		if configDir == "" {
			home, _ := os.UserHomeDir()
			configDir = filepath.Join(home, ".croopor")
		}
	})
	return configDir
}

func ConfigPath() string {
	return filepath.Join(ConfigDir(), "config.json")
}

func Load() (*Config, error) {
	cfg := DefaultConfig()
	data, err := os.ReadFile(ConfigPath())
	if err != nil {
		if os.IsNotExist(err) {
			return cfg, nil
		}
		return nil, err
	}
	if err := json.Unmarshal(data, cfg); err != nil {
		return nil, err
	}
	if cfg.MaxMemoryMB < 512 {
		cfg.MaxMemoryMB = 4096
	}
	if cfg.MinMemoryMB < 256 {
		cfg.MinMemoryMB = 512
	}
	if cfg.Username == "" {
		cfg.Username = "Player"
	}
	return cfg, nil
}

func Save(cfg *Config) error {
	dir := ConfigDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(ConfigPath(), data, 0644)
}
