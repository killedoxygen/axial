# Changelog

All notable changes to Axial are recorded here, newest first.

Format follows [Keep a Changelog](https://keepachangelog.com): one
`## [version] - date` section per release, with `### Heading` groups and `-`
bullets. `## [Unreleased]` collects work that has not shipped yet.

## [Unreleased]

### Updates
- Update controls moved into the topbar with a streamlined download-and-restart flow
- Clearer queued-restart state while a game or download is still running

### Interface
- Unified instance card banners
- Removed the window-management permission prompt on startup

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
