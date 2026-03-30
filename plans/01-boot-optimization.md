# Plan 1: Boot Optimization

**Priority**: Highest — independent of all other plans, immediately noticeable impact, low-medium complexity.

**Goal**: Make the system remain fully responsive while Minecraft boots. Reduce actual boot time where possible.

---

## Background

Minecraft boot causes a triple resource spike (CPU from JVM class loading + JIT, I/O from asset loading, memory from heap expansion) that starves other processes. The launcher controls the process creation and JVM arguments, so it can mitigate all three axes.

---

## Phase 1: Process Priority Management

**What**: Launch the JVM at `BELOW_NORMAL` priority so the OS scheduler deprioritizes it against other apps. Raise to `NORMAL` once the game window is up.

### Backend Changes

**File: `internal/launcher/procattr_windows.go`**
- Add `BELOW_NORMAL_PRIORITY_CLASS` (0x00004000) to `CreationFlags` in `setProcAttr()`
- Current code already sets `CREATE_NEW_PROCESS_GROUP`; combine both flags

**File: `internal/launcher/procattr_other.go`**
- Set `syscall.SysProcAttr.Pdeathsig` (already done) and add a post-start `syscall.Setpriority(syscall.PRIO_PROCESS, pid, 10)` call (nice value 10 = low priority)

**File: `internal/launcher/process.go`**
- In the goroutine that reads stdout lines, detect the boot-complete marker line:
  - Modern versions (1.13+): look for `[Render thread/INFO]: Created:` or `Setting user:` in log output
  - Legacy versions: look for `Minecraft Launcher` or `LWJGL Version`
- When detected, call a new `promoteProcess()` function
- Add `bootCompleted bool` field to `GameProcess` with mutex protection
- Add `BootDuration time.Duration` field — record elapsed time from start to boot-complete for the frontend to display

**New file: `internal/launcher/priority_windows.go`**
```go
//go:build windows

package launcher

import "golang.org/x/sys/windows"

func promoteProcess(pid int) error {
    handle, err := windows.OpenProcess(windows.PROCESS_SET_INFORMATION, false, uint32(pid))
    if err != nil {
        return err
    }
    defer windows.CloseHandle(handle)
    return windows.SetPriorityClass(handle, windows.NORMAL_PRIORITY_CLASS)
}
```

**New file: `internal/launcher/priority_other.go`**
```go
//go:build !windows

package launcher

import "syscall"

func promoteProcess(pid int) error {
    return syscall.Setpriority(syscall.PRIO_PROCESS, pid, 0) // reset to default
}
```

### Dependencies
- Add `golang.org/x/sys` to go.mod (may already be present for windows package)

### Testing
- Launch a version, confirm system remains responsive during boot
- Confirm game gets promoted to NORMAL after window appears
- Confirm `BootDuration` is populated and reasonable (typically 5-30s)
- Test on low-end hardware if available

---

## Phase 2: JVM GC Flag Presets

**What**: Expose a "Launch Profile" setting that injects optimized GC flags. Two presets: Aikar's (battle-tested G1GC tuning) and ZGC (low-latency, Java 17+ only).

### Config Changes

**File: `internal/config/config.go`**
- Add field: `JVMPreset string` — values: `""` (default/none), `"aikar"`, `"zgc"`
- Add to `DefaultConfig()`: `JVMPreset: ""`
- No validation needed beyond checking allowed values

### Backend Changes

**File: `internal/launcher/builder.go`**
- After the memory flags are added (around line 140), insert GC preset flags before the main class
- New function `gcPresetArgs(preset string, javaMajor int) []string`:

