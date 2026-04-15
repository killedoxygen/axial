# Architecture
This is the current map of the launcher. Keep it accurate. If the architecture changes, update this file in the same change.

## Topology
- `frontend/`: Preact UI, state, launch/install workflows, browser + desktop runtime integration
- `apps/api`: local Axum HTTP surface and SSE endpoints under `/api/v1/*`
- `apps/desktop`: Tauri shell and native event bridge
- `core/config`: config model, normalization, persistence, path detection
- `core/launcher`: launch pipeline, Guardian, Healing, command planning, session/status mapping
- `core/minecraft`: version metadata, runtime discovery/install, download/install, loader strategies
- `core/performance`: managed performance planning/install

## Primary docs
- Docs index: `docs/README.md`
- Guardian architecture: `docs/GUARDIAN-ARCHITECTURE.md`
- Loader architecture: `docs/LOADER-ARCHITECTURE.md`
- Version metadata architecture: `docs/VERSION-METADATA-ARCHITECTURE.md`
- ADRs: `docs/adr/`

## Frontend map
- `frontend/src/main.tsx`: app bootstrap
- `frontend/src/store.ts`: runtime state
- `frontend/src/actions.ts`: state transitions
- `frontend/src/launch.ts`: launch request, status/log subscription, failure handling
- `frontend/src/install.ts`: install workflow
- `frontend/src/settings.ts`: settings draft + save flow
- `frontend/src/native.ts`: desktop event bridge
- `frontend/src/machines/`: workflow machines that should hold complex async state

## Backend map
- `apps/api/src/routes/launch/`: launch route, task assembly, streaming, runner
- `apps/api/src/state/sessions/`: live launch session store, subscriptions, process supervision
- `core/launcher/src/guardian/`: launch-safety authority and intervention model
- `core/launcher/src/service/`: launch preparation, mappings, Healing summary/recovery helpers
- `core/minecraft/src/runtime/`: runtime discovery and managed runtime installation
- `core/minecraft/src/version_meta/`: version classification, effective-version resolution, display metadata, deterministic ordering

## Full launcher pipeline

### High-level launcher lifecycle
```mermaid
flowchart TD
    A[App starts] --> B[frontend/src/main.tsx bootstraps UI]
    B --> C[Load config, system info, versions, instances, launch status]
    C --> D{Setup/onboarding needed?}
    D -->|yes| E[Setup flow chooses managed or existing library]
    D -->|no| F[Launcher UI becomes interactive]
    E --> F
    F --> G{User action}
    G -->|Install| H[Install pipeline]
    G -->|Launch| I[Launch pipeline]
    G -->|Settings| J[Config update pipeline]
    G -->|Update check| K[Update pipeline]
```

### Launch pipeline: end-to-end
```mermaid
flowchart TD
    A[User clicks Play] --> B[frontend/src/launch.ts persists dirty per-instance overrides]
    B --> C[POST /api/v1/launch]
    C --> D[apps/api routes/launch/task.rs validates request and builds LaunchIntent]
    D --> E[Create LaunchGuardianContext from config + instance overrides]
    E --> F[Reserve queued session in SessionStore]
    F --> G[routes/launch/runner.rs starts synchronous launch flow]
    G --> H[emit status: validating]
    H --> I[core/launcher service prepares attempt]
    I --> J[resolve version metadata]
    J --> K[collect runtime facts and effective runtime candidate]
    K --> L[Guardian-driven pre-launch decision]
    L -->|block| M[return HTTP error with guardian + healing]
    L -->|intervene| N[mutate attempt overrides and re-run prepare]
    L -->|allow| O[plan launch command]
    N --> I
    O --> P[spawn process via SessionStore]
    P --> Q[emit starting + monitoring status]
    Q --> R[wait_for_startup observation window]
    R -->|stable or timed out| S[return HTTP success with pid + guardian + healing]
    R -->|stalled or exited| T[collect failure observations]
    T --> U[Guardian decides whether startup recovery is allowed]
    U -->|recover| V[apply one startup recovery plan and retry]
    U -->|block| W[emit terminal failure + guardian guidance]
    V --> I
```

