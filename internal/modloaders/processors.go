package modloaders

import (
	"archive/zip"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/mateoltd/croopor/internal/minecraft"
)

// installProfileJSON represents the install_profile.json from Forge/NeoForge installers.
type installProfileJSON struct {
	Processors []processor        `json:"processors"`
	Libraries  []minecraft.Library `json:"libraries"`
	Data       map[string]dataEntry `json:"data"`
}

type processor struct {
	Jar       string   `json:"jar"`
	Classpath []string `json:"classpath"`
	Args      []string `json:"args"`
	Sides     []string `json:"sides,omitempty"`
}

type dataEntry struct {
	Client string `json:"client"`
	Server string `json:"server"`
}

// RunForgeProcessors executes the processors from an install_profile.json.
// This patches the Minecraft client JAR and generates required artifacts.
func RunForgeProcessors(mcDir, mcVersion, versionID string, installProfileData, installerData []byte, progress chan<- Progress) error {
	var profile installProfileJSON
	if err := json.Unmarshal(installProfileData, &profile); err != nil {
		return fmt.Errorf("parsing install profile: %w", err)
	}

	if len(profile.Processors) == 0 {
		return nil // Legacy Forge, no processors needed
	}

	// Find Java runtime
	javaPath, err := findJavaForProcessors(mcDir)
	if err != nil {
		return fmt.Errorf("Java required for Forge processors: %w", err)
	}

	// Build library path lookup: Maven coordinate -> absolute path
	libDir := minecraft.LibrariesDir(mcDir)
	libPaths := map[string]string{}
	for _, lib := range profile.Libraries {
		mavenPath := minecraft.MavenToPath(lib.Name)
		if mavenPath != "" {
			libPaths[lib.Name] = filepath.Join(libDir, mavenPath)
		}
	}

	// Build data variable lookup
	dataVars, tempDir := buildDataVars(profile.Data, mcDir, mcVersion, versionID, installerData)
	if tempDir != "" {
		defer os.RemoveAll(tempDir)
	}

	// Filter to client-side processors only
	var clientProcessors []processor
	for _, p := range profile.Processors {
		if len(p.Sides) > 0 {
			isClient := false
			for _, s := range p.Sides {
				if s == "client" {
					isClient = true
					break
				}
			}
			if !isClient {
				continue
			}
		}
		clientProcessors = append(clientProcessors, p)
	}

	total := len(clientProcessors)
	for i, proc := range clientProcessors {
		progress <- Progress{
			Phase:   "loader_processors",
			Current: i + 1,
			Total:   total,
			Detail:  fmt.Sprintf("Processor %d/%d", i+1, total),
		}

		if err := runProcessor(javaPath, proc, libPaths, dataVars, libDir); err != nil {
			return fmt.Errorf("processor %d/%d failed: %w", i+1, total, err)
		}
	}

	return nil
}

func runProcessor(javaPath string, proc processor, libPaths map[string]string, dataVars map[string]string, libDir string) error {
	// Build classpath: processor JAR + its classpath entries
	var cpParts []string

	procJarPath := libPaths[proc.Jar]
	if procJarPath == "" {
		mavenPath := minecraft.MavenToPath(proc.Jar)
		if mavenPath != "" {
			procJarPath = filepath.Join(libDir, mavenPath)
		}
	}
	if procJarPath == "" {
		return fmt.Errorf("cannot resolve processor JAR: %s", proc.Jar)
	}
	cpParts = append(cpParts, procJarPath)

	for _, cp := range proc.Classpath {
		p := libPaths[cp]
		if p == "" {
			mavenPath := minecraft.MavenToPath(cp)
			if mavenPath != "" {
				p = filepath.Join(libDir, mavenPath)
			}
		}
		if p != "" {
			cpParts = append(cpParts, p)
		}
	}

	sep := ":"
	if runtime.GOOS == "windows" {
		sep = ";"
	}
	classpath := strings.Join(cpParts, sep)

	// Resolve processor main class from its JAR manifest
	mainClass, err := readMainClassFromJar(procJarPath)
	if err != nil {
		return fmt.Errorf("reading main class from %s: %w", proc.Jar, err)
	}

	// Substitute arguments
	args := make([]string, 0, len(proc.Args)+4)
	args = append(args, "-cp", classpath, mainClass)
	for _, arg := range proc.Args {
		args = append(args, substituteArg(arg, libPaths, dataVars, libDir))
	}

	cmd := exec.Command(javaPath, args...)
	cmd.Dir = libDir
	output, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%s\noutput: %s", err, string(output))
	}

	return nil
}

