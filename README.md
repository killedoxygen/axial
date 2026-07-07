# croopor
offline minecraft launcher. on `rewrite-in-rust`, the active desktop/backend path is Rust + Tauri.

it already handles:
- multi-instance installs
- vanilla + fabric + quilt + forge + neoforge
- java runtime detection and download flows
- launch sessions, logs, install progress
- local music, themes, shortcuts, onboarding
- desktop update detection

## stack
- dev tooling: [task](https://taskfile.dev) (`Taskfile.yml`) + [tauri-cli](https://v2.tauri.app/reference/cli/)
- desktop shell: tauri
- frontend: preact + signals
- backend: rust workspace under `apps/` and `core/`
- desktop event streaming: tauri-native bridge
- browser mode: sse

## prereqs
- rust stable with `rustfmt` and `clippy`
- node 22+
- [task](https://taskfile.dev/installation) — the single dev entrypoint on every OS
  - macos: `brew install go-task`
  - windows: `winget install Task.Task`
  - linux: `sh -c "$(curl -fsSL https://taskfile.dev/install.sh)" -- -d -b ~/.local/bin`
- ubuntu 24.04 linux desktop builds need `libgtk-3-dev` and `libwebkit2gtk-4.1-dev`

```bash
sudo apt-get update
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev
```

everything else (`pnpm` via corepack, `tauri-cli`, the Windows cross toolchain on Linux/WSL) is installed by `task setup`.
`cargo-binstall` is optional but makes the `tauri-cli` install a ~15s download instead of a compile.

## quickstart
same commands on macos, linux, wsl, and windows:

```bash
task setup
task --list
task dev
```

## common commands
- `task setup`: one-time setup — frontend deps, Rust deps, tauri-cli, and (Linux/WSL) Windows cross-build prerequisites
- `task dev`: run the desktop app in dev mode — tauri-cli starts the frontend dev server, waits for it, hot-restarts Rust on change, and cleans up on exit
- `task dev:windows`: run desktop dev as a Windows app from Linux/WSL
- `task dev:web`: run the frontend-only dev server (browser mode, works headless)
- `task watch`: rebuild frontend assets to disk on file changes
- `task api`: run the local API server
- `task check`: run all static checks — prettier, tsc, `cargo fmt`, `cargo check`, clippy (matches CI)
- `task test`: run the Rust workspace tests
- `task verify`: checks + tests + release desktop build (full CI parity)
- `task fmt`: format Rust and frontend code
- `task build` / `task build:dev`: build the release/debug desktop binary (debug uses bundled static assets, no dev server)
- `task build:windows` / `task build:windows:dev`: cross-build the Windows desktop binary from Linux/WSL
- `task build:api` / `task build:api:release`: build the API binary
- `task bundle`: build native installers via tauri (`.app`/`.dmg`, `.msi`/`.exe`, `.deb`/`.rpm`)
- `task host:launch-evidence`: report Windows host Java and Minecraft/Croopor folders without printing paths (WSL or Windows)
- `task doctor`: show detected tools and platform state
- `task clean`: remove `target/` and `dist/`

the desktop dev server is pinned to `localhost:1420`; override it with `DEV_PORT=3001 task dev`.
`.env` and `.env.local` in the repo root are loaded automatically for every task.

## wsl note
`task dev` needs a Linux gui session. with wsl that means wslg or another linux gui path.

if you are in headless wsl, use the frontend-only server or run the Windows-targeted desktop app:

```bash
task dev:web
task dev:windows
```

windows cross-build note:

```bash
task setup
task build:windows
```

on Ubuntu/WSL, `setup` installs the Rust target and `gcc-mingw-w64-x86-64` by default unless `CI=true`.
`build:windows` produces a raw Windows `.exe`, not a signed installer or updater package.
tagged GitHub releases publish raw desktop archives plus matching `.sha256` checksum sidecars.

## maintainer docs
- `docs/CONVENTIONS.md`
- `docs/ARCHITECTURE.md`
- `plans/RUST-REWRITE.md`
