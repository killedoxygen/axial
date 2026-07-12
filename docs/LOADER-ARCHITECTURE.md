# Loader Architecture

This is the current modloader model used by Axial.

The important rule is:

- loaders are normalized by component and build
- user-facing loader labels come only from explicit upstream terms
- backend selection policy is separate from user-facing terms
- the frontend consumes normalized records and does not inspect raw upstream payloads

## Core model

Axial treats loaders as components.

Current component ids:

- `net.fabricmc.fabric-loader`
- `org.quiltmc.quilt-loader`
- `net.minecraftforge`
- `net.neoforged`

The two main normalized record types are:

- `LoaderGameVersion`
- `LoaderBuildRecord`

`LoaderGameVersion` answers:

- which Minecraft versions a loader component supports
- what the backend thinks that Minecraft version means

Important fields:

- `subject_kind = minecraft_version`
- `id`
- `release_time`
- `minecraft_meta`
- `lifecycle`

`LoaderBuildRecord` answers:

- which loader build is installable for one `(component, minecraft_version)` pair
- what explicit upstream terms it carries
- how the backend should rank it for default selection

Important fields:

- `subject_kind = loader_build`
- `component_id`
- `build_id`
- `minecraft_version`
- `loader_version`
- `version_id`
- `build_meta`
- `strategy`
- `artifact_kind`
- `install_source`

`build_id` is Axial-owned. It is the stable opaque selection key for install work. Current build
ids use the canonical `loader-build-v1-<base64url>` encoding: a domain/version marker, explicit
component byte, and length-prefixed exact UTF-8 Minecraft and loader versions. Delimiter-shaped
legacy ids are rejected rather than parsed or upgraded.

`version_id` is the installed local version id written into `versions/`.

Installed loader ids use a current-only `loader-v2-<base64url>` encoding. The encoded payload contains a domain/version marker, an explicit component byte, and length-prefixed exact UTF-8 Minecraft and loader versions. Construction is fallible: empty, non-canonical, control-bearing, or filesystem-oversized coordinates are rejected. The 123-byte id ceiling is shared with the known-good 128-byte path-segment ceiling so `<id>.json` is always representable. Every component, including NeoForge, is bound to both the Minecraft target and loader version. Code recomputes this identity; it does not parse the display shape as authority and there are no compatibility ids.

## Loader build metadata

Loader builds use `LoaderBuildMetadata`, not generic `LifecycleMeta`.

`LoaderBuildMetadata` has four concerns:

- `terms`
  - explicit normalized upstream terms exposed to the UI
- `evidence`
  - where a term came from, like `explicit_version_label`, `explicit_api_flag`, or `promotion_marker`
- `selection`
  - backend-owned default-selection policy
- `display_tags`
  - backend-authored badge strings derived from `terms`

Important rule:

- the UI renders `display_tags`
- the backend uses `selection`
- the UI does not infer extra loader meaning from missing terms

Current normalized terms are:

- `recommended`
- `latest`
- `snapshot`
- `pre_release`
- `release_candidate`
- `beta`
- `alpha`
- `nightly`
- `dev`

Examples:

- stable Fabric build with only `stable=true`
  - `terms = []`
  - `selection.reason = stable`
  - `selection.source = explicit_api_flag`
- recommended Forge build
  - `terms = [recommended]`
  - `evidence` includes `promotion_marker`
- latest Forge build with no recommended promotion
  - `terms = [latest]`
  - `selection.reason = latest_unstable`
  - `selection.source = absence_of_recommended`
- NeoForge beta
  - `terms = [beta]` or `[latest, beta]` depending on upstream markers

`latest` and `recommended` are explicit provider-facing terms. They are not generic maturity classes.

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
- normalize supported Minecraft versions and loader builds
- map upstream terms and evidence into `LoaderBuildMetadata`

This layer knows provider wire formats.

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

This layer is the backend source of truth for loader selection and build ordering.

Loader catalog caches store normalized metadata, not raw upstream payloads. The cache schema must be bumped whenever record semantics change, including lifecycle, stable/beta, promotion, default-selection, installability, or provider-term interpretation, so older normalized records cannot keep driving new UI or install behavior.

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

Profile-based loaders, currently Fabric and Quilt, download libraries declared by trusted upstream profile JSON. Those profile libraries may omit SHA-1 metadata. The loader strategy passes an explicit checksumless planning policy only at its typed install boundary; vanilla planning and launch readiness stay strict, and no provider payload or mutable installed metadata can grant checksum authority. Missing-checksum `.jar` profile libraries are not trusted solely because a file exists: existing and freshly downloaded jars must be structurally readable so stale bad cache entries are replaced. R1 currently has a temporary wiring gap: strict readiness blocks checksumless loader libraries until the sealed known-good inventory authorizes their exact paths and structural integrity contracts. `.axial-loader.json` never grants that authority. Loader strategies also validate base Minecraft dependencies before treating a base version as already installed: the base JSON, client jar, incomplete marker, and selected base libraries must be ready so a partially-installed vanilla base cannot produce a finalized loader profile with missing inherited libraries.

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

