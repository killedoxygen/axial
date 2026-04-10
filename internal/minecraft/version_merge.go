package minecraft

import "fmt"

// ResolveVersion loads a version and resolves any inheritsFrom chain.
// Returns a fully merged VersionJSON ready for launch command building.
func ResolveVersion(mcDir, versionID string) (*VersionJSON, error) {
	v, err := LoadVersionJSON(mcDir, versionID)
	if err != nil {
		return nil, err
	}

	if v.InheritsFrom == "" {
		return v, nil
	}

	// Resolve parent chain (usually just one level, but handle recursion)
	return resolveInheritance(mcDir, v, 0)
}

func resolveInheritance(mcDir string, child *VersionJSON, depth int) (*VersionJSON, error) {
	if depth > 10 {
		return nil, fmt.Errorf("inheritsFrom chain too deep (>10) for %s", child.ID)
	}

	if child.InheritsFrom == "" {
		return child, nil
	}

	parent, err := LoadVersionJSON(mcDir, child.InheritsFrom)
	if err != nil {
		return nil, fmt.Errorf("loading parent %s for %s: %w", child.InheritsFrom, child.ID, err)
	}

	// Recursively resolve parent if it also inherits
	if parent.InheritsFrom != "" {
		parent, err = resolveInheritance(mcDir, parent, depth+1)
		if err != nil {
			return nil, err
		}
	}

	return mergeVersions(parent, child), nil
}

// mergeVersions merges a child version onto a parent version.
// Child values override parent values where set; libraries and arguments are concatenated.
func mergeVersions(parent, child *VersionJSON) *VersionJSON {
	merged := &VersionJSON{}

	// ID comes from the child (the version the user selected)
	merged.ID = child.ID
	merged.Type = nonEmpty(child.Type, parent.Type)
	merged.ReleaseTime = nonEmpty(child.ReleaseTime, parent.ReleaseTime)
	merged.Time = nonEmpty(child.Time, parent.Time)
	merged.ComplianceLevel = parent.ComplianceLevel
	if child.ComplianceLevel != 0 {
		merged.ComplianceLevel = child.ComplianceLevel
	}

	// MainClass: child overrides parent
	merged.MainClass = nonEmpty(child.MainClass, parent.MainClass)

	// AssetIndex: parent provides it unless child overrides
	merged.AssetIndex = parent.AssetIndex
	if child.AssetIndex.ID != "" {
		merged.AssetIndex = child.AssetIndex
	}
	merged.Assets = nonEmpty(child.Assets, parent.Assets)

	// JavaVersion: parent provides it unless child overrides
	merged.JavaVersion = parent.JavaVersion
	if child.JavaVersion.Component != "" {
		merged.JavaVersion.Component = child.JavaVersion.Component
	}
	if child.JavaVersion.MajorVersion != 0 {
		merged.JavaVersion.MajorVersion = child.JavaVersion.MajorVersion
	}

	// Downloads: parent has the client JAR download
	merged.Downloads = parent.Downloads

	// Logging: parent provides it
	merged.Logging = parent.Logging
	if child.Logging != nil {
		merged.Logging = child.Logging
	}

	// Libraries: child libraries come FIRST (higher priority), then parent
	merged.Libraries = make([]Library, 0, len(child.Libraries)+len(parent.Libraries))
	merged.Libraries = append(merged.Libraries, child.Libraries...)
	merged.Libraries = append(merged.Libraries, parent.Libraries...)

	// Arguments: concatenate parent + child
	mergeArguments(merged, parent, child)

	return merged
}

func mergeArguments(merged, parent, child *VersionJSON) {
	// Handle legacy format
	if child.MinecraftArguments != "" {
		merged.MinecraftArguments = child.MinecraftArguments
	} else if parent.MinecraftArguments != "" {
		merged.MinecraftArguments = parent.MinecraftArguments
	}

	// Handle modern format
	if parent.Arguments != nil || child.Arguments != nil {
		merged.Arguments = &ArgumentsSection{}

		if parent.Arguments != nil {
			merged.Arguments.Game = append(merged.Arguments.Game, parent.Arguments.Game...)
			merged.Arguments.JVM = append(merged.Arguments.JVM, parent.Arguments.JVM...)
		}
		if child.Arguments != nil {
			merged.Arguments.Game = append(merged.Arguments.Game, child.Arguments.Game...)
			merged.Arguments.JVM = append(merged.Arguments.JVM, child.Arguments.JVM...)
		}
	}
}

func nonEmpty(a, b string) string {
	if a != "" {
		return a
	}
	return b
}
