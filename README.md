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

## dev
install deps once:

```bash
task frontend:install
```

run the app:

```bash
task wails:dev
```

frontend-only server:

```bash
task frontend:serve
```

## build
see what exists:

```bash
task --list-all
```

native builds:

```bash
task build
task build:dev
```

windows builds from any machine:

```bash
task build:windows
task build:windows:dev
```

wails production build:

```bash
task wails:build
```

## verify
full local pass:

```bash
task verify
```

useful smaller targets:

```bash
task check
task test
task frontend:build
```

## roadmap
- msa auth
- modrinth-powered bundles
- skin stuff

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

## make
`make` is only a small compatibility shim now. use `task` as the real interface.