- `api.rs`: component ids, build ids, and installed version-id construction
- `types.rs`: normalized types and errors
- helper modules: install artifacts, work dirs, composition, legacy behavior, processors
- `forge_installer.rs` and `processors.rs`: parse installer ZIPs through bounded blocking work, with explicit decompressed-entry ceilings for profile JSON, embedded Maven libraries, and processor data extraction. Modern Forge/NeoForge installer profiles are not parsed as legacy root-library install profiles; legacy root-library extraction is a no-op unless the legacy `install` schema is present.

## Selection flow

Frontend flow:

1. pick a loader component
2. fetch supported Minecraft versions for that component
3. pick a Minecraft version from that supported set
4. fetch build records for that pair
5. submit the backend-authored selection id
6. the backend chooses the highest stable default build, falling back to the best provider-ranked unstable build when no stable build exists
7. create the instance with `build.version_id`
8. install using `component_id + build_id`

Version-level loader selections can choose unstable builds only through backend policy. If a supported Minecraft-version row only has beta/unstable loader builds, create-view keeps the row selectable and renders a backend-authored `Beta` tag. Provider-unlabeled non-beta builds, such as current Quilt loader rows, remain valid stable defaults. Exact `loader_build` selections still work for deliberate build selection, but beta-only Minecraft rows do not require a separate exact-build path. Create-view uses supported-version metadata plus fresh cached build metadata for expensive build-level exceptions, so slow provider lookups do not block source switching. Known build-level exception rows may receive conservative non-blocking tags when build metadata is not cached yet, but create-submit and install still perform the full build resolution and stale-catalog checks before accepting a selection.

Complex async loader state lives in:

- `frontend/src/machines/new-instance-loader.ts`

The frontend should not parse composite ids as its main loader data model.

## Forge split

Forge is not one installer path.

Axial still treats it as three eras:

1. earliest pre-installer Forge
2. legacy installer/FML Forge
3. modern processor-based Forge

That split is install-strategy architecture, not lifecycle architecture.

Earliest pre-installer Forge client archives are overlays, not complete replacement client jars. The strategy builds a temporary jar from the base Minecraft client plus the Forge archive, skipping signature metadata, then promotes the composed jar into the installed loader version.

## API shape

Current backend endpoints:

- `GET /api/v1/loaders/components`
- `GET /api/v1/loaders/components/{id}/game-versions`
- `GET /api/v1/loaders/components/{id}/builds?mc_version=...`
- `POST /api/v1/loaders/install`
- `GET /api/v1/loaders/install/{id}/events`

Install requests use:

- `component_id`
- `build_id`

Route code does not inspect raw upstream payloads.

## Installed versions

An installed loader profile is a materialized launch model that still declares its exact base through `inheritsFrom`. `axialMaterialized = true` is honored only when the strict current sidecar validates; it is not standalone authority. Install writes the sidecar while the directory is still incomplete and before verification, so any failed validation removes the incomplete install. The scanner attaches loader identity only when all of these agree exactly:

- the version directory and profile id equal the recomputed `loader-v2` id
- `.axial-loader.json` is the private current schema and contains only component id, Minecraft target, and loader version
- the profile's declared `inheritsFrom` equals the metadata Minecraft target

Metadata with an old shape, extra fields, a mismatched target, a false materialized marker, or an unrelated folder/profile id is malformed and grants no loader attachment. Materialized resolution also fails closed when that provenance is absent or invalid. Display metadata and `build_id` are derived locally; they are not persisted as mutable provenance. Core exposes a read-only validated provenance query for base-version policy such as legacy Forge signature checks; it grants no checksum authority.

When a loader build is installed, the resulting `VersionEntry` carries:

- Minecraft `minecraft_meta`
- Minecraft `lifecycle`
- optional `loader` attachment

The loader attachment carries:

- `component_id`
- `component_name`
- `build_id`
- `loader_version`
- `build_meta`

That keeps Minecraft-version lifecycle and loader-build terms separate in the UI.

Install strategies also write `versions/<id>/.axial-loader.json` beside the composed version JSON. Version scanning reads that file as the authoritative installed-loader attachment source, so routes do not infer loader identity, Minecraft version, or loader version from opaque local version ids.

The installed-version scanner anchors Minecraft metadata for loader versions to the exact inherited/base Minecraft id declared by the version JSON and validated by `.axial-loader.json`. Opaque loader ids remain install identities, not Minecraft-version labels or lifecycle inputs.

## Maintenance rules

- add new providers by normalizing them into `LoaderBuildMetadata`
- do not add provider-specific booleans like `stable`, `recommended`, `latest`, or `prerelease` to app-facing records
- do not invent generic loader labels like `preview` or `experimental`
- keep explicit upstream loader terms and selection policy as separate concerns
- keep provider heuristics in the provider layer, not the frontend
