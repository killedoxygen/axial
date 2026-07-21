# Conventions
keep this short and real. if the codebase changes, update this file.

## State
- app runtime state lives in `frontend/src/store.ts`
- domain workflow state owned by a machine lives with that machine (accounts, skin wardrobe, downloads in `frontend/src/machines/downloads.ts`)
- download/install state has one active-download representation: the `activeDownload` signal fed by the backend queue view model; do not reintroduce local mirrors of it
- local prefs live in `frontend/src/state.ts`
- use signal reassignment or helpers in `frontend/src/actions.ts`
- do not bring back proxy state, manual rerender helpers, or nested in-place mutations that dodge reactivity

## Frontend
- prefer preact components over hand-built dom
- for UI layout/design work, read and follow `docs/DESIGN.md`
- keep modules flat, named exports only
- no classes, no default exports
- use signals/actions for cross-module state, not custom event spaghetti
- keep complex async workflows in small machine modules built on signals, not scattered local flags
- keep workflow machines under `frontend/src/machines/`
- frontend renders backend-authored readiness, safety, performance, install, operation, and notice state; do not move business policy into UI helpers
- do not classify process exits, parse raw JVM args for policy, decide install failure or retry policy, decide performance health, or choose Guardian/Healing notice precedence in frontend code
- loader UI logic should consume normalized backend records, not raw ids or raw provider payloads
- do not use composite version-id parsing as the main loader UI data model

## DOM and handlers
- wrap dom listeners with arrows, do not pass business functions directly to `addEventListener`
- if a handler needs context, read it from state, not from `dataset` as a fake state bus
- ids and classes stay kebab-case
- if helpers already exist, use `byId`, `$`, `$$`

## Sounds
- button sounds are owned by `bindButtonSounds()`
- if the thing is not a button, call `Sound.ui()` yourself when needed
- add new button sound mappings in `inferButtonSound()`, not at random callsites
- slider sounds go through `playSliderSound()`

## API
- backend surface is `/api/v1/*`
- json in, json out
- errors are `{\"error\":\"message\"}`
- launch/install progress uses sse in browser mode and the Tauri desktop event bridge on desktop
- update checks go through `/api/v1/update`; the in-app update flow is backend-owned via `/api/v1/update/download`, `/api/v1/update/flow`, and `/api/v1/update/apply`, and the frontend renders the backend-authored flow state
- loader selection uses component ids and build ids
- loader version pickers must be driven by per-component supported Minecraft versions, not the vanilla catalog
- route and frontend code must not inspect raw Fabric, Quilt, Forge, or NeoForge payloads
- public errors, notices, progress, operation state, proof exports, and logs must not echo raw paths, Java paths, JVM args, command lines, provider payloads, account ids, usernames, tokens, server addresses, or token-like strings
- use backend-owned DTO/view-model boundaries for user-facing safety copy; routes adapt Application/Guardian/Performance output instead of authoring policy ad hoc

## Feature flags
- the flag registry is `core/config/src/flags.rs`; adding a flag means adding a registry entry, and retiring one means deleting the entry
- stale user overrides self-prune through `AppConfig::normalized()`
- user overrides persist in `feature_overrides` in `config.json`
- flags are read and toggled only through `/api/v1/flags`
- frontend code must read flags through `flagEnabled` and toggle them through `setFlagOverride` in `frontend/src/flags.ts`
- do not fetch or cache flag state ad hoc
- flag precedence is user override, then registry default
- the Dev Lab owns lazy frontend flag loading; do not add flag loading to application startup or Settings
- dev-only flags never appear in release builds

## Telemetry
- telemetry events exist only in the closed vocabulary in `apps/api/src/observability/telemetry.rs`
- do not add ad hoc event emission or extra telemetry egress points
- frontend errors are reported only through `frontend/src/error-reporting.ts`; no ad hoc reporting
- every telemetry property value goes through the `TelemetryExport` redaction audience
- document new events in `docs/TELEMETRY.md` in the same change

