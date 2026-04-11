# Architecture
Very small map so it is obvious where to start. weirdly enough, this document was written by a human this time

## desktop side
- runtime: `apps/api` + `apps/desktop`
- the backend serves `/api/v1/*`

## frontend side
- `frontend/src/main.tsx`: bootstrap
- `frontend/src/components/App.tsx`: app shell
- `frontend/src/store.ts`: runtime state
- `frontend/src/actions.ts`: state transitions
- `frontend/src/native.ts`: desktop runtime bridge, Tauri-aware
- `frontend/src/updater.ts`: update checks, dismissal, desktop-only auto-check

main workflow files:
- `install.ts`
- `launch.ts`
- `sidebar.ts`
- `settings.ts`
- `modals.ts`
- `context-menu.ts`
- `updater.ts`

## packages that matter
- `apps/api`: Rust Axum server for `/api/v1/*` and static asset serving
- `apps/desktop`: Rust Tauri shell
- `core/config`: config store, paths, and model normalization
- `core/launcher`: launch pipeline, runtime selection, healing, process/session lifecycle
- `core/minecraft`: vanilla metadata, downloads, Java/runtime discovery, loaders, installs
- `core/performance`: performance planning and policy surfaces

## runtime flow
Bootstrap:
1. app mounts
2. frontend loads config, system, status
3. setup/onboarding may run
4. frontend loads versions and instances
5. sidebar watch starts

Install:
1. frontend queues install work
2. backend starts a session
3. progress comes through sse or desktop-native events
4. frontend refreshes versions and catalog flags

Launch:
1. frontend posts to `/launch`
2. running session goes into `store.ts`'s state machine
3. logs and status stream in
4. explicit backend exit status is the source of truth for teardown

Update check:
1. frontend waits until bootstrap is ready
2. desktop runtime does a quiet `/api/v1/update` check
3. backend currently returns the local app version and no available update
4. release automation currently publishes raw desktop binaries, not a native updater feed

## where to look
- install bugs: `core/minecraft`, `apps/api/src/routes/install.rs`, `apps/api/src/routes/loaders.rs`, `frontend/src/install.ts`
- launch bugs: `core/launcher`, `core/minecraft`, `apps/api/src/routes/launch.rs`, `frontend/src/launch.ts`
- settings/prefs: `core/config`, `frontend/src/settings.ts`, `frontend/src/state.ts`
- updater/release metadata: `apps/api/src/routes/update.rs`, `frontend/src/updater.ts`, `.github/workflows/release.yml`, `README.md`
- shell/layout: `frontend/src/components/App.tsx`
- current desktop/runtime entrypoints: `apps/api`, `apps/desktop`, `core/*`, `plans/RUST-REWRITE.md`

hard todo:
- windows packaging is still just raw release binaries, not a signed native updater path