```go
func gcPresetArgs(preset string, javaMajor int) []string {
    switch preset {
    case "aikar":
        return []string{
            "-XX:+UseG1GC",
            "-XX:+ParallelRefProcEnabled",
            "-XX:MaxGCPauseMillis=200",
            "-XX:+UnlockExperimentalVMOptions",
            "-XX:+DisableExplicitGC",
            "-XX:G1NewSizePercent=30",
            "-XX:G1MaxNewSizePercent=40",
            "-XX:G1HeapRegionSize=8M",
            "-XX:G1ReservePercent=20",
            "-XX:G1HeapWastePercent=5",
            "-XX:G1MixedGCCountTarget=4",
            "-XX:InitiatingHeapOccupancyPercent=15",
            "-XX:G1MixedGCLiveThresholdPercent=90",
            "-XX:G1RSetUpdatingPauseTimePercent=5",
            "-XX:SurvivorRatio=32",
            "-XX:+PerfDisableSharedMem",
            "-XX:MaxTenuringThreshold=1",
        }
    case "zgc":
        if javaMajor < 17 {
            return nil // ZGC not available
        }
        args := []string{"-XX:+UseZGC"}
        if javaMajor >= 21 {
            args = append(args, "-XX:+ZGenerational") // generational ZGC, Java 21+
        }
        return args
    default:
        return nil
    }
}
```

