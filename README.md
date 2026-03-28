# Croopor

Offline Minecraft launcher for Windows. Reads your existing Minecraft installation, builds the launch command from version metadata, and strips the `--demo` flag so the game launches in full offline mode without demo restrictions.

## How it works

The official Minecraft launcher adds `--demo` to the launch command for non-premium accounts. Croopor constructs the command itself by parsing version JSON files, resolving libraries, and substituting template variables. The `--demo` argument is excluded and `--accessToken` is set to null, which results in a normal offline session.

Croopor never modifies existing Minecraft files. It only reads from `.minecraft` and writes new files when downloading versions or Java runtimes.

## Features

- Detects installed Minecraft versions (vanilla, Fabric, Forge, NeoForge)
- Downloads new versions directly from Mojang servers
- Auto-downloads the correct Java runtime for each version
- Handles version inheritance for modded loaders
- Configurable player name, memory allocation, window size
- First-launch onboarding with system RAM detection
- Real-time game output log

## Building

Requires Go 1.23 or later.

Production build (Windows, no console window):

```
GOOS=windows GOARCH=amd64 go build -ldflags="-s -w -H windowsgui" -o croopor.exe .
```

Development build (with dev tools for cleanup/flush):

```
GOOS=windows GOARCH=amd64 go build -tags dev -o croopor.exe .
```

The dev build adds a "Developer Tools" section in Settings with options to wipe all installed versions (with automatic backup of worlds, mods, and resource packs) and to flush all program data back to the onboarding state. These features are excluded from production binaries entirely via build tags.

On Windows, the app opens a native WebView2 window. On other platforms, it falls back to opening a browser tab for development purposes.

## Releasing

Push a version tag to trigger the CI build:

```
git tag v1.0.0
git push --tags
```

The GitHub Actions workflow builds Windows (amd64, arm64) and Linux (amd64) binaries, embeds the Windows icon via go-winres, and publishes them as a GitHub release.

## Project structure

```
main.go                          Entry point, flags, server startup
app_windows.go                   Native WebView2 window (Windows)
app_other.go                     Browser fallback (Linux/macOS)
internal/
  config/config.go               User preferences (stored in %APPDATA%/paralauncher/)
  minecraft/
    paths.go                     .minecraft detection and validation
    version.go                   Version JSON parsing (legacy and modern formats)
    version_merge.go             inheritsFrom resolution for modded versions
    library.go                   Library classpath resolution
    rules.go                     Rule evaluation (OS, features, demo suppression)
    arguments.go                 Template variable substitution
    scanner.go                   Local version scanning
    manifest.go                  Mojang version manifest fetching
    downloader.go                Version file downloader
    java.go                      Java runtime discovery and auto-download
  launcher/
    builder.go                   Launch command construction
    process.go                   Game process management
    natives.go                   Temporary natives directory handling
  server/
    server.go                    HTTP server and route registration
    api.go                       REST API handlers
    events.go                    Server-Sent Events for launch/install progress
    dev.go                       Dev-only routes (build tag: dev)
    dev_stub.go                  No-op stub for production builds
frontend/static/
  index.html, style.css, app.js  Embedded web UI
```

## License

See repository for license information.
