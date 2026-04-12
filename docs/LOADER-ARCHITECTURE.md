# Loader Architecture

This is the modloader shape used by Croopor.

The goal is simple:

- upstream APIs stay isolated
- Croopor works on normalized component and build records
- install behavior is selected by strategy, not by scattered `if` chains
- the launcher sees the result as a normal installed version

## Core model

Croopor treats loaders as components.

Current component ids:

- `net.fabricmc.fabric-loader`
- `org.quiltmc.quilt-loader`
- `net.minecraftforge`
- `net.neoforged`

Each installable loader build is a normalized `LoaderBuildRecord`.

Important fields:

- `component_id`
- `build_id`
- `minecraft_version`
- `loader_version`
- `version_id`
- `strategy`
- `artifact_kind`
- `install_source`

`build_id` is Croopor-owned. It is the stable selection key for install work.

`version_id` is the installed local version id written into `versions/`.

## Layers

### 1. Provider layer

Files:

- `core/minecraft/src/loaders/providers/mod.rs`
- `core/minecraft/src/loaders/providers/common.rs`
- `core/minecraft/src/loaders/providers/fabric.rs`
- `core/minecraft/src/loaders/providers/quilt.rs`
- `core/minecraft/src/loaders/providers/forge.rs`
- `core/minecraft/src/loaders/providers/neoforge.rs`

Responsibility:

- fetch raw upstream endpoints
- parse upstream payloads
- normalize into Croopor build records

This layer knows upstream wire shapes.

It does not install anything.

### 2. Index layer

Files:

- `core/minecraft/src/loaders/index/mod.rs`
- `core/minecraft/src/loaders/index/cache.rs`
- `core/minecraft/src/loaders/index/normalize.rs`
- `core/minecraft/src/loaders/index/query.rs`

Responsibility:

- cache normalized metadata
- track freshness and stale fallback
- expose:
  - components
  - supported Minecraft versions per component
  - builds for `(component, mc_version)`
  - one resolved build record for install

This layer is the backend source of truth for loader selection.

### 3. Strategy layer

Files:

- `core/minecraft/src/loaders/strategies/mod.rs`
- `core/minecraft/src/loaders/strategies/common.rs`
- `core/minecraft/src/loaders/strategies/fabric_profile.rs`
- `core/minecraft/src/loaders/strategies/quilt_profile.rs`
- `core/minecraft/src/loaders/strategies/forge_modern.rs`
- `core/minecraft/src/loaders/strategies/forge_legacy_installer.rs`
- `core/minecraft/src/loaders/strategies/forge_earliest_legacy.rs`
- `core/minecraft/src/loaders/strategies/neoforge_modern.rs`

Responsibility:

- install one normalized build
- choose behavior from `LoaderInstallStrategy`
- keep loader-family and era-specific work local to the selected strategy

Shared helpers stay in `common.rs`.

### 4. Helper layers

Files:

- `core/minecraft/src/loaders/api.rs`
- `core/minecraft/src/loaders/types.rs`
- `core/minecraft/src/loaders/artifacts/*`
- `core/minecraft/src/loaders/workspace/*`
- `core/minecraft/src/loaders/legacy/*`
- `core/minecraft/src/loaders/compose.rs`
- `core/minecraft/src/loaders/forge_installer.rs`
- `core/minecraft/src/loaders/processors.rs`

Responsibility:

- `api.rs`: component ids, build ids, version-id inference
- `types.rs`: normalized types and errors
- `artifacts/*`: artifact classification and source helpers
- `workspace/*`: Croopor-owned cache and work paths
- `legacy/*`: legacy-specific boundaries
- `compose.rs`: profile fragment composition
- `forge_installer.rs`: Forge installer extraction and legacy rewrite rules
- `processors.rs`: processor execution

## Forge split

Forge is not one installer path.

Croopor treats it as three eras:

1. earliest pre-installer Forge
2. legacy installer/FML Forge
3. modern processor-based Forge

That split is required from the start.

It prevents fake assumptions like:

- every Forge build has an installer jar
- every Forge artifact is downloadable
- every Forge version uses the same processor or library rules

## API shape

Current backend endpoints:

- `GET /api/v1/loaders/components`
- `GET /api/v1/loaders/components/{id}/builds?mc_version=...`
- `POST /api/v1/loaders/install`
- `GET /api/v1/loaders/install/{id}/events`

Install requests use:

- `component_id`
- `build_id`

Route code does not inspect raw upstream payloads.

## Frontend model

The frontend is component-driven now.

Selection flow:

- pick a Minecraft version
- pick a loader component
- fetch build records for that pair
- pick a build record
- create the instance with `build.version_id`
- install using `component_id + build_id`

Complex async loader state lives in:

- `frontend/src/machines/new-instance-loader.ts`

Frontend loader API helpers live in:

- `frontend/src/loaders/api.ts`
- `frontend/src/loaders/view-model.ts`
- `frontend/src/loaders/types.ts`

The frontend should not parse composite ids as its main data model.

## Flow

```text
Frontend loader UI
  |
  v
/api/v1/loaders/components
/api/v1/loaders/components/{id}/builds
  |
  v
index/query
  |
  +--> providers/*
  |      |
  |      +--> raw upstream APIs
  |
  +--> normalized LoaderBuildRecord
            |
            v
      strategies/*
            |
            +--> artifacts/*
            +--> workspace/*
            +--> compose.rs
            +--> forge_installer.rs
            +--> processors.rs
            +--> legacy/*
            |
            v
      installed local version
            |
            v
      launcher treats it like a normal version
```

## Cache and workspace

Croopor stores loader data inside the library:

```text
cache/
  loaders/
    catalog/
    artifacts/
    work/
```

Meaning:

- `catalog/`: normalized metadata caches
- `artifacts/`: reusable installer jars and similar artifacts
- `work/`: install-scoped extraction and processor work

The loader system should not use generic OS temp directories for normal install work.

## Prism reference

PrismLauncher is kept as a local code reference at:

- `/tmp/PrismLauncher`

Useful reference files:

- `/tmp/PrismLauncher/launcher/ui/dialogs/InstallLoaderDialog.cpp`
- `/tmp/PrismLauncher/launcher/ui/pages/modplatform/CustomPage.cpp`
- `/tmp/PrismLauncher/launcher/minecraft/update/LegacyFMLLibrariesTask.cpp`

Use Prism for:

- component and metadata selection flow
- parent-version filtering
- loader-specific install boundaries
- legacy Forge/FML split

Prism is a reference for boundaries.

It is not the target design.

Croopor keeps its own normalized types, API shape, and strategy model.
