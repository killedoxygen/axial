# Plan 3: Performance Client Bundle

**Priority**: Medium — depends on Instance Isolation (Plan 2) being complete. High user value.

**Goal**: One-click creation of a "Performance Instance" that auto-installs Fabric + curated performance mods for any version 1.16+, via Modrinth API.

---

## Background

The Minecraft modding community has converged on a standard performance stack (Sodium, Lithium, FerriteCore, etc.) that provides 2-5x FPS improvement. Currently users must manually install Fabric, find compatible mod versions, and place JARs. This plan automates that entirely.

---

## Design Decisions

### Why Fabric (not Forge/NeoForge)
- All top performance mods target Fabric first
- Fabric is lightweight (tiny loader, no coremod complexity)
- Fabric installer is a simple JAR execution that creates a version JSON in `versions/`
- NeoForge support can be added later as a second profile type

### Why Modrinth API (not bundling JARs)
- Licensing: performance mods have various licenses — redistribution may not be permitted
- Freshness: Modrinth always has the latest compatible versions
- Integrity: Modrinth provides SHA512 hashes
- Legal clarity: we're an installer, not a redistributor

### Version Coverage
- 1.16.x – 1.21.x: Full support (Sodium + full stack)
- 1.14 – 1.15: Fabric exists, Sodium doesn't — limited optimization
- Pre-1.14: No Fabric, skip entirely

---

## Phase 1: Modrinth API Client

### New File: `internal/modrinth/client.go`

```go
package modrinth

const BaseURL = "https://api.modrinth.com/v2"

type Client struct {
    httpClient *http.Client
    userAgent  string // "mateoltd/croopor/<version> (contact@example.com)"
}

// Version represents a specific mod version on Modrinth.
type Version struct {
    ID            string   `json:"id"`
    ProjectID     string   `json:"project_id"`
    Name          string   `json:"name"`
    VersionNumber string   `json:"version_number"`
    GameVersions  []string `json:"game_versions"`
    Loaders       []string `json:"loaders"`
    Files         []File   `json:"files"`
}

type File struct {
    URL      string            `json:"url"`
    Filename string            `json:"filename"`
    Hashes   map[string]string `json:"hashes"` // "sha512" → hash
    Size     int64             `json:"size"`
    Primary  bool              `json:"primary"`
}

// GetCompatibleVersion finds the latest compatible version of a project
// for a given game version and loader.
func (c *Client) GetCompatibleVersion(projectSlug, gameVersion, loader string) (*Version, error)

// BatchResolve resolves multiple projects at once using Modrinth's bulk endpoint.
// Uses: GET /v2/projects?ids=[...]  +  GET /v2/project/{id}/version?game_versions&loaders
func (c *Client) BatchResolve(slugs []string, gameVersion, loader string) (map[string]*Version, error)

// DownloadFile downloads a mod JAR to the target directory with hash verification.
func (c *Client) DownloadFile(file File, targetDir string) error
```

### Modrinth API Rate Limits
- 300 requests/minute per IP
- We'll need at most ~10 requests per bundle install (one per mod)
- Include `User-Agent` header as required by Modrinth TOS

---

## Phase 2: Performance Bundle Manifest

### New File: `internal/performance/bundle.go`