### Launch pipeline: backend detail
```mermaid
flowchart TD
    A[LaunchIntent] --> B[prepare_launch_attempt]
    B --> C[resolve_version]
    C --> D[runtime fact gathering]
    D --> E[manual override fact gathering]
    E --> F[Guardian evaluates facts]
    F -->|allow| G[compute effective preset + JVM args]
    F -->|switch runtime| H[set force_managed_runtime]
    F -->|strip raw JVM args| I[set ignore_extra_jvm_args]
    F -->|downgrade preset| J[set preset_override]
    F -->|disable custom GC| K[set disable_custom_gc]
    F -->|block| L[LaunchPreparationError + Guardian guidance]
    H --> B
    I --> B
    J --> B
    K --> B
    G --> M[plan_resolved_launch]
    M --> N[PreparedLaunchAttempt]
```

### Live session and event flow
```mermaid
flowchart TD
    A[SessionStore.start_process] --> B[store pid + command + guardian + healing]
    B --> C[spawn stdout/stderr pumps]
    B --> D[spawn startup watchdog]
    B --> E[spawn wait task]
    C --> F[emit log events]
    F --> G[update startup observation markers]
    D --> H{startup seen?}
    H -->|no| I[emit stalled terminal status]
    H -->|yes| J[wait]
    E --> K[process exits]
    K --> L[emit exited status]
    F --> M[SSE stream and Tauri bridge deliver logs]
    L --> N[SSE stream and Tauri bridge deliver status]
    N --> O[frontend/src/launch.ts updates runningSessions]
    O --> P[UI tears down session or keeps monitoring]
```

### Frontend launch flow
```mermaid
flowchart TD
    A[launch.ts launchGame] --> B[save dirty overrides if needed]
    B --> C[POST /launch]
    C --> D{HTTP result}
    D -->|error| E[render guardian/healing failure notice]
    D -->|success| F[store running session]
    F --> G[connect SSE or Tauri event stream]
    G --> H[status updates patch runningSessions]
    G --> I[log updates append launch log]
    H --> J{terminal status?}
    J -->|no| K[keep monitoring]
    J -->|yes| L[render terminal notice and clear session]
```

### Config/settings flow
```mermaid
flowchart TD
    A[User edits settings] --> B[frontend/src/settings.ts saves draft]
    B --> C[PUT /api/v1/config]
    C --> D[core/config AppConfig normalized]
    D --> E[ConfigStore writes config.json]
    E --> F[frontend store updates live config]
```

### Install flow
```mermaid
flowchart TD
    A[frontend install.ts queues install] --> B[POST /api/v1/install]
    B --> C[apps/api install route creates install session]
    C --> D[core/minecraft resolves downloads and loader strategy]
    D --> E[progress events emitted over SSE or Tauri]
    E --> F[frontend updates progress UI]
    F --> G[frontend refreshes versions/catalog/instance state]
```

### Version metadata pipeline
```mermaid
flowchart TD
    A[Raw version id from manifest/local scan/loader provider] --> B[tokenize.rs builds typed token stream]
    B --> C[parse.rs strips known variant suffixes and detects shape]
    C --> D[mod.rs maps shape to canonical kind and family]
    D --> E[mod.rs resolves effective version]
    E --> F[mod.rs builds display_name and display_hint]
    F --> G[mod.rs applies deterministic ordering]
    G --> H[attach VersionMeta to catalog, installed version, or loader version record]
    H --> I[frontend renders backend metadata without re-parsing vanilla-like ids]
```

## Launch authority boundaries
- Guardian is the authority for launch-safety policy.
- Healing is a capability used by Guardian, not the authority.
- Runtime/JVM/validation layers should produce facts and execution helpers, not user-policy decisions.
- Session heuristics are observations. They should not invent user-policy outcomes on their own.
- The frontend should render backend-authored Guardian outcomes, not reinterpret policy locally.

## Where to look
- launch behavior: `apps/api/src/routes/launch/`, `apps/api/src/state/sessions/`, `core/launcher/`, `core/minecraft/src/runtime/`
- config/settings: `core/config/`, `frontend/src/settings.ts`
- install flow: `apps/api/src/routes/install.rs`, `core/minecraft/`, `frontend/src/install.ts`
- version analysis: `core/minecraft/src/version_meta/`, `apps/api/src/routes/catalog.rs`, `apps/api/src/routes/versions.rs`, `core/minecraft/src/loaders/index/query.rs`
- desktop bridge: `apps/desktop/`, `frontend/src/native.ts`

## Current architectural pressure points
- Guardian authority is still being tightened across runtime, Healing, session heuristics, and frontend rendering.
- Session startup/failure inference still depends on log heuristics.
- Update flow exists but is still not a full native updater/distribution pipeline.
