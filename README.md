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
- desktop shell: tauri
- frontend: preact + signals
- backend: rust workspace under `apps/` and `core/`
- desktop event streaming: tauri-native bridge
- browser mode: sse

## prereqs
- rust stable with `rustfmt` and `clippy`
- node 22+
- ubuntu 24.04 linux desktop builds need `libgtk-3-dev` and `libwebkit2gtk-4.1-dev`

you do **not** need to install `task` or `pnpm` globally for normal local work.
you also do not need a global cargo plugin setup.

ubuntu 24.04 quick prereqs:

```bash
sudo apt-get update
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev
```

## cli
main entrypoints:
- unix, mac, wsl: `./dev`
- windows powershell: `.\dev.ps1`
- windows cmd: `dev.cmd`

extra compatibility path:
- unix, mac, wsl: `make`

## quickstart
unix, mac, wsl:

```bash
./dev setup
./dev help
./dev dev
```

windows powershell:

```powershell
.\dev.ps1 setup
.\dev.ps1 help
.\dev.ps1 dev
```

## common commands
- `setup`: install frontend deps and prefetch Rust deps
- on Linux/WSL, `setup` also prepares the Windows GNU target and MinGW linker for cross-builds
- `dev`: run desktop dev with Rust + Tauri
- `dev-web`: run the frontend-only dev server
- `watch`: rebuild frontend assets on file changes
- `check`: run `fmt`, `check`, `clippy`, and frontend typecheck
- `test`: run the Rust workspace tests
- `verify`: run checks, tests, frontend build, and a release desktop build
- `rust:fmt`: run Rust formatting checks
- `rust:fmt:fix`: format Rust code
- `rust:check`: typecheck the Rust workspace
- `rust:clippy`: run clippy with warnings denied
- `rust:test`: run the Rust workspace tests
- `rust:api`: run the Rust Axum API
- `rust:desktop`: run the Rust Tauri desktop shell
- `build`: build the release desktop binary
- `build-dev`: build the dev desktop binary
- `build --target windows`: build the release Windows desktop binary from Linux/WSL
- `build-dev --target windows`: build the dev Windows desktop binary from Linux/WSL
- `build:windows`: explicit alias for the release Windows cross-build
- `build:windows:dev`: explicit alias for the dev Windows cross-build
- `build:api`: build the dev API binary
- `build:api:release`: build the release API binary
- `doctor`: show detected tools and platform state
- `clean`: remove `target/` and `dist/`

examples:

```bash
./dev setup
./dev dev-web
./dev watch
./dev build-dev
./dev build --target windows
./dev verify
```

## wsl note
`dev` needs a gui session. with wsl that means wslg or another linux gui path.

if you are in headless wsl, use:

```bash
./dev dev-web
```

frontend dev server port is configurable:

```bash
PORT=3001 ./dev dev-web
```

windows cross-build note:

```bash
./dev setup
./dev build --target windows
```

on Ubuntu/WSL, `setup` installs the Rust target and `gcc-mingw-w64-x86-64` by default unless `CI=true`.
this currently builds a raw Windows `.exe`, not a signed installer or updater package.

## taskfile
`Taskfile.yml` mirrors the same commands as `./dev`, but it is optional.

if you already have `task` installed and prefer it:

```bash
task --list
```

## maintainer docs
- `docs/CONVENTIONS.md`
- `docs/ARCHITECTURE.md`
- `plans/RUST-REWRITE.md`
