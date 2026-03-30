# Plan 2: Instance Isolation (Profile System)

**Priority**: High — prerequisite for Performance Client Bundle (Plan 3). User already working on this.

**Goal**: Each version launch can run against an isolated directory instead of the shared `.minecraft`, preventing mods/saves/configs from bleeding across versions.

---

## Background

Currently all versions share a single `.minecraft` directory. If you install Fabric 1.21.4 mods and then launch vanilla 1.20.1, the 1.21.4 mods are still in the `mods/` folder and cause crashes. Real launchers solve this with per-instance directories.

---

## Design Decisions

### Isolation Model: Symlink-Based Hybrid

Rather than copying the entire `.minecraft` per instance (wasteful), or using fully independent directories (loses shared assets), use a **hybrid model**:

- **Shared** (symlinked/referenced from each instance): `assets/`, `libraries/`, `runtime/`, `versions/`
- **Per-instance** (unique copy): `mods/`, `saves/`, `resourcepacks/`, `shaderpacks/`, `config/`, `options.txt`, `servers.dat`

Each instance is a directory under `<configDir>/instances/<instance-id>/` containing:
```
instances/
└── my-survival-1.21/
    ├── instance.json          # Instance metadata
    ├── minecraft/             # Instance game directory
    │   ├── mods/              # Real directory
    │   ├── saves/             # Real directory
    │   ├── resourcepacks/     # Real directory
    │   ├── shaderpacks/       # Real directory
    │   ├── config/            # Real directory
    │   ├── options.txt        # Real file
    │   ├── servers.dat        # Real file
    │   ├── assets -> ../../.minecraft/assets         # Symlink
    │   ├── libraries -> ../../.minecraft/libraries   # Symlink
    │   └── versions -> ../../.minecraft/versions     # Symlink
    └── logs/                  # Instance launch logs
```

### Why Symlinks
- Saves ~2-5 GB of duplicated assets/libraries per instance
- Version JARs and Java runtimes remain centralized
- `--gameDir` argument already tells Minecraft where to look for saves/mods/config
- The game follows symlinks transparently

### Windows Symlink Caveat
- Windows symlinks require either admin privileges or Developer Mode enabled
- Fallback: use NTFS junction points for directories (no special privileges needed)
- Implementation: try `os.Symlink()` first, fall back to `mklink /J` via exec

---

## Phase 1: Instance Data Model

### New File: `internal/instance/instance.go`

```go
package instance

type Instance struct {
    ID          string    `json:"id"`          // Unique slug (e.g., "survival-1.21")
    Name        string    `json:"name"`        // Display name
    VersionID   string    `json:"versionId"`   // Minecraft version to launch
    CreatedAt   string    `json:"createdAt"`   // ISO 8601
    LastPlayed  string    `json:"lastPlayed"`  // ISO 8601
    Notes       string    `json:"notes"`       // User notes

    // Overrides (empty = use global config)
    MemoryMaxMB int       `json:"memoryMaxMb,omitempty"`
    MemoryMinMB int       `json:"memoryMinMb,omitempty"`
    JavaPath    string    `json:"javaPath,omitempty"`
    WindowWidth int       `json:"windowWidth,omitempty"`
    WindowHeight int      `json:"windowHeight,omitempty"`
    JVMPreset   string    `json:"jvmPreset,omitempty"`   // From Plan 1
    JVMArgs     string    `json:"jvmArgs,omitempty"`      // Custom JVM args
}
```

### New File: `internal/instance/manager.go`

```go
package instance

type Manager struct {
    baseDir    string // <configDir>/instances/
    mcDir      string // shared .minecraft path
}

// Core operations
func NewManager(configDir, mcDir string) *Manager
func (m *Manager) List() ([]Instance, error)
func (m *Manager) Get(id string) (*Instance, error)
func (m *Manager) Create(name, versionID string) (*Instance, error)
func (m *Manager) Delete(id string, deleteSaves bool) error
func (m *Manager) Rename(id, newName string) error
func (m *Manager) Duplicate(id, newName string) error
func (m *Manager) Update(id string, patch InstancePatch) error

// Directory management
func (m *Manager) GameDir(id string) string        // Returns path to instance's minecraft/ dir
func (m *Manager) SetupLinks(id string) error       // Create symlinks to shared dirs
func (m *Manager) ValidateLinks(id string) error     // Check symlinks are intact
```

### Instance Creation Flow

`Create()`:
1. Generate ID from name (slugify: lowercase, replace spaces with `-`, append short random suffix if collision)
2. Create `instances/<id>/` directory
3. Create `instances/<id>/minecraft/` subdirectories: `mods/`, `saves/`, `resourcepacks/`, `shaderpacks/`, `config/`
4. Create symlinks/junctions from `minecraft/assets` → `.minecraft/assets`, etc.
5. Copy `options.txt` from `.minecraft/` if it exists (sensible defaults)
6. Write `instance.json` metadata
7. Return the created instance

---

## Phase 2: Integrate with Launch System

### File: `internal/launcher/builder.go`

**Current**: `BuildAndLaunch()` takes `LaunchOptions` with `MCDir` pointing to the shared `.minecraft`.

**Change**: Add `InstanceID string` to `LaunchOptions`. If set:
1. Resolve the instance via `Manager.Get(instanceID)`
2. Use `Manager.GameDir(instanceID)` as the `--gameDir` argument
3. Apply instance-specific overrides (memory, java path, JVM preset) over global config
4. Validate symlinks before launch (`Manager.ValidateLinks()`)

If `InstanceID` is empty, fall back to current behavior (shared `.minecraft`).

### File: `internal/minecraft/arguments.go`

