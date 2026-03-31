package minecraft

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// VersionJSON represents the full parsed version JSON file.
type VersionJSON struct {
	ID                 string `json:"id"`
	InheritsFrom       string `json:"inheritsFrom,omitempty"`
	Type               string `json:"type"`
	MainClass          string `json:"mainClass"`
	MinimumLauncherVer int    `json:"minimumLauncherVersion,omitempty"`
	ComplianceLevel    int    `json:"complianceLevel,omitempty"`
	ReleaseTime        string `json:"releaseTime,omitempty"`
	Time               string `json:"time,omitempty"`

	// Modern argument format (1.13+)
	Arguments *ArgumentsSection `json:"arguments,omitempty"`

	// Legacy argument format (<=1.12.2)
	MinecraftArguments string `json:"minecraftArguments,omitempty"`

	AssetIndex  AssetIndex   `json:"assetIndex"`
	Assets      string       `json:"assets,omitempty"`
	Downloads   Downloads    `json:"downloads"`
	JavaVersion JavaVersion  `json:"javaVersion"`
	Libraries   []Library    `json:"libraries"`
	Logging     *LoggingConf `json:"logging,omitempty"`
}

// ArgumentsSection holds the modern game and JVM argument arrays.
type ArgumentsSection struct {
	Game []Argument
	JVM  []Argument
}

func (a *ArgumentsSection) UnmarshalJSON(data []byte) error {
	var raw struct {
		Game []json.RawMessage `json:"game"`
		JVM  []json.RawMessage `json:"jvm"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	var err error
	a.Game, err = parseArguments(raw.Game)
	if err != nil {
		return fmt.Errorf("parsing game arguments: %w", err)
	}
	a.JVM, err = parseArguments(raw.JVM)
	if err != nil {
		return fmt.Errorf("parsing jvm arguments: %w", err)
	}
	return nil
}

// Argument represents a single entry in the arguments arrays.
// Can be a plain string or a conditional object with rules.
type Argument struct {
	Rules []Rule
	Value []string // normalized to slice; plain strings become len-1 slices
}

func parseArguments(raw []json.RawMessage) ([]Argument, error) {
	args := make([]Argument, 0, len(raw))
	for _, r := range raw {
		var arg Argument

		// Try plain string first
		var s string
		if err := json.Unmarshal(r, &s); err == nil {
			arg.Value = []string{s}
			args = append(args, arg)
			continue
		}

		// Try conditional object
		var obj struct {
			Rules []Rule          `json:"rules"`
			Value json.RawMessage `json:"value"`
		}
		if err := json.Unmarshal(r, &obj); err != nil {
			return nil, fmt.Errorf("cannot parse argument: %s", string(r))
		}
		arg.Rules = obj.Rules

		// Value can be a string or array of strings
		var single string
		if err := json.Unmarshal(obj.Value, &single); err == nil {
			arg.Value = []string{single}
		} else {
			var multi []string
			if err := json.Unmarshal(obj.Value, &multi); err != nil {
				return nil, fmt.Errorf("cannot parse argument value: %s", string(obj.Value))
			}
			arg.Value = multi
		}

		args = append(args, arg)
	}
	return args, nil
}

type AssetIndex struct {
	ID        string `json:"id"`
	SHA1      string `json:"sha1,omitempty"`
	Size      int64  `json:"size,omitempty"`
	TotalSize int64  `json:"totalSize,omitempty"`
	URL       string `json:"url,omitempty"`
}

type Downloads struct {
	Client         *DownloadEntry `json:"client,omitempty"`
	Server         *DownloadEntry `json:"server,omitempty"`
	ClientMappings *DownloadEntry `json:"client_mappings,omitempty"`
	ServerMappings *DownloadEntry `json:"server_mappings,omitempty"`
}

type DownloadEntry struct {
	SHA1 string `json:"sha1"`
	Size int64  `json:"size"`
	URL  string `json:"url"`
}

type JavaVersion struct {
	Component    string `json:"component"`
	MajorVersion int    `json:"majorVersion"`
}

type Library struct {
	Name      string            `json:"name"`
	Downloads *LibraryDownload  `json:"downloads,omitempty"`
	URL       string            `json:"url,omitempty"`
	Rules     []Rule            `json:"rules,omitempty"`
	Natives   map[string]string `json:"natives,omitempty"`
	Extract   *ExtractRule      `json:"extract,omitempty"`

	// Fabric/Forge style direct fields
	SHA1   string `json:"sha1,omitempty"`
	SHA256 string `json:"sha256,omitempty"`
	Size   int64  `json:"size,omitempty"`
}

type LibraryDownload struct {
	Artifact    *LibraryArtifact            `json:"artifact,omitempty"`
	Classifiers map[string]*LibraryArtifact `json:"classifiers,omitempty"`
}

type LibraryArtifact struct {
	Path string `json:"path"`
	SHA1 string `json:"sha1,omitempty"`
	Size int64  `json:"size,omitempty"`
	URL  string `json:"url,omitempty"`
}

type ExtractRule struct {
	Exclude []string `json:"exclude,omitempty"`
}

type LoggingConf struct {
	Client *LoggingEntry `json:"client,omitempty"`
}

type LoggingEntry struct {
	Argument string      `json:"argument"`
	File     LoggingFile `json:"file"`
	Type     string      `json:"type"`
}

type LoggingFile struct {
	ID   string `json:"id"`
	SHA1 string `json:"sha1,omitempty"`
	Size int64  `json:"size,omitempty"`
	URL  string `json:"url,omitempty"`
}

// LoadVersionJSON reads and parses a version JSON file.
func LoadVersionJSON(mcDir, versionID string) (*VersionJSON, error) {
	path := filepath.Join(VersionsDir(mcDir), versionID, versionID+".json")
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading version %s: %w", versionID, err)
	}

	var v VersionJSON
	if err := json.Unmarshal(data, &v); err != nil {
		return nil, fmt.Errorf("parsing version %s: %w", versionID, err)
	}

	// If assets field is set but assetIndex.ID is empty, use assets as fallback
	if v.AssetIndex.ID == "" && v.Assets != "" {
		v.AssetIndex.ID = v.Assets
	}

	return &v, nil
}

// MavenToPath converts a Maven coordinate like "org.lwjgl:lwjgl:3.3.3" or
// "org.lwjgl:lwjgl:3.3.3:natives-windows" to a relative file path.
func MavenToPath(coordinate string) string {
	// Handle @extension syntax: group:artifact:version:classifier@extension
	ext := ".jar"
	if atIdx := strings.LastIndex(coordinate, "@"); atIdx >= 0 {
		ext = "." + coordinate[atIdx+1:]
		coordinate = coordinate[:atIdx]
	}

	parts := strings.Split(coordinate, ":")
	if len(parts) < 3 {
		return ""
	}

	group := strings.ReplaceAll(parts[0], ".", string(filepath.Separator))
	artifact := parts[1]
	version := parts[2]

	filename := artifact + "-" + version
	if len(parts) >= 4 {
		filename += "-" + parts[3]
	}
	filename += ext

	return filepath.Join(group, artifact, version, filename)
}

// IsLegacyVersion returns true if this version uses the old minecraftArguments format.
func (v *VersionJSON) IsLegacyVersion() bool {
	return v.Arguments == nil && v.MinecraftArguments != ""
}
