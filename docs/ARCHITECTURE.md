# Architecture

Axial is one desktop process: a Tauri shell hosts the Preact interface, which talks to an in-process HTTP API on loopback.

| Path | Role |
|---|---|
| `apps/desktop` | Tauri shell: windows, native sign-in, updater, Discord presence and process lifetime. |
| `apps/api` | Authenticated loopback HTTP API; thin adapters over `core/app`. Also runs headless (`task dev:api`). |
| `core/app` | Application features, one module per feature (`install`, `launch`, `instances`, `content`, `accounts`, `settings`, ...). Owns SQLite state, OS-keyring credentials, operation recovery and the public wire types. |
| `core/minecraft` | Minecraft and loader domain: version metadata, downloads, Java runtimes, loader installers and processors, managed game-file publication. |
| `core/fs` | Capability-scoped filesystem: root/lease admission, staged writes, atomic promotion, observation windows and crash-recoverable file effects. |
| `core/resource` | Process-wide physical work admission (worker and I/O budgets). |
| `core/performance` | Performance mode catalog, rule resolution and managed mod bundles. |
| `frontend` | Preact interface. `frontend/src/generated` holds wire types generated from `core/app` (`task contracts:generate`). |
| `acceptance` | Fixtures and fake providers used by integration tests. |
| `scripts` | Build, release, dependency-policy and asset-provenance tooling. |
| `legacy` | The previous implementation, kept for reference only. It is not a build dependency. |

## Key rules

- Each behaviour, piece of state and wire contract has one owner module. HTTP routes do not contain product logic.
- All game-file writes go through `core/fs` capabilities: stage, verify, then promote. Interrupted operations are settled on the next start from their durable records.
- Wire types are generated from Rust; the frontend never hand-mirrors backend state.
