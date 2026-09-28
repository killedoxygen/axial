# ADR 0002: Content Discovery And Provenance
Status: Accepted

## Context
Axial can install Minecraft versions and loaders, but it has no way to find or
install content (mods, modpacks, resource packs, shader packs). Mods today are
opaque `.jar` files under `<game_dir>/mods`, scanned by filename and toggled with
a `.disabled` suffix. Nothing records what a jar is, where it came from, or which
version it is.

That missing identity is the real blocker. Without provenance we cannot dedupe
results against what is already installed, offer updates, resolve dependencies or
conflicts, or let users cherry-pick content into an existing instance. Any
"Discover" feature that ignores this ends up as a download button bolted onto an
unmanaged mods folder.

We also want one design that covers several content types, integrates with the
existing install queue rather than growing a parallel download system, and keeps
policy (dependency and conflict decisions) on the backend per `CONVENTIONS.md`.

## Decision
Add a content discovery subsystem built on four durable choices.

1. Dedicated `core/content` crate.
The Modrinth-backed service, provider-neutral model, resolution engine, and
install pipeline live in `core/content`, which depends on `core/minecraft` for
verified downloads and integrity. This keeps `core/minecraft` focused on the
game runtime and gives content a clear API boundary.

2. Direct service with a provider-neutral model.
`ContentService` owns one shared HTTP client and directly maps Modrinth search,
detail, version, hash-identity, and metadata endpoints into provider-neutral
domain records. Modrinth is the only supported source: CurseForge requires a
partner key and carries redistribution restrictions. We do not retain a trait,
registry, merge pass, or per-record source list for a provider that does not
exist. Canonical IDs remain explicitly namespaced, and the service rejects every
namespace except `modrinth:` before making a request. A future second source must
justify and introduce the abstraction its real behavior requires.

3. Hash-based provider identification.
An authenticated file's `sha512` identifies its exact bytes. Modrinth's
`version_file/{hash}` endpoint resolves provider-described pack files back to a
project and version before the launcher records ownership. Duplicate destinations
or ambiguous repeated managed hashes fail closed. Project records retain their
Modrinth namespace; no speculative cross-provider project merge is performed.

4. Per-instance provenance manifest.
Each instance has a strict v3 `axial.content.json` manifest, owned by
`core/content`, recording every launcher-managed entry. Schemas v1 and v2 are
unsupported and have no compatibility reader. Every file-owning entry has a
typed managed filename, a canonical lowercase SHA-512 digest, and an exact
positive byte size; Modpack provenance is the sole pathless, no-file exception
and represents that state by omitting the optional filename directly, never by
serializing an empty-string sentinel. Provider metadata enters through checked
constructors, while identified pack files remain typed pending entries until
authenticated positive sizes allow one atomic batch transition. SHA-1 is retained
only as transient upstream download evidence and is never manifest authority.
The filesystem stays the truth for current file presence, while the manifest
retains durable identity and ownership. Reading instance content never rewrites
provenance or adopts files that appeared outside a launcher-owned install
transaction.

Supporting choices:
- Content installs are a new kind on the existing install queue, reusing verified
  transfer, SSE/desktop progress, and the single `activeDownload` representation.
  No second download-state mirror.
- Dependency and conflict resolution produce a backend-authored `ResolutionPlan`
  view-model (to install, deps added, conflicts with suggested resolution,
  removals). The frontend renders and confirms it; it does not author policy.
- Installs stage and verify files before promotion, then update provenance as
  part of the managed install flow.

Scope for the first release:
- Data packs are deferred. Vanilla data packs are per-world, which does not fit
  "install into instance" cleanly; they come in a later phase.
- Discover ships with Modrinth mods, resource packs, shaders, full modpack setup,
  compatible-file cherry-pick, managed provenance, dependency and conflict
  resolution, update detection, and queue-integrated progress. A second provider
  remains deferred until a suitable independent source exists.

## Consequences
Positive:
- One pipeline and one canonical model serve every content type.
- Provenance makes dedupe, updates, conflict resolution, and cherry-pick
  tractable for launcher-managed content.
- Reusing the install queue keeps progress and download state unified.
- Backend-authored resolution keeps policy out of the UI, matching conventions.

Tradeoffs:
- Instances now carry a strict manifest whose entries may drift from the
  filesystem. Read paths report only live managed entries without destroying the
  ownership record; explicit managed operations handle changes.
- Hand-dropped files remain local and unmanaged. Axial does not infer ownership,
  updates, compatibility, or removal authority from their hashes.
- Supporting another source requires a deliberate model and service design once
  its actual identity, authentication, and redistribution constraints are known.
- Provider-unidentified pack files remain unmanaged rather than being assigned
  invented provenance, update, compatibility, or removal authority.
