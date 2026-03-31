# croopor
offline minecraft launcher, built as a wails desktop app.

it already handles:
- multi-instance installs
- vanilla + fabric + quilt + forge + neoforge
- java runtime detection and download flows
- launch sessions, logs, install progress
- local music, themes, shortcuts, onboarding

it does **not** handle microsoft account auth, so for online-mode stuff you need the official launcher.

## state
current stack:
- desktop shell is wails
- frontend state is mostly signal-driven
- backend exposes `/api/v1/*`
- desktop event streaming uses wails runtime events
- browser mode uses sse

## prereqs
- go 1.25+
- node 22+
- npm 10+
- task v3
- wails cli `v2.11.0`
- goreleaser v2 if you want local release snapshots

ubuntu 24.04:

```bash
sudo apt-get update
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev
go install github.com/go-task/task/v3/cmd/task@latest
go install github.com/wailsapp/wails/v2/cmd/wails@v2.11.0
go install github.com/goreleaser/goreleaser/v2@latest
```

if `go install` put `task` in `~/go/bin`, `make` will pick it up even if your shell PATH was not reloaded yet.

## quickstart
first time:

```bash
make setup
```

daily app dev:

```bash
make dev
```

native local builds:

```bash
make build
make build-dev
```

see everything:

```bash
make help
```

## cli
`task` is the real interface. `make` is only a small compatibility shim for the common commands.

daily commands:

```bash
task build
task build:dev
task wails:dev
task verify
```

same thing through `make`:

```bash
make build
make build-dev
make dev
make verify
```

first time setup:

```bash
task frontend:install
# or
make setup
```

frontend only:

```bash
task frontend:serve
task frontend:check
task frontend:build
```

desktop app:

```bash
task test
task wails:dev
task wails:build
task build
task build:dev
```

windows cross-builds:

```bash
task build:windows
task build:windows:dev
```

full local verification:

```bash
task check
task test
task verify
```

release snapshot:

```bash
task release:snapshot
```

## maintainer docs
- `docs/CONVENTIONS.md`
- `docs/ARCHITECTURE.md`

## release
tag push builds release artifacts through goreleaser.

```bash
git tag v1.1.0
git push --tags
```

local snapshot:

```bash
task release:snapshot
```

## roadmap
- msa auth
- modrinth-powered bundles
- skin stuff
