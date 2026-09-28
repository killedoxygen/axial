# ADR 0003: One Target For Every Discover Entry Point
Status: Accepted

Supersedes the staging plan in ADR 0002, which sequenced resource packs, shader
packs and modpacks into later phases. They ship together here.

## Context
ADR 0002 gave content an identity and an install pipeline, but assumed one way in:
open Discover, pick a mod, choose an instance from a dropdown. Real use has more
entry points than that.

- Someone browsing Discover with no instance yet, who picks a handful of mods and
  needs somewhere to put them.
- Someone inside a Forge 1.20.1 instance who wants one more mod, and should never
  be shown anything that cannot work there.
- Someone who wants a modpack, which is not content you add to an instance — it
  *is* an instance.
- Someone who wants a resource pack for a vanilla instance, where there is no
  loader at all.

Building a flow per entry point multiplies the surface and guarantees they drift.
The install rules (dependencies, conflicts, compatibility) would end up restated
in each one.

## Decision
Discover is parameterized by a single piece of state, the **target**, and every
entry point does nothing but set it.

- **No target** — browsing. Filters are the user's own.
- **Instance target** — filters are derived and locked to that instance's loader
  and Minecraft version, results are annotated with what it already has, and every
  action goes to it. Carried in the route, so it survives navigation and the
  breadcrumb can say where the content is headed.
- **Draft target** — content staged with nowhere to put it yet. The set itself
  implies the instance: the backend ranks the (loader, Minecraft version) pairs the
  picks support, and the chosen one is created and filled.

`content_plan` therefore takes a target descriptor (`instance` or `draft`) rather
than an instance id, so one resolver serves both. A draft resolves against an empty
manifest; nothing else differs. `content_install` stays instance-only: you cannot
install into something that does not exist.

Supporting decisions:

- **The create API is already addressable by loader and Minecraft version**
  (`selection_id = loader_version|fabric|1.21.6`). Discover constructs one directly
  rather than decoupling the create wizard. The wizard's job is *browsing* versions,
  which is exactly what Discover does not need — the content has already decided.
- **An instance records the loader and Minecraft version it was created with.** The
  installed version entry is a scan artifact: absent before its download starts,
  and missing its loader attachment while one is in flight. Deriving "is this
  modded?" from it alone made a freshly created modded instance claim to be vanilla
  and refuse the very content it was created for. The declaration is immutable and
  fills the gap.
- **Content kinds are not uniformly loader-tagged.** Modrinth tags resource packs
  as `minecraft` and shaders as `iris`/`optifine`. Filtering those by the instance's
  loader matched almost nothing (2 hits instead of 13,499). Loader filtering applies
  to mods and modpacks only, and only mods require a loader to install.
- **A modpack is never added to an instance.** Its action is always "create an
  instance from this pack": resolve the loader and Minecraft version from its
  metadata, create that instance, then import the `.mrpack` into it.
- **Interrupt only for a decision.** A clean plan installs silently and toasts. A
  plan with conflicts raises a dialog, because there the user's answer changes what
  happens.
- **Content detail is a route, not a modal.** It is a destination with a
  description, gallery, versions and a decision — and as a route, leaving it and
  coming back preserves the search behind it.
- **Project titles, not version names.** A hash lookup and a version record both
  name the *version* ("Sodium 0.7.3 for Fabric 1.21.8"). Every path that displays or
  records a resolved item batches a project-title lookup, or the provenance manifest
  fills with strings nobody recognizes.

## Consequences
Positive:
- Entry points are free. Adding one means setting a target, not writing a flow.
- One resolver enforces dependencies, conflicts and compatibility everywhere, so
  the rules cannot drift between "add to my instance" and "build me an instance".
- A targeted Discover cannot offer content that will not work, because the target's
  facts *are* the filters.
- Resource packs and shaders work on vanilla instances, which the mods-only guard
  had made a dead end.

Tradeoffs:
- The tray (staging) is real UI state that has to stay coherent with the route's
  target, including when the target is cleared mid-flow.
- Compatibility ranking is a heuristic over upstream metadata. It scores
  (loader, version) pairs and reports what each drops; it cannot know that two mods
  are semantically incompatible if neither declares it.
- Modpack import stages every selected file and override before promotion. Replaced
  files are backed up until the complete file set and provenance manifest commit;
  setup-owned instances are removed if their queued import fails.
