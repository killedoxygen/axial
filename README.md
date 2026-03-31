# Croopor

A standalone Minecraft launcher that fully replaces the official launcher for offline play. Manages versions, instances, Java runtimes, and mod loaders independently — no Mojang launcher required.

Croopor is currently a **paralauncher**: it runs alongside or instead of the official launcher, but doesn't yet support Microsoft account authentication. Online-mode server play requires a premium account through the official launcher. MSA authentication is on the roadmap to make Croopor a complete standalone launcher.

## Roadmap

| Status | Milestone |
|--------|-----------|
| Done | Multi-instance management with isolated game directories |
| Done | Boot optimization (CDS caching, CPU throttling, JVM tuning) |
| Planned | One-click performance mod bundles via Modrinth API |
| Planned | Microsoft account authentication (online-mode servers) |
| Planned | Skin viewer and management |

## Prerequisites

Croopor is now a Wails desktop app.

- Go 1.25+
- Node.js 22+
- npm 10+
- Wails CLI `v2.11.0`

Linux desktop builds also require GTK/WebKit development packages. On Ubuntu 24.04:

```bash
sudo apt-get update
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev
```

Install Wails:

```bash
go install github.com/wailsapp/wails/v2/cmd/wails@v2.11.0
```

## Development

Install frontend dependencies once:

```bash
make frontend-install
```

Run the desktop app with live reload:

```bash
make dev
```

This wraps `wails dev`. The frontend watcher writes `frontend/static/app.js`, the Go backend runs in-process, and the app opens through the Wails desktop runtime.

If you only want the standalone frontend server:

```bash
make serve
```

## Verification

Run the full local verification path:

```bash
make verify
```

Useful individual targets:

```bash
make check              # frontend typecheck + gofmt check
make test               # go test ./...
make build              # production desktop binary for the current platform
make build-dev          # current-platform binary with the dev tag
make build-dev-windows  # Windows amd64 binary with the dev tag
make wails-build        # production Wails build
```

## Building

Standard local build:

```bash
make build
```

Explicit Wails production build:

```bash
wails build -nopackage -m -v 1
```

Example Windows dev-tag build:

```bash
make build-dev-windows
```

## CI And Releases

- Pull requests and branch pushes run frontend checks, Go tests, Linux desktop builds, and `wails build`.
- Tag pushes create release artifacts.

```bash
git tag v1.1.0 && git push --tags
```

## License

See repository for license information.
