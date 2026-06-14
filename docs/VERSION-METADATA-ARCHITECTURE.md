# Version Metadata Architecture
This is the current Minecraft-version interpretation model. Keep it accurate. If version naming, lifecycle classification, or ordering changes, update this file in the same change.

## Purpose
The backend owns two separate concerns:

- `minecraft_meta`: what kind of Minecraft version this is structurally
- `lifecycle`: how mature that version is in launcher terms

That split is deliberate. Loader builds use a different metadata contract and are documented separately in `docs/LOADER-ARCHITECTURE.md`.

## Source of truth
The authority lives in:

- `core/minecraft/src/version_meta/mod.rs`
- `core/minecraft/src/version_meta/parse.rs`
- `core/minecraft/src/version_meta/tokenize.rs`

It produces:

- `MinecraftVersionMeta`
- `LifecycleMeta`

Those fields are attached to:

- vanilla catalog entries from `/api/v1/catalog`
- installed/local versions from `/api/v1/versions`
- loader-supported Minecraft versions from `/api/v1/loaders/components/{id}/game-versions`

## Record model
Minecraft-version records expose:

- `subject_kind = minecraft_version` for catalog and loader-supported records
- `subject_kind = installed_version` for installed/local version records
- `raw_kind`: upstream kind string when available
- `minecraft_meta`
- `lifecycle`

`MinecraftVersionMeta` carries:

- `family`
- `base_id`
- `effective_version`
- `variant_of`
- `variant_kind`
- `display_name`
- `display_hint`

`LifecycleMeta` carries:

- `channel`: `stable | preview | experimental | legacy | unknown`
- `labels`
- `default_rank`
- `badge_text`
- `provider_terms`

`minecraft_meta` does not own maturity anymore. `lifecycle` does.

## Pipeline
```mermaid
flowchart TD
    A[Raw version id + raw kind + release time] --> B[tokenize.rs builds typed token stream]
    B --> C[parse.rs strips known variant suffixes]
    C --> D[parse.rs detects structural shape]
    D --> E[mod.rs builds MinecraftVersionMeta]
    D --> F[mod.rs maps shape to LifecycleMeta]
    E --> G[mod.rs resolves effective_version and display strings]
    F --> H[mod.rs assigns default_rank and badge_text]
    G --> I[attach minecraft_meta]
    H --> J[attach lifecycle]
    I --> K[frontend renders backend-authored metadata]
    J --> K
```

## Layer responsibilities
### 1. Tokenizer
`tokenize.rs` converts a raw id into tolerant typed tokens:

- `number`
- `word`
- `separator`

It stays generic and should not own family policy.

### 2. Structural parser
`parse.rs` detects Minecraft version shapes and strips variant suffixes.

Current shapes include:

- release
- pre-release
- release candidate
- release snapshot
- weekly snapshot
- combat test
- experimental snapshot
- deep dark experimental snapshot
- old beta
- old alpha
- unknown

### 3. Semantic interpreter
`mod.rs` is the policy layer.

It decides:

- `minecraft_meta.family`
- `minecraft_meta.effective_version`
- `minecraft_meta.display_name`
- `minecraft_meta.display_hint`
- `lifecycle.channel`
- `lifecycle.labels`
- sort precedence

## Classification rules
Examples:

- `25w46a`
  - `family = weekly_snapshot`
  - `lifecycle.channel = preview`
  - `lifecycle.labels = [snapshot]`
- `1.21.11-pre5`
  - `family = pre_release`
  - `lifecycle.channel = preview`
  - `lifecycle.labels = [pre_release]`
- `1.21.11-rc3`
  - `family = release_candidate`
  - `lifecycle.channel = preview`
  - `lifecycle.labels = [release_candidate]`
- `26.1-snapshot-9`
  - `family = release_snapshot`
  - `lifecycle.channel = preview`
  - `lifecycle.labels = [snapshot]`
- `b1.7.3`
  - `family = old_beta`
  - `lifecycle.channel = legacy`
  - `lifecycle.labels = [old_beta]`
- `a1.2.6`
  - `family = old_alpha`
  - `lifecycle.channel = legacy`
  - `lifecycle.labels = [old_alpha]`

## Effective version
`effective_version` is the practical release target or grouping anchor.

Examples:

- `1.21.11-pre5` -> `1.21.11`
- `1.21.11-rc3` -> `1.21.11`
- snapshot estimates represent the incoming edition: when a snapshot technically anchors to or timeline-matches an existing release, `effective_version` advances to the immediate next known release
- `26.1-snapshot-9` -> immediate next known release after `26.1`; falls back to `26.1` when no later release is known
- `1.16_combat-3` -> `1.16`
- `25w46a` -> nearest release by manifest timestamp

## Ordering rules
Installed version ordering is backend-owned and currently follows:

1. lifecycle priority
2. release time descending when available
3. effective version descending
4. family priority
5. base id descending
6. variant priority
7. raw id descending

Loader-supported Minecraft versions use:

1. Mojang manifest order when known
2. backend fallback version comparison otherwise

The fallback comparator stays deterministic even for unknown shapes.

## Frontend contract
Frontend code should:

- render version labels through `frontend/src/version-display.ts`
- use `minecraftVersionLabel()` for Minecraft-only UI labels
- never render composite loader ids such as `quilt-loader-0.29.2-1.16.5`, `1.19-forge-41.1.0`, or `neoforge-26.1.0.19-beta` as the Minecraft version; backend metadata must provide `inherits_from` or normalized Minecraft metadata
- installed loader versions must carry backend-authored loader metadata from `versions/<id>/.croopor-loader.json`; scanners and routes should not infer loader identity from raw version ids
- use `normalizeVersionDisplay()` / `versionSearchText()` for version picker rows and filtering
- use `lifecycle` for filtering and badges
- avoid re-parsing vanilla-like version ids locally

## Maintenance rules
- add new Mojang naming families in `version_meta/mod.rs`, not in the frontend
- do not overload `family` as a maturity field
- do not reintroduce top-level `type` or `canonical_kind`
- if lifecycle policy changes, update both this doc and `docs/LOADER-ARCHITECTURE.md`
