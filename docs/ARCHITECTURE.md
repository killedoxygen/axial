# Architecture
Very small map so it is obvious where to start. weirdly enough, this document was written by a human this time

## desktop side
- `main.go`: wails bootstrap
- `app.go`: bindings exposed to the frontend
- `internal/server`: api routes, session/install managers, wails bridge helpers

the backend serves `/api/v1/*` inside wails too.

## frontend side
- `frontend/src/main.tsx`: bootstrap
- `frontend/src/components/App.tsx`: app shell
- `frontend/src/store.ts`: runtime state
- `frontend/src/actions.ts`: state transitions
- `frontend/src/native.ts`: wails runtime bridge

main workflow files:
- `install.ts`
- `launch.ts`
- `sidebar.ts`
- `settings.ts`
- `modals.ts`
- `context-menu.ts`

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

## where to look
- install bugs: `internal/minecraft`, `internal/modloaders`, `frontend/src/install.ts`
- launch bugs: `internal/launcher`, `internal/server/api.go`, `frontend/src/launch.ts`
- settings/prefs: `internal/config/config.go`, `frontend/src/settings.ts`, `frontend/src/state.ts`
- shell/layout: `frontend/src/components/App.tsx`
- wails integration: `main.go`, `app.go`, `wails.json`, `frontend/src/native.ts`
