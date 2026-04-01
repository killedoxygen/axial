# croopor
offline minecraft launcher, built as a wails desktop app.

it already handles:
- multi-instance installs
- vanilla + fabric + quilt + forge + neoforge
- java runtime detection and download flows
- launch sessions, logs, install progress
- local music, themes, shortcuts, onboarding

## stack
- desktop shell: wails
- frontend: preact + signals
- backend: go + `/api/v1/*`
- desktop event streaming: wails runtime events
- browser mode: sse

## prereqs
- go 1.25+
- node 22+
- ubuntu 24.04 linux desktop builds need `libgtk-3-dev` and `libwebkit2gtk-4.1-dev`

you do **not** need to install `task`, `pnpm`, `wails`, or `goreleaser` globally for normal local work.

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

the repo bootstraps its own local tools into `.tools/bin`.
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
- `setup`: install go deps, frontend deps, and the local wails cli
- `dev`: run desktop dev with wails
- `dev-web`: run the frontend-only dev server
- `dev-windows`: build and launch the windows dev binary
- `watch`: rebuild frontend assets on file changes
- `build`: build the native release binary for this machine
- `build-dev`: build the native dev binary for this machine
- `build-windows`: cross-build a windows amd64 release binary
- `build-windows-dev`: cross-build a windows amd64 dev binary
- `verify`: run checks, tests, and native builds
- `doctor`: show detected tools and platform state
- `clean`: remove build outputs and go caches

examples:

```bash
./dev setup
./dev dev-web
./dev dev-windows
./dev watch
./dev build-dev
./dev verify
```

## wsl note
`dev` needs a gui session. with wsl that means wslg or another linux gui path.

if you are in headless wsl, use:

```bash
./dev dev-web
```

if you want the windows app from wsl instead of the linux desktop app:

```bash
./dev dev-windows
```

frontend dev server port is configurable:

```bash
PORT=3001 ./dev dev-web
```

## direct task usage
`task` is the workflow engine, not the required user entrypoint.

if you really want to use it directly after setup:

```bash
.tools/bin/task --list
```

## release
tag pushes build release artifacts and updater metadata.

current release output:
- raw binaries through goreleaser
- linux amd64 appimage
- windows msix + appinstaller files for internal updater validation
- github pages update metadata at `updates/stable.json`

local snapshot:

```bash
./dev release-snapshot
```

## maintainer docs
- `docs/CONVENTIONS.md`
- `docs/ARCHITECTURE.md`