- Call this after resolving Java (we know `javaMajor` from the version JSON's `javaVersion.majorVersion`)
- Insert returned flags into `jvmArgs` slice before memory flags

### API Changes

**File: `internal/server/api.go`**
- The existing `PUT /api/v1/config` handler already accepts arbitrary config fields — just ensure `JVMPreset` is included in the config struct and it will work

### Frontend Changes

**File: `frontend/static/app.js`**
- In the Settings → Java section, add a dropdown/radio group:
  - "Default (no optimization)" — `""`
  - "Aikar's Flags (recommended)" — `"aikar"`
  - "ZGC Low-Latency (Java 17+)" — `"zgc"`
- If ZGC is selected but the current version's Java is <17, show a warning note
- Wire to `PUT /api/v1/config` with `jvmPreset` field

### Testing
- Launch with each preset, verify correct flags appear in `GET /api/v1/launch/{id}/command`
- Launch with `"zgc"` on a Java 8 version → verify it gracefully falls back (returns nil, no crash)
- Compare FPS/stability anecdotally with and without Aikar's flags

---

## Phase 3: Application Class Data Sharing (CDS)

**What**: After first launch of a version, generate a JVM class data sharing archive. Subsequent launches use the cached archive, reducing class-loading time by 15-30%.

### Backend Changes

**New file: `internal/launcher/cds.go`**
```go
package launcher

// CDSArchivePath returns the path to the CDS archive for a version.
// Located at: <configDir>/cds/<versionID>.jsa
func CDSArchivePath(configDir, versionID string) string

// CDSArchiveExists checks if a CDS archive exists and is valid.
func CDSArchiveExists(configDir, versionID string) bool

// GenerateCDSArchive runs a one-time JVM dump pass to create the archive.
// Uses: java -Xshare:dump -XX:SharedArchiveFile=<path> -cp <classpath>
// This is a blocking operation (typically 5-15 seconds).
func GenerateCDSArchive(javaPath, classpath, archivePath string) error

// InvalidateCDSArchive removes the archive (called when mods change, version reinstalled).
func InvalidateCDSArchive(configDir, versionID string) error
```

**File: `internal/launcher/builder.go`**
- After classpath is built, check `CDSArchiveExists()`
- If exists: prepend `-Xshare:on -XX:SharedArchiveFile=<path>` to JVM args
- If not exists: after successful launch (process started), schedule `GenerateCDSArchive()` in a background goroutine for next time
- The generation runs using the same Java path and classpath as the launch

**File: `internal/minecraft/downloader.go`**
- At the end of `InstallVersion()`, call `InvalidateCDSArchive()` to clear stale archives when a version is reinstalled

**File: `internal/server/api.go`**
- In the delete version handler, also call `InvalidateCDSArchive()`

### CDS Archive Storage
- Store in `<configDir>/cds/<versionID>.jsa`
- Typical size: 30-60 MB per version
- Invalidated on: reinstall, delete, mod changes (future: tracked via mods folder hash)

### Frontend Changes
- None required for basic CDS. Optionally:
  - Show "Optimizing for next launch..." in the version detail after first launch
  - Show "Boot optimized" badge if CDS archive exists

### Edge Cases
- CDS archive is classpath-dependent. If a mod is added/removed, the archive becomes invalid
  - For now: only generate for vanilla versions (modded versions change mods frequently)
  - Future: hash the mods folder and include in archive key
- If `-Xshare:on` fails (corrupted archive), JVM falls back gracefully — no crash
- Java 8 supports CDS but with limitations. Java 11+ has full AppCDS. Only enable for Java 11+.

### Testing
- Install a vanilla version, launch once (no CDS), launch again (CDS applied)
- Verify with `-Xlog:class+load` that classes are loaded from shared archive
- Time both launches and compare
- Reinstall the version → verify CDS archive is deleted
- Test with Java 8 version → verify CDS is skipped

---

## Phase 4: I/O Priority (Optional Enhancement)

**What**: Set I/O priority to low during boot so disk reads don't starve other applications.

### Backend Changes

**File: `internal/launcher/priority_windows.go`**
- Add `setIOPriority(pid int, low bool) error` using `NtSetInformationProcess` with `ProcessIoPriority`
- Call with `low=true` at launch, `low=false` at boot-complete (same trigger as Phase 1)

**File: `internal/launcher/priority_other.go`**
- Use `exec.Command("ionice", "-c", "3", "-p", strconv.Itoa(pid)).Run()` for idle I/O class
- Reset with `exec.Command("ionice", "-c", "0", "-p", strconv.Itoa(pid)).Run()`

### Note
- Phase 4 is a nice-to-have that rides on Phase 1's infrastructure (same trigger, same timing)
- Can be deferred if Phase 1 alone provides sufficient improvement
- `NtSetInformationProcess` requires `ntdll.dll` syscall on Windows — test carefully

### Testing
- Observe disk I/O patterns with Resource Monitor (Windows) or `iotop` (Linux) during boot
- Confirm other apps' disk responsiveness improves during MC boot

---

## Implementation Order

1. Phase 1 (priority sandwich) — do this first, largest impact
2. Phase 2 (GC presets) — trivial addition to config + builder
3. Phase 3 (CDS) — medium effort, meaningful boot time reduction
4. Phase 4 (I/O priority) — only if Phase 1 isn't sufficient alone

## Files Changed (Summary)

| File | Change |
|------|--------|
| `internal/launcher/procattr_windows.go` | Add BELOW_NORMAL flag |
| `internal/launcher/procattr_other.go` | Add nice value |
| `internal/launcher/process.go` | Boot detection, promote trigger, BootDuration |
| `internal/launcher/priority_windows.go` | **New** — promoteProcess + I/O priority |
| `internal/launcher/priority_other.go` | **New** — promoteProcess + ionice |
| `internal/launcher/builder.go` | GC preset injection, CDS flags |
| `internal/launcher/cds.go` | **New** — CDS archive management |
| `internal/config/config.go` | Add JVMPreset field |
| `internal/minecraft/downloader.go` | Invalidate CDS on reinstall |
| `internal/server/api.go` | Invalidate CDS on delete |
| `frontend/static/app.js` | GC preset dropdown in settings |
| `go.mod` | Add golang.org/x/sys if not present |

---

## Remaining Plans

After this plan is implemented, the following plans are ready for future sessions (execute only when explicitly approved):

- **[Plan 2: Instance Isolation](./02-instance-isolation.md)** — Per-version game directories with symlink-based hybrid model. Prerequisite for Plan 3.
- **[Plan 3: Performance Client Bundle](./03-performance-client-bundle.md)** — Auto-install Fabric + Sodium/Lithium/FerriteCore via Modrinth API for 1.16+. Depends on Plan 2.
- **[Plan 4: MSA Authentication](./04-msa-authentication.md)** — Microsoft account login via device code flow. Full OAuth2 → Xbox Live → XSTS → MC token chain.
- **[Plan 5: Skin System](./05-skin-system.md)** — Skin preview in launcher, SkinRestorer helper, CustomSkinLoader auto-config. Partially depends on Plan 4.
