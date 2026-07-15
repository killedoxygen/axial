# Axial

Axial is a modern Minecraft launcher and management environment that creates, tunes, repairs, and safely launches vanilla and modded instances.

It is built around a simple promise: launching Minecraft should feel fast, understandable, and resilient. Axial absorbs the routine technical work, explains meaningful adjustments, and gives advanced players control without making every player debug Java, loaders, or damaged game files.

> Axial Launcher is pre-release software under active development.

## What Axial does

- Creates and manages multiple Minecraft instances
- Installs vanilla, Fabric, Quilt, Forge, and NeoForge
- Detects compatible Java runtimes and downloads managed runtimes when needed
- Tunes memory, JVM settings, and managed performance compositions
- Runs offline accounts and Microsoft-authenticated Minecraft accounts
- Tracks installs and launches with live progress, logs, and local evidence
- Manages local skins, themes, shortcuts, music, onboarding, and desktop updates
- Builds native desktop releases for macOS, Windows, and Linux

## Guardian

Guardian is Axial's safety and recovery layer. It turns runtime, install, launch, and performance facts into bounded decisions before a small configuration problem becomes a broken instance. It stays quiet until it is needed; Guardian supports the product rather than defining it.

Depending on the selected mode and who owns the affected state, Guardian can warn, choose a safer runtime, repair launcher-managed files, retry a failed startup once, fall back to a safe performance plan, or block an unsafe operation. Its actions are journaled, verified, redacted for user-facing output, and constrained to avoid destructive repair loops or silent changes to user-owned files.

The goal is not to hide technical control. Managed defaults protect casual players, while explicit and reversible overrides keep the launcher useful for advanced players.

## Project shape

- Desktop shell: [Tauri](https://v2.tauri.app/)
- Frontend: Preact and Signals
- Backend: Rust workspace under `apps/` and `core/`
- Desktop progress transport: native Tauri event bridge
- Browser-mode progress transport: server-sent events
- Development entrypoint: [Task](https://taskfile.dev/)

The product logic is split across launcher, Minecraft, performance, configuration, application, Guardian, state, and observability boundaries. Start with [`docs/README.md`](docs/README.md) for the current architecture map and contributor documentation.

## Development

### Requirements

- Rust stable with `rustfmt` and `clippy`
- Node.js 22 or newer
- [Task](https://taskfile.dev/installation)

On Ubuntu 24.04, desktop builds also need:

```bash
sudo apt-get update
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev
```

`task setup` installs the remaining project tooling, including frontend dependencies and `tauri-cli`. On Linux and WSL it also prepares Windows cross-build dependencies. `cargo-binstall` is optional, but speeds up the `tauri-cli` installation.

### Quick start

```bash
task setup
task dev
```

Run `task --list` to see every available task.

### Common commands

| Command | Purpose |
| --- | --- |
| `task dev` | Run the desktop app in development mode |
| `task dev:web` | Run the frontend in browser mode |
| `task dev:web:mock` | Run the frontend against the built-in mock API |
| `task dev:windows` | Run the Windows desktop target from Linux or WSL |
| `task api` | Run the local API server |
| `task check` | Run formatting, TypeScript, Rust check, and Clippy checks |
| `task test` | Run the Rust workspace tests |
| `task verify` | Run checks, tests, and a release desktop build |
| `task fmt` | Format Rust and frontend sources |
| `task build` | Build the release desktop binary |
| `task bundle` | Build native installer packages |
| `task doctor` | Report the local toolchain state |

The desktop development server uses `localhost:1420` by default. Override it with `DEV_PORT=3001 task dev`. Root `.env` and `.env.local` files are loaded automatically by Task.

### WSL and cross-building

`task dev` needs a Linux GUI session such as WSLg. From a headless WSL environment, use browser mode or the Windows target:

```bash
task dev:web
task dev:windows
```

`task build:windows` produces a raw Windows executable rather than a signed installer or updater package. Tagged GitHub releases publish raw desktop executables for manual downloads and archives for the in-app updater, each with a matching SHA-256 checksum file. Raw Linux and macOS downloads may need `chmod +x <file>` after downloading.

## Documentation

- [`docs/README.md`](docs/README.md): documentation index and ownership map
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): current system architecture
- [`docs/GUARDIAN-ARCHITECTURE.md`](docs/GUARDIAN-ARCHITECTURE.md): Guardian safety and self-healing model
- [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md): contributor conventions
- [`docs/DESIGN.md`](docs/DESIGN.md): product and interface guardrails