func readMainClassFromJar(jarPath string) (string, error) {
	f, err := os.Open(jarPath)
	if err != nil {
		return "", err
	}
	defer f.Close()

	stat, err := f.Stat()
	if err != nil {
		return "", err
	}

	zr, err := zip.NewReader(f, stat.Size())
	if err != nil {
		return "", err
	}

	for _, zf := range zr.File {
		if zf.Name == "META-INF/MANIFEST.MF" {
			rc, err := zf.Open()
			if err != nil {
				return "", err
			}
			data, err := io.ReadAll(rc)
			rc.Close()
			if err != nil {
				return "", err
			}
			for _, line := range strings.Split(string(data), "\n") {
				line = strings.TrimSpace(line)
				if strings.HasPrefix(line, "Main-Class:") {
					return strings.TrimSpace(strings.TrimPrefix(line, "Main-Class:")), nil
				}
			}
		}
	}

	return "", fmt.Errorf("no Main-Class in manifest")
}

func substituteArg(arg string, libPaths, dataVars map[string]string, libDir string) string {
	// [artifact:coordinate] -> library path
	if strings.HasPrefix(arg, "[") && strings.HasSuffix(arg, "]") {
		coord := arg[1 : len(arg)-1]
		if p, ok := libPaths[coord]; ok {
			return p
		}
		mavenPath := minecraft.MavenToPath(coord)
		if mavenPath != "" {
			return filepath.Join(libDir, mavenPath)
		}
		return arg
	}

	// {DATA_KEY} -> data variable value
	if strings.HasPrefix(arg, "{") && strings.HasSuffix(arg, "}") {
		key := arg[1 : len(arg)-1]
		if v, ok := dataVars[key]; ok {
			return v
		}
		return arg
	}

	return arg
}

func buildDataVars(data map[string]dataEntry, mcDir, mcVersion, versionID string, installerData []byte) (map[string]string, string) {
	vars := map[string]string{}
	tempDir := ""

	for key, entry := range data {
		val := entry.Client
		if val == "" {
			continue
		}

		// If value is [coordinate], resolve to library path
		if strings.HasPrefix(val, "[") && strings.HasSuffix(val, "]") {
			coord := val[1 : len(val)-1]
			mavenPath := minecraft.MavenToPath(coord)
			if mavenPath != "" {
				vars[key] = filepath.Join(minecraft.LibrariesDir(mcDir), mavenPath)
			}
			continue
		}

		// If value starts with /, it's a path inside the installer JAR — extract to temp
		if strings.HasPrefix(val, "/") {
			if tempDir == "" {
				var err error
				tempDir, err = os.MkdirTemp("", "forge-processors-")
				if err != nil {
					continue
				}
			}
			if extracted := extractFromInstallerJar(installerData, val[1:], tempDir); extracted != "" {
				vars[key] = extracted
			}
			continue
		}

		vars[key] = val
	}

	// Standard variables
	vars["MINECRAFT_JAR"] = filepath.Join(minecraft.VersionsDir(mcDir), mcVersion, mcVersion+".jar")
	vars["SIDE"] = "client"
	vars["MINECRAFT_VERSION"] = mcVersion
	vars["ROOT"] = mcDir
	vars["LIBRARY_DIR"] = minecraft.LibrariesDir(mcDir)

	return vars, tempDir
}

// extractFromInstallerJar extracts a single entry from the installer JAR ZIP to tempDir.
func extractFromInstallerJar(jarData []byte, entryPath, tempDir string) string {
	r, err := zip.NewReader(bytes.NewReader(jarData), int64(len(jarData)))
	if err != nil {
		return ""
	}

	for _, f := range r.File {
		if f.Name != entryPath {
			continue
		}
		rc, err := f.Open()
		if err != nil {
			return ""
		}
		data, err := io.ReadAll(rc)
		rc.Close()
		if err != nil {
			return ""
		}

		destPath := filepath.Join(tempDir, filepath.FromSlash(entryPath))
		os.MkdirAll(filepath.Dir(destPath), 0755)
		if err := os.WriteFile(destPath, data, 0644); err != nil {
			return ""
		}
		return destPath
	}
	return ""
}

func findJavaForProcessors(mcDir string) (string, error) {
	// Try common Java version components
	components := []string{"java-runtime-delta", "java-runtime-gamma", "java-runtime-beta", "java-runtime-alpha"}
	for _, comp := range components {
		result, err := minecraft.FindJava(mcDir, minecraft.JavaVersion{Component: comp, MajorVersion: 21}, "")
		if err == nil {
			return result.Path, nil
		}
	}

	// Fallback: check if "java" is on PATH
	javaExe := "java"
	if runtime.GOOS == "windows" {
		javaExe = "javaw.exe"
	}
	if path, err := exec.LookPath(javaExe); err == nil {
		return path, nil
	}

	return "", fmt.Errorf("no Java runtime found; install the base game version first to download Java")
}

