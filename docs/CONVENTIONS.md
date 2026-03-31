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
- launch/install progress uses sse in browser mode, wails runtime events in desktop mode
- update checks go through `/api/v1/update`

## Build
- frontend entry is `frontend/src/main.tsx`
- output is `frontend/static/app.js`
- frontend package manager is `pnpm`, pinned through `frontend/package.json`
- run frontend commands through `corepack pnpm`, do not assume a global `pnpm` shim
- workflow definitions live in `Taskfile.yml`
- main local entrypoints are `./dev` on unix/wsl and `dev.ps1` or `dev.cmd` on windows
- `make` is a fallback path, not the main daily interface
- the repo bootstraps local tools into `.tools/bin`, do not rely on random global `task`, `wails`, or `goreleaser` installs
- frontend installs should use the lockfile and `--ignore-scripts`
- desktop build is wails
- on ubuntu 24 the linux build uses `webkit2_41`
- local dev commands live in `Taskfile.yml`
- `Makefile` is only a unix/wsl convenience shim
- raw release binaries are driven by `.goreleaser.yml`
- extra release packaging and updater metadata live in `.github/workflows/release.yml`

## Inputs
- text and number inputs should use `autocomplete="off"`
- same goes for dynamic modal/dialog inputs

## repo attitude
- pre-release codebase
- prefer cleanup over compatibility shims
- if some old path is dead, delete it instead of half-maintaining it
