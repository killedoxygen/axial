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
- wails cli `v2.11.0`

ubuntu 24.04:

```bash
sudo apt-get update
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev
go install github.com/wailsapp/wails/v2/cmd/wails@v2.11.0
```

## dev
install deps once:

```bash
make frontend-install
```

run the app:

```bash
make dev
```

frontend-only server:

```bash
make serve
```

## build
normal build:

```bash
make build
```

dev-tag build:

```bash
make build-dev
make build-dev-windows
```

wails production build:

```bash
make wails-build
```

## verify
full local pass:

```bash
make verify
```

useful smaller targets:

```bash
make check
make test
make frontend-build
```

## roadmap
- msa auth
- modrinth-powered bundles
- skin stuff

## maintainer docs
- `docs/CONVENTIONS.md`
- `docs/ARCHITECTURE.md`

## release
tag push builds release artifacts.

```bash
git tag v1.1.0
git push --tags
```