## Backend layout
- the Rust rewrite lives under `apps/` and `core/`
- `apps/api` owns the local HTTP surface and static frontend serving
- `apps/desktop` owns the Tauri shell
- `core/launcher`, `core/minecraft`, `core/performance`, and `core/config` are the long-term Rust product logic crates
- if backend work is part of this branch, add it in Rust
- loader-specific install behavior belongs in `core/minecraft/src/loaders/strategies/`, not in route handlers
- Application owns workflow request/response contracts, operation ids, route orchestration, and backend-authored view models
- Execution owns primitive facts/effects only; it must not decide Guardian policy
- Guardian owns horizontal safety diagnosis, action selection, self-healing orchestration, failure-memory loop control, and backend-authored safety outcomes
- State owns sessions, operation journals, operation state, failure memory, proof persistence, and runtime admission/lifecycle coordination for identity-bound managed composition state
- Observability owns redaction, evidence tiers, local proof records, and the telemetry-safe export boundary
- Performance owns performance rules, plans, health and composition semantics, composition-managed mutation logic, rollback snapshots, and queued performance operations
- production managed-composition access starts from a canonical registered instance id and crosses the State-owned runtime authority; do not pass caller-supplied paths to core state, health, rollback, or mutation helpers
- unknown ownership is treated as user-owned; automatic repair needs owned state, journaling, redaction, and loop control

## Architecture docs
- `docs/README.md` is the docs entrypoint and ownership map
- `plans/stabilization/` is the ignored local stabilization spec and execution control plane; keep `execution/PROGRESS.md` current-only, not as a log
- `docs/ARCHITECTURE.md` must describe the current launcher pipeline, not an aspirational one
- if launch/install/settings/runtime architecture shifts, update `docs/ARCHITECTURE.md` in the same change
- if Guardian authority, self-healing, Healing scope, redaction, or safety policy changes, update `docs/GUARDIAN-ARCHITECTURE.md` in the same change
- if version classification, naming, or ordering architecture shifts, update `docs/VERSION-METADATA-ARCHITECTURE.md` in the same change
- if the docs structure changes, update `docs/README.md` in the same change
- use `docs/adr/` for major decisions that need rationale, not for current-state walkthroughs
- do not land architecture shifts without updating the matching docs

## Build shape
- frontend entry is `frontend/src/main.tsx`
- frontend CSS is imported through `frontend/src/styles.ts`; `frontend/static` contains source assets only
- production and watch builds atomically publish one complete, budget-checked generation under ignored `frontend/dist`; every generated and public file is owned by its deterministic receipt
- frontend generation mutations hold one OS-released, portable case-folded loopback lease and fail closed on contention
- Task-owned Cargo target writers and storage reports share one fail-fast loopback lease only within the same network namespace; direct Cargo, other namespaces, and orphaned Cargo remain unobserved
- storage source receipts admit fixed regular no-link inputs, hash owned descriptors, and bound observable reads; POSIX descriptor admission uses no-follow/nonblocking flags, while Windows verifies pre/post-open identity and retains the unavoidable reparse/open and kernel-stall boundary
- the Cargo wrapper isolates a detached process group and performs bounded tree termination for ordinary wrapper signals; POSIX group settlement is also probed after natural Cargo close, while Windows `taskkill` is snapshot-based because Node does not provide Job Object ownership
- a supervisor hard kill or failed tree-control proof can leave Cargo descendants outside cooperative ownership; the wrapper reports that boundary and does not claim quiescence for them
- standalone API builds serve only the verified embedded generation; desktop API builds have no frontend fallback and Tauri owns `frontend/dist`
- frontend mock mode is build-time gated via `__AXIAL_MOCK_API__` and lives at the `api()` seam in `frontend/src/mock/`; run it with `task dev:web:mock`
- frontend package manager is `pnpm`, pinned through `frontend/package.json`
- exact Node, pnpm, Rust, Task, Tauri, container, and base-image identities are owned
  by `toolchain.json`; tracked mirrors and active executables are verified through
  `scripts/toolchain.mjs`, never widened with version ranges or mutable tags
- `Cargo.toml` is the sole authored application release version; a release tag
  must match it and one dated `CHANGELOG.md` section before publication
- frontend tests are discovered recursively by `frontend/test/run.mjs`; targeted
  runs select one exact inventory member through `task frontend:test TEST=...`
- capability proofs run only through the closed registry in `scripts/capabilities/`
  and only the dispatcher may publish verified evidence under `evidence/capabilities/`
- frontend formatting uses Prettier from `frontend/`; run `pnpm run format:check` to check and `pnpm run format` to write
- Biome owns only the configured hook-order and floating-promise semantic rules; Prettier remains the sole formatter
- the Rust workspace root is `Cargo.toml`
- local dev commands live in `Taskfile.yml` and run through `task` on all OSes; desktop dev and bundling go through `tauri-cli` (`task dev`, `task bundle`)
- release/build automation lives in `.github/workflows/`
- Rust build output lives in `target/`
- local release staging lives in `dist/`

## Inputs
- text and number inputs should use `autocomplete="off"`
- same goes for dynamic modal/dialog inputs

## repo attitude
- pre-release codebase
- prefer cleanup over compatibility shims
- if some old path is dead, delete it instead of half-maintaining it
