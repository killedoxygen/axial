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

## Building

Requires Go 1.23+ and Node.js 18+.

```bash
cd frontend && npm install && npm run build && cd ..

# Production (Windows, no console)
GOOS=windows GOARCH=amd64 go build -trimpath -ldflags="-s -w -H windowsgui" -o croopor.exe .

# Development (includes dev tools)
GOOS=windows GOARCH=amd64 go build -tags dev -o croopor.exe .
```

On Windows the app opens a native WebView2 window. On other platforms it falls back to a browser tab.

## Releasing

```bash
git tag v1.1.0 && git push --tags
```

GitHub Actions builds Windows (amd64, arm64) and Linux (amd64) binaries and publishes them as a release.

## License

See repository for license information.
