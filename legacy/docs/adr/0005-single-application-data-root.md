# Single Application Data Root

## Status

Accepted.

## Context

The API, desktop shell, and managed runtime cache previously rediscovered storage
from process environment variables. Missing variables could select a relative
working-directory fallback, while runtime storage could select a different root.
That split durable authority and made development state overlap production state.

## Decision

`apps/api::bootstrap` resolves one absolute root before store construction.
Production appends `dev.mateoltd.axial` to the platform local-data directory.
Development uses `dev.mateoltd.axial.dev`. Before constructing stores, the desktop
selects between those roots from the identifier in its generated Tauri context.
Non-release desktop builds merge the development Tauri configuration at compile
time, so directly launched debug artifacts retain the development identity without
a task-provided environment. An explicit production or development environment
mode must match that native identity or startup fails before path resolution.

The standalone API retains environment selection and defaults to production.
Tests inject a validated absolute root. Portable execution sets the mode to
`portable` and supplies `AXIAL_APP_ROOT`; portable injection is independent of the
desktop identity, while an unpaired root is invalid.

`core/config::AppPaths` keeps the root private and derives immutable static purpose
paths for stores, journals, reports, staging areas, and content roots. Live managed
filesystem owners receive purpose-scoped Directory capabilities minted by
`AppRootSession`; music has no ambient path accessor. Performance rules receive
`performance/`, while `AppRootSession` retains the startup-captured physical
root, process-image proof, and process lease. Terminal reset preflights that proof,
proves any configured existing library physically outside the root from retained
native ancestry identities, drains the sole filesystem authority after application
quiescence, clears anchored root children, and releases the real retained lease
receipt before relaunch. The session returns a bounded error and retains exact drain
or clear failure authority for a same-intent retry; terminal settlement and clear
revalidate the lease, root identity, and process image before mutation. The managed
runtime cache receives only `runtimes/`. There is no generic root getter, relative
fallback, root-discovery canonicalization, legacy-root read, or migration.
Windows accepts disk and UNC roots in normal or extended-length form, while
device and opaque verbatim namespaces are rejected.

## Consequences

All durable stores and managed runtime files share one identity. Development and
production data are isolated. Invalid or unavailable roots fail startup with
path-free errors. Physical root anchoring and the process lease remain a separate
filesystem boundary.