```go
package performance

// Mod represents a performance mod to include in the bundle.
type Mod struct {
    Slug        string `json:"slug"`        // Modrinth project slug
    Name        string `json:"name"`        // Display name
    Description string `json:"description"` // Why this mod helps
    Required    bool   `json:"required"`    // false = optional enhancement
    MinVersion  string `json:"minVersion"`  // Minimum MC version (e.g., "1.16")
}

// The curated performance bundle.
var PerformanceMods = []Mod{
    // Render & GPU
    {Slug: "sodium",          Name: "Sodium",          Description: "Rewrites render engine (2-5x FPS)",         Required: true,  MinVersion: "1.16"},
    {Slug: "indium",          Name: "Indium",          Description: "Sodium compat for Fabric Rendering API",    Required: true,  MinVersion: "1.16"},
    {Slug: "immediatelyfast", Name: "ImmediatelyFast", Description: "Optimizes immediate-mode rendering",        Required: false, MinVersion: "1.17"},
    {Slug: "entityculling",   Name: "Entity Culling",  Description: "Skips rendering hidden entities",           Required: false, MinVersion: "1.16"},

    // CPU & logic
    {Slug: "lithium",         Name: "Lithium",         Description: "Optimizes game logic and entity AI",        Required: true,  MinVersion: "1.16"},
    {Slug: "starlight",       Name: "Starlight",       Description: "Rewrites lighting engine",                  Required: true,  MinVersion: "1.17"},

    // Memory
    {Slug: "ferritecore",     Name: "FerriteCore",     Description: "Reduces memory usage 30-50%",               Required: true,  MinVersion: "1.16"},

    // Network
    {Slug: "krypton",         Name: "Krypton",         Description: "Optimizes networking stack",                Required: false, MinVersion: "1.16"},

    // Boot time — these directly reduce the "system lag during launch" problem
    {Slug: "modernfix",       Name: "ModernFix",       Description: "Defers DFU init + various boot optimizations", Required: true, MinVersion: "1.16"},
    {Slug: "smooth-boot-reloaded", Name: "Smooth Boot", Description: "Limits worker threads during boot to prevent system stall", Required: true, MinVersion: "1.16"},
    {Slug: "lazy-language-loader", Name: "Lazy Language Loader", Description: "Defers language file loading until needed", Required: false, MinVersion: "1.16"},
}

// ResolveBundleForVersion returns the subset of mods compatible with a game version.
func ResolveBundleForVersion(gameVersion string) []Mod

// BundleLockfile records exactly which versions were installed.
type BundleLockfile struct {
    GameVersion string                    `json:"gameVersion"`
    Loader      string                    `json:"loader"`
    InstalledAt string                    `json:"installedAt"`
    Mods        map[string]LockedMod      `json:"mods"`
}

type LockedMod struct {
    Slug          string `json:"slug"`
    VersionID     string `json:"versionId"`
    VersionNumber string `json:"versionNumber"`
    Filename      string `json:"filename"`
    SHA512        string `json:"sha512"`
}
```

### Lockfile Purpose
- Stored in instance directory as `performance-bundle.lock.json`
- Used to detect when updates are available
- Used to distinguish "user-added mods" from "bundle mods" (bundle mods are managed; user mods are left alone)

---

## Phase 3: Fabric Auto-Installer

### New File: `internal/performance/fabric.go`

```go
package performance

// EnsureFabric checks if Fabric is installed for the target MC version.
// If not, downloads and runs the Fabric installer.
func EnsureFabric(mcDir, gameVersion string) (fabricVersionID string, error)

// FabricInstallerURL returns the URL for the latest Fabric installer JAR.
// Uses: https://meta.fabricmc.net/v2/versions/installer
func FabricInstallerURL() (string, error)

// FabricLoaderVersion returns the latest stable loader version.
// Uses: https://meta.fabricmc.net/v2/versions/loader
func FabricLoaderVersion() (string, error)

// RunFabricInstaller executes the Fabric installer JAR headlessly.
// Command: java -jar fabric-installer.jar client -dir <mcDir> -mcversion <version> -loader <loaderVersion>
func RunFabricInstaller(javaPath, installerPath, mcDir, gameVersion, loaderVersion string) error
```

### Fabric Installation Flow
1. Fetch latest Fabric installer URL from `meta.fabricmc.net`
2. Download installer JAR to temp directory
3. Find a Java runtime (any available — installer is Java 8+)
4. Run: `java -jar fabric-installer.jar client -dir <mcDir> -mcversion <gameVersion> -loader <loaderVersion>`
5. This creates `versions/fabric-loader-<loader>-<mc>/` with the version JSON
6. Clean up installer JAR
7. Return the `fabric-loader-X.Y.Z-<mcVersion>` version ID

---

## Phase 4: Bundle Installation Orchestrator

### New File: `internal/performance/installer.go`

```go
package performance

type InstallProgress struct {
    Phase   string `json:"phase"`   // "fabric", "resolving", "downloading", "complete"
    Current int    `json:"current"`
    Total   int    `json:"total"`
    ModName string `json:"modName,omitempty"`
    Error   string `json:"error,omitempty"`
}

// InstallPerformanceBundle orchestrates the full performance instance creation.
// Steps:
// 1. Ensure Fabric is installed for the game version
// 2. Resolve compatible mod versions via Modrinth API
// 3. Download mod JARs to instance mods/ directory
// 4. Write lockfile
// 5. Stream progress via channel
func InstallPerformanceBundle(
    mcDir string,
    instanceModsDir string,
    gameVersion string,
    javaPath string,
    progressCh chan<- InstallProgress,
) (*BundleLockfile, error)
```

