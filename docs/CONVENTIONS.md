# Conventions
keep this short and real. if the codebase changes, update this file.

## State
- app runtime state lives in `frontend/src/store.ts`
- local prefs live in `frontend/src/state.ts`
- use signal reassignment or helpers in `frontend/src/actions.ts`
- do not bring back proxy state, manual rerender helpers, or nested in-place mutations that dodge reactivity

## Frontend
- prefer preact components over hand-built dom
- keep modules flat, named exports only
- no classes, no default exports
- use signals/actions for cross-module state, not custom event spaghetti
- keep complex async workflows in small machine modules built on signals, not scattered local flags
- keep workflow machines under `frontend/src/machines/`
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
- update checks go through `/api/v1/update`
- loader selection uses component ids and build ids
- loader version pickers must be driven by per-component supported Minecraft versions, not the vanilla catalog
- route and frontend code must not inspect raw Fabric, Quilt, Forge, or NeoForge payloads

## Backend layout
- the Rust rewrite lives under `apps/` and `core/`
- `apps/api` owns the local HTTP surface and static frontend serving
- `apps/desktop` owns the Tauri shell
- `core/launcher`, `core/minecraft`, `core/performance`, and `core/config` are the long-term Rust product logic crates
- if backend work is part of this branch, add it in Rust
- loader-specific install behavior belongs in `core/minecraft/src/loaders/strategies/`, not in route handlers

## Architecture docs
- `docs/README.md` is the docs entrypoint and ownership map
- `docs/ARCHITECTURE.md` must describe the current launcher pipeline, not an aspirational one
- if launch/install/settings/runtime architecture shifts, update `docs/ARCHITECTURE.md` in the same change
- if Guardian authority, Healing scope, or launch-safety policy changes, update `docs/GUARDIAN-ARCHITECTURE.md` in the same change
- if version classification, naming, or ordering architecture shifts, update `docs/VERSION-METADATA-ARCHITECTURE.md` in the same change
- if the docs structure changes, update `docs/README.md` in the same change
- use `docs/adr/` for major decisions that need rationale, not for current-state walkthroughs
- do not land architecture shifts without updating the matching docs

## Build shape
- frontend entry is `frontend/src/main.tsx`
- frontend bundle output is `frontend/static/app.js`
- frontend package manager is `pnpm`, pinned through `frontend/package.json`
- the Rust workspace root is `Cargo.toml`
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
