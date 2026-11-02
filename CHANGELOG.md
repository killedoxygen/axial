# Changelog

All notable changes to Axial are recorded here, newest first.

Format follows [Keep a Changelog](https://keepachangelog.com): one
`## [version] - date` section per release, with `### Heading` groups and `-`
bullets. `## [Unreleased]` collects work that has not shipped yet.

## [Unreleased]

## [0.4.0-rc.5] - 2026-11-02

### Distribution
- Axial updates itself: update packages are signed and announced at updates.axial.run, so only the first install meets the operating system's warning
- The download page at updates.axial.run picks the right file for the computer and shows, with pictures, exactly what to click when macOS, Windows or the browser warns
- The macOS disk image carries the same guide, and its button opens Privacy & Security directly
- Axial is available under the MIT license

## [0.4.0-rc.4] - 2026-11-02

### Onboarding
- First run takes two steps: sign in with Microsoft or pick an offline name and look, then choose a color and what runs in the background, with the player's 3D character alongside
- Memory starts at the system recommendation instead of being asked up front; anonymous stats stay off until turned on

### Mods and content
- Updates, disables and removals no longer leave an instance unable to start; Axial names the mods involved and offers to change them together
- Mods installed by a modpack are kept out of Update all
- Import .jar files into an instance and create instances from .mrpack files, by picking or dropping them, including packs not published on Modrinth
- A hand-added file that matches a mod is adopted instead of blocking its install; a different file can be replaced on request
- Installed mods show their icon, version, authors and modpack; new Resource packs and Shaders tabs
- Downloads lists recent failures with what failed and in which instance

### Performance
- Installs and launches are faster: on macOS a fresh vanilla 1.20.1 install went from about 100 s to 41 s, Forge from 43 s to 29 s, and launching from 1.7 s to 0.8 s

### Fixes
- A lost OpenGL context on Minecraft 26.x reports graphics as unavailable instead of a generic crash
- A saved custom Java path is shown again, the Mods pane refreshes after setup, and modpack set up honours the active version and loader filters
- A start stopped by a loader's incompatible-mods screen is recorded as a failed start with the reason
- Built-in shortcuts use Cmd instead of Ctrl on macOS, and shortcut hints show the platform's modifier symbols

### Security
- Updated tokio, libz-sys and p256 for RustSec advisories
- Builds use Node 26 LTS and pnpm 11.28.10, which fixes three pnpm advisories (GHSA-c59q-g84q-2gj5, GHSA-vq4v-j7r6-jq4m, GHSA-vx52-2968-3vc6)
- Rust dependencies refreshed to their latest compatible releases

### Release integrity
- Releases are exposed only after their version, notes, complete platform asset set, and checksums agree

### Distribution
- Source files keep LF line endings on every platform, so Windows packages build from the same checked bytes
- macOS apps are signed ad hoc as a whole bundle, so macOS offers Open Anyway instead of reporting a downloaded app as damaged

## [0.4.0-rc.1] - 2026-10-11

### App
- Replaced application orchestration with feature-owned install, launch, content, resource, and Performance workflows
- Preserved current-application operation recovery and profile reopen
- Restored original loader logos and native skin, screenshot, and world controls

### Breaking changes
- Start with a fresh application profile; predecessor profiles are not imported or upgraded

### Distribution
- Candidate packages bind their exact source, version, changelog, platform payloads, and checksums
- Unpublished release candidate; installed-platform acceptance, publisher trust, and signed installed updates remain release gates

## [0.4.0-dev.5] - 2026-07-15

### Distribution
- Native macOS disk images published alongside archived in-app update packages

## [0.4.0-dev.4] - 2026-07-15

### Distribution
- User-facing executables published alongside archived in-app update packages

### Interface
- Axial window decorations enabled on Linux

## [0.4.0-dev.3] - 2026-07-15

### Content
- Discover and install Modrinth mods, resource packs, shader packs, and modpacks
- Target existing instances or configure content while creating a new instance
- Dependency-aware, provenance-preserving installs, updates, and bulk removals

### Updates
- Update controls moved into the topbar with a streamlined download-and-restart flow
- Clearer queued-restart state while a game or download is still running
- In-app update archives added for Intel and Apple Silicon macOS

### Distribution
- Platform-specific update archives and checksums for Linux, Windows, and both macOS architectures

### Interface
- Unified instance card banners
- Removed the window-management permission prompt on startup
- Search-led Discover navigation with pagination, filters, trays, and pack previews

### Performance
- Cached create options return without blocking provider refreshes
- Deferred views warm during idle time with reduced paint and compositing work

## [0.4.0-dev.2] - 2026-07-11

### Updates
- Development release channel so dev builds can offer newer dev versions

## [0.4.0-dev.1] - 2026-07-11

### In-app updater
- Verified, staged update downloads that apply on restart
- semver-aware release comparison so pre-releases are offered correctly

### App
- Rebranded the project to Axial
- Rebuilt global settings on one auto-saving sheet layout
- Rebindable keyboard shortcuts with local overrides and conflict guard
- Native window chrome per platform

## [0.3.1] - 2026-04-02

### Maintenance
- Version bump and packaging fixes

## [0.3.0] - 2026-04-01

### Desktop app rewrite
- Migrated from a browser-served frontend to a native desktop app with Preact and signals
- Proper taskbar icon and rounded app icons on all platforms

### In-app updater
- Automatic update detection on startup and from settings

### Background music
- Optional background music with auto-mute when a game instance launches

### Stability
- Hardened loader installs, onboarding, and processor runtime
- Per-version loader install locks prevent concurrent corruption

## [0.2.0] - 2026-03-30

### Multi-instance launching
- Run multiple Minecraft instances simultaneously
- Per-instance session tracking with independent progress streams
- Resource warning when combined memory allocation exceeds system RAM

### Install queue
- Queued installs no longer silently drop the second request
- Sequential queue with per-instance sidebar progress

### Game output
- Timestamps on every log line and per-instance tags
- Instance filter to isolate one instance's output

### Stability and settings
- `-Xshare:auto` with automatic CDS archive repair on corruption
- JVM performance preset and theme now persist correctly
- In-app styled dialogs replace native alert/confirm/prompt