### Installation Flow (Full)
1. **Validate**: Check game version is ≥1.16
2. **Install Fabric**: `EnsureFabric()` — creates the Fabric version in shared `.minecraft/versions/`
3. **Resolve mods**: `modrinth.BatchResolve()` — find latest compatible version of each mod
4. **Report unresolvable**: If any required mod has no compatible version, abort with clear error
5. **Download mods**: Download each JAR to the instance's `mods/` directory with SHA512 verification
6. **Write lockfile**: Save `performance-bundle.lock.json` in the instance directory
7. **Update instance**: Set the instance's `versionId` to the Fabric version ID
8. **Done**: Instance is ready to launch

---

## Phase 5: API & Frontend

### API Endpoints

**File: `internal/server/api.go`**

```
POST /api/v1/instances/{id}/performance-bundle     → Install performance bundle on instance
GET  /api/v1/instances/{id}/performance-bundle      → Get bundle status (installed mods, lockfile)
DELETE /api/v1/instances/{id}/performance-bundle     → Remove bundle (delete bundle mods, keep user mods)
GET  /api/v1/performance/check?gameVersion=1.21.4   → Preview: what mods would be installed
POST /api/v1/performance/update/{id}                → Update bundle mods to latest compatible versions
```

The install endpoint streams SSE progress (same pattern as version install).

### Frontend

**File: `frontend/static/app.js`**

**In "New Instance" modal**:
- Add checkbox: "Enable Performance Optimization" (checked by default)
- When checked, show the mod list that will be installed
- Version compatibility note: "Available for Minecraft 1.16+"
- If version is <1.16, checkbox is disabled with tooltip

**In Instance Detail panel**:
- "Performance Bundle" section showing:
  - Installed mods with versions
  - "Update Available" indicator if lockfile is outdated
  - "Remove Performance Bundle" button
  - "Update Bundle" button

**In New Instance creation flow**:
1. User picks name + version
2. If performance checkbox is on:
   - Progress modal shows: "Installing Fabric..." → "Resolving mods..." → "Downloading Sodium..." → etc.
   - On completion, instance is ready

---

## Phase 6: Bundle Updates

### Update Detection
- On app start (or instance list load), check lockfiles against Modrinth API
- Compare `versionNumber` in lockfile vs latest compatible version
- Show "Update available" badge on instances with outdated bundles

### Update Flow
1. Resolve new versions via Modrinth
2. Delete old mod JARs (identified by lockfile)
3. Download new JARs
4. Update lockfile
5. Invalidate CDS archive (from Plan 1) if applicable

### Rate Limiting
- Cache Modrinth responses for 1 hour
- Only check for updates on explicit user action or app start (not continuously)

---

## Files Changed (Summary)

| File | Change |
|------|--------|
| `internal/modrinth/client.go` | **New** — Modrinth API client |
| `internal/performance/bundle.go` | **New** — Bundle manifest and lockfile |
| `internal/performance/fabric.go` | **New** — Fabric auto-installer |
| `internal/performance/installer.go` | **New** — Orchestrator with progress streaming |
| `internal/server/api.go` | New performance bundle endpoints |
| `frontend/static/app.js` | Instance creation checkbox, bundle status UI |
| `go.mod` | No new dependencies (uses net/http) |

---

## Risks & Mitigations

1. **Modrinth API downtime**: Cache last-known-good mod versions locally. If API is unreachable, offer to install from cache.
2. **Mod compatibility breaks**: Lockfile pins exact versions. Updates are opt-in, never automatic.
3. **Fabric installer changes**: The CLI interface is stable (`-mcversion`, `-loader`, `-dir`). Pin a known-good installer version as fallback.
4. **Mod removed from Modrinth**: Detect in update check, warn user, offer to keep existing JAR.
5. **Disk space**: Performance mods are small (~1-5 MB each). Total bundle: ~15-30 MB.
6. **Pre-1.16 gap**: Clearly communicate "Performance optimization available for 1.16+" in the UI. Don't attempt partial solutions for older versions.
