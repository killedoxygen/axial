# Architecture
Very small map so it is obvious where to start. weirdly enough, this document was written by a human this time

## desktop side
- `main.go`: wails bootstrap
- `app.go`: bindings exposed to the frontend
- `internal/server`: api routes, session/install managers, wails bridge helpers
- `internal/update`: release manifest fetch + version resolution

the backend serves `/api/v1/*` inside wails too.

## frontend side
- `frontend/src/main.tsx`: bootstrap
- `frontend/src/components/App.tsx`: app shell
- `frontend/src/store.ts`: runtime state
- `frontend/src/actions.ts`: state transitions
- `frontend/src/native.ts`: wails runtime bridge
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
- `internal/minecraft`: vanilla metadata, downloads, java, integrity
- `internal/modloaders`: fabric/quilt/forge/neoforge
- `internal/launcher`: command building, process lifecycle, profiling
- `internal/config`: config persistence
- `internal/instance`: instance storage

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
3. progress comes through sse or wails events
4. frontend refreshes versions and catalog flags

Launch:
1. frontend posts to `/launch`
2. running session goes into `store.ts`'s state machine
3. logs and status stream in
4. explicit backend exit status is the source of truth for teardown

Update check:
1. frontend waits until bootstrap is ready
2. desktop runtime does a quiet `/api/v1/update` check
3. backend fetches the stable Pages manifest
4. frontend shows a quiet CTA if a newer build exists

## where to look
- install bugs: `internal/minecraft`, `internal/modloaders`, `frontend/src/install.ts`
- launch bugs: `internal/launcher`, `internal/server/api.go`, `frontend/src/launch.ts`
- settings/prefs: `internal/config/config.go`, `frontend/src/settings.ts`, `frontend/src/state.ts`
- updater/release metadata: `internal/update`, `frontend/src/updater.ts`, `.github/workflows/release.yml`
- shell/layout: `frontend/src/components/App.tsx`
- wails integration: `main.go`, `app.go`, `wails.json`, `frontend/src/native.ts`

hard todo:
- windows native updater packaging is only public when real production signing exists
- do not expose dev/test/self-signed msix or appinstaller artifacts to users