**Change**: `LaunchVars.GameDirectory` should use the instance's game dir when launching from an instance. The `--gameDir` argument already exists in Minecraft's argument template — this is a variable substitution change only.

### File: `internal/launcher/process.go`

**Change**: Add `InstanceID string` to `GameProcess` for tracking which instance a running process belongs to.

---

## Phase 3: API Endpoints

### File: `internal/server/api.go`

Add the `Manager` to the `Server` struct. New routes:

```
GET    /api/v1/instances              → List all instances
GET    /api/v1/instances/{id}         → Get instance details
POST   /api/v1/instances              → Create instance (body: {name, versionId})
PUT    /api/v1/instances/{id}         → Update instance (body: partial Instance)
DELETE /api/v1/instances/{id}         → Delete instance (query: ?deleteSaves=true)
POST   /api/v1/instances/{id}/duplicate → Duplicate instance
POST   /api/v1/instances/{id}/open-folder → Open instance dir in file manager
POST   /api/v1/instances/{id}/launch  → Launch instance (replaces version-based launch)
```

### Modify existing launch endpoint

**Current**: `POST /api/v1/launch` takes `{versionId, ...}`.

**Change**: Keep existing endpoint for "quick launch" (no instance, shared .minecraft). Add `POST /api/v1/instances/{id}/launch` for instance-based launch. Both call the same `BuildAndLaunch()` but with different `LaunchOptions`.

---

## Phase 4: Frontend — Instance Management UI

### File: `frontend/static/app.js`

**New page**: "Instances" view (or integrate into existing launcher view).

**Option A — Instances as primary concept** (recommended):
- The sidebar becomes an instance list instead of a version list
- Each instance shows: name, version, last played, status (running/stopped)
- Clicking an instance shows its detail panel: settings overrides, launch button, mods folder shortcut
- "New Instance" button → modal: pick name, pick version from catalog, create
- The existing "version list" becomes the "catalog" used only when creating instances

**Option B — Instances as secondary** (simpler):
- Keep current version list as-is
- Add a small "Instances" tab in the sidebar
- This is less disruptive to the existing UX but creates a confusing dual-model

**Recommended: Option A** — it's the standard mental model (Prism, MultiMC, ATLauncher all work this way).

### UI Components Needed

1. **Instance list** (sidebar):
   - Instance name, version badge, running indicator
   - Search/filter by name
   - Right-click context menu: rename, duplicate, delete, open folder

2. **Instance detail panel** (center):
   - Header: name (editable), version, created date, last played
   - Launch button (large, prominent)
   - Settings overrides section: memory, java path, JVM preset, window size
   - Quick links: open mods folder, open saves folder, open resourcepacks folder
   - Notes field (free text)

3. **New Instance modal**:
   - Name input
   - Version picker (reuse existing catalog UI)
   - "Create" button

4. **Instance running state**:
   - Reuse existing LIVE badge + uptime counter
   - Log viewer attached to instance

### Migration

On first launch after this update:
- If user has versions installed but no instances, show a one-time migration prompt
- "Create instances from your installed versions?" → auto-creates one instance per installed version
- Or: silently allow shared `.minecraft` mode alongside instances (no forced migration)

---

## Phase 5: Instance Lifecycle Features

### Mods Folder Watching
- Optional: use `fsnotify` to watch the instance's `mods/` folder
- Show mod count in the instance detail
- Detect mod changes and invalidate CDS archive (ties into Plan 1)

### Import/Export
- Export: zip the instance's `minecraft/` directory (excluding symlinked dirs)
- Import: unzip into a new instance, re-create symlinks
- Useful for sharing modpacks between machines

### World Management
- List worlds in the instance's `saves/` directory
- Show world name, last played, size
- Allow moving worlds between instances

---

## Files Changed (Summary)

| File | Change |
|------|--------|
| `internal/instance/instance.go` | **New** — Instance struct and types |
| `internal/instance/manager.go` | **New** — CRUD operations, symlink management |
| `internal/instance/links.go` | **New** — Symlink/junction creation (platform-specific) |
| `internal/instance/links_windows.go` | **New** — NTFS junction fallback |
| `internal/launcher/builder.go` | Accept InstanceID, apply overrides |
| `internal/minecraft/arguments.go` | Use instance game dir in LaunchVars |
| `internal/launcher/process.go` | Track InstanceID on running process |
| `internal/server/api.go` | New instance endpoints, integrate Manager |
| `internal/server/server.go` | Add Manager to Server struct |
| `internal/config/config.go` | No changes needed (instances have own config) |
| `frontend/static/app.js` | Instance list, detail panel, creation modal, migration |
| `frontend/static/style.css` | Instance UI styling |
| `frontend/static/index.html` | Minimal structural changes |

---

## Edge Cases & Risks

1. **Windows junctions**: If `os.Symlink` fails AND `mklink /J` fails, fall back to copying shared directories (wasteful but functional)
2. **Broken symlinks**: If `.minecraft` is moved, all instance symlinks break. `ValidateLinks()` detects this and the UI shows a repair prompt
3. **Concurrent launches**: Two instances launched simultaneously is fine — they have separate game dirs. But two launches of the same instance should be blocked
4. **Disk space**: Instance isolation uses minimal extra space (only per-instance files). The export feature could create large zips — show size estimate before export
5. **Mod loader installers**: Fabric/Forge installers write to `.minecraft/versions/`. Since `versions/` is symlinked, this works transparently. But some mod loaders also write to `.minecraft/mods/` — which in the instance model should go to the instance's `mods/` dir. Document: "Install mod loaders globally, then assign to instances"
