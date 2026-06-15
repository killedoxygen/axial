# Guardian Architecture
Guardian is Croopor's horizontal safety intelligence layer. It exists so the launcher can keep working when runtime configuration, downloads, local state, performance state, connectivity, or user overrides are messy, without scattering safety policy across route handlers, Execution primitives, Performance helpers, Healing copy, session heuristics, and frontend code.

Guardian is not only a launch-time checker. It is the safety authority that receives facts from the rest of the backend, diagnoses risk, chooses the allowed safety action, orchestrates lower-level repair systems, records evidence, and emits bounded user-facing outcomes. Self-healing is a Guardian subsystem, not a separate top-level policy authority.

## Goal
For every safety-relevant operation phase, Guardian answers:

`given the request, observed facts, ownership, failure memory, current operation context, and configured safety mode, what safety decision protects the user and keeps the app functional?`

That decision covers:
- whether the operation is allowed
- whether Guardian may warn, intervene, repair, retry, degrade, roll back, suppress, or block
- which lower-level subsystem is allowed to execute the action
- what evidence must be recorded
- what public summary or notice the user should see
- why Guardian did not act when a safety-relevant failure occurred

## Layer Model
Guardian has a foot in every backend system, but it should not absorb every implementation detail.

- Application owns command staging, operation identity, route orchestration, and command result shape.
- Execution owns primitive facts and concrete effects: file, download, JVM, runtime, process, and launch effects.
- Performance owns performance plan resolution, health, composition state, rollback snapshots, and performance operation execution.
- State owns live/durable sessions, operation journals, failure memory, and proof state.
- Observability owns evidence tiers, redaction, local proof records, and any future telemetry-safe export boundary.
- Interface/API owns DTO and view-model boundaries that let the frontend render without reconstructing policy.
- Frontend owns draft UI state, event wiring, and rendering backend-authored states, actions, notices, and progress.
- Guardian owns safety diagnosis, action selection, self-healing orchestration, public safety outcomes, and safety non-intervention reasons.

Guardian coordinates those systems through structured facts and bounded actions. It should not become a giant list of route-local conditionals, and lower layers should not smuggle product safety decisions back into helpers.

## Current Implemented Surface
Guardian currently has working proof across these areas:

- launch/runtime/JVM preflight facts for undefined, null, empty, missing, incompatible, and bad custom Java overrides
- malformed or unsupported JVM argument and preset facts
- launch readiness facts for missing metadata, incomplete install markers, missing jars/libraries/assets, and managed-runtime readiness
- managed-runtime ready-marker repair before session creation when ownership, journal, postcondition, and failure-memory gates allow it
- memory, CPU, concurrent launch, active install/download, low disk, and Custom override warnings
- startup stall, pre-boot exit, crash-after-boot, clean external close, and launcher-stop session outcome separation
- one startup recovery attempt when the target and failure memory allow it
- install/download artifact evidence for checksum, size, missing-artifact, metadata, provider, network, permission, ownership, temp-write, promote, and success facts
- one-shot launcher-managed artifact repair when checksum, size, or missing selected-artifact facts exactly match a private selected descriptor
- install repair outcome journaling and failure-memory suppression
- performance facts for invalid rules, degraded/fallback/invalid health, user-owned conflicts, repeated failures, and rollback availability
- public/exportable redaction for Guardian outcomes, launch notices, session status/events, install status/events, operation status, performance health/status, operation journals, and local proof exports

Not every domain has a specialized automatic repair workflow yet. When Guardian does not have a specific workflow, it still owns the safety interpretation if the issue crosses a safety boundary: it should cushion damage by producing a bounded block, warning, degraded state, retry suppression, or user-facing notice rather than letting raw errors or frontend guesses escape.

## Modes
### Managed
Intent:
- the launcher actively protects the user from technical mistakes and damaged launcher-managed state

Policy:
- Guardian may replace incompatible Java overrides with managed Java
- Guardian may strip fatal raw JVM args
- Guardian may downgrade or disable unsafe GC/preset choices
- Guardian may repair launcher-managed runtime and artifact state when ownership and postcondition gates pass
- Guardian may allow one bounded startup recovery when the startup window fails
- Guardian may degrade or fall back when Performance marks that path safe
- Guardian must journal what it changed and emit bounded public copy

### Custom
Intent:
- the launcher respects explicit technical choices while still enforcing hard safety invariants

Policy:
- Guardian does not silently mutate explicit user intent
- Guardian warns when explicit Java, JVM preset, or raw JVM argument overrides are preserved
- Guardian blocks guaranteed-fatal override combinations before spawn
- Guardian blocks explicit named JVM presets before spawn when the selected runtime is known not to support the emitted flags
- Guardian can cushion unsafe Custom state with guidance, bounded errors, or non-destructive checks
- automatic mutation of user-owned or unknown-owned state is not allowed

### Disabled
Intent:
- the user has explicitly opted out of normal Guardian intervention

Policy:
- Guardian does not perform normal repair or warning interventions
- hard invariants still block unsafe unjournaled mutation, user-owned destructive repair, or unredacted public output
- safety-relevant non-intervention should still be explainable in local evidence when possible

## Decision Loop
Guardian decisions follow the same shape even when a domain has only partial workflow coverage:

1. Fact intake
   Lower systems emit structured facts with source, phase, ownership, target, evidence fields, and sensitivity.

2. Diagnosis
   Guardian maps facts into diagnoses with domain, diagnosis id, confidence, severity, ownership, and public reason templates.

3. Risk and action pressure
   Guardian combines severity, confidence, blast radius, reversibility, ownership, user intent, operation phase, mode, and failure memory into an action decision.

4. Action selection
   Guardian chooses allow, warn, intervene, repair, retry, degrade, rollback, suppress, ask, or block. Unsupported or unsafe actions become bounded warnings/blocks instead of improvised mutation.

5. Plan and ownership gates
   Any mutating plan must have launcher-managed or composition-managed ownership, an operation journal, a rollback/quarantine/postcondition story where applicable, redaction-ready public output, and loop-control checks.

6. Execution by lower layer
   Execution, Performance, runtime, install, or process helpers perform the concrete effect. Guardian authorizes; lower layers execute.

7. Verification and memory
   The postcondition is checked, the operation journal records success/failure/block/suppression, and failure memory prevents repeated destructive or useless loops.

8. Public outcome
   Guardian emits bounded message/details, intervention summaries, or non-intervention reasons. Raw paths, Java paths, JVM args, command lines, provider payloads, account ids, usernames, tokens, server addresses, and token-like strings do not cross public or exportable boundaries.

## Core Data Model
### Inputs
Guardian reasons from explicit facts:
- Guardian mode and operation phase
- command kind and operation id
- ownership class for every target
- explicit Java override presence and origin
- explicit JVM preset and raw JVM args presence and origin
- required Java major/update facts
- effective runtime facts and managed-runtime readiness
- target Minecraft version, loader component, and modded state from installed metadata
- selected memory bounds and host resource observations
- active launch, install/download, and performance operation pressure
- launch readiness diagnostics
- startup observations: boot marker, log evidence, exit code, stall, clean stop, launcher stop
- download/install facts and private selected launcher-managed descriptors
- performance rules, composition, health, rollback, and failure-memory facts
- prior attempts and suppression windows
- redaction readiness for any public/exportable output

### Outputs
Guardian outputs normalized safety results:
- decision: `allowed | warned | intervened | blocked | suppressed | repaired | degraded | rollback`
- diagnosis ids and confidence/severity
- action plan and prerequisite metadata
- ownership and target descriptors
- operation/journal ids when a mutation or repair is involved
- user-facing `message`, ordered `details`, optional guidance, and intervention summary
- evidence for why Guardian acted or did not act

## Self-Healing
Self-healing is the Guardian subsystem that turns an approved action plan into a bounded repair attempt.

It owns:
- repair plan validation
- ownership gates
- journal requirements
- rollback/quarantine/postcondition requirements
- failure-memory loop control
- public repair outcome shape

It orchestrates:
- managed-runtime ready-marker repair
- selected launcher-managed artifact quarantine/redownload/promote
- selected missing launcher-managed artifact download/promote
- performance fallback/degraded decisions and rollback eligibility through the Performance system

It does not own raw provider selection, arbitrary file deletion, user-owned file mutation, or unbounded retry loops.

## Launch Runtime And JVM
Runtime, JVM, and launch preparation code emit facts and provide execution hooks. Guardian decides whether to keep a requested runtime, switch to managed runtime, strip or preserve raw JVM args, disable custom GC, downgrade a preset, warn, repair, or block.

Launch preparation remains backend-owned. The frontend does not decide readiness, compute effective memory policy, classify Java/JVM failures, choose Guardian/Healing precedence, or synthesize crash warnings.

## Session Outcomes
Session code collects observations and preserves bounded history. It distinguishes:
- clean external close after boot
- launcher stop
- startup crash before boot
- startup stall
- crash after boot
- unknown or failed exits

Guardian owns the safety interpretation when those observations affect user-facing outcomes. Closing Minecraft through the game window after startup is a clean `ExternalUserClosed` outcome and does not produce a crash warning unless the backend explicitly authors a notice.

## Install And Download
Install/download systems emit redacted facts and keep private selected descriptors for repair planning. Public install progress/status/events are sanitized at the API boundary.

Guardian can repair only launcher-managed artifacts when the failed fact exactly matches a private selected descriptor and ownership/postcondition gates pass. Metadata, provider, network, permission, ownership, temp-write, promote, and success facts do not trigger automatic artifact repair today; they still produce bounded evidence and public failures rather than raw provider or filesystem output.

## Performance
Performance owns plan resolution, health, composition locks, managed artifact mutation, rollback snapshots, and queued performance operations.

Guardian consumes Performance facts for invalid remote rules, degraded or fallback health, invalid ownership, repeated failure, and rollback availability. Guardian can recommend or record degraded/fallback/rollback-safe states, but concrete composition mutation stays in Performance and must respect composition-managed ownership.

## Observability, Redaction, And Telemetry
Observability is the redaction and evidence boundary.

Evidence tiers are:
- internal local
- user visible
- exportable proof
- optional telemetry export

Current code has local evidence and exportable proof records. The config contains `telemetry_enabled` as a disabled-by-default consent flag only; there is no current upload pipeline or remote diagnostics channel. Any future telemetry must export sanitized evidence and cannot be required for local Guardian behavior.

Public and exportable output must not expose raw:
- tokens
- account identifiers
- usernames unless intentionally bounded
- filesystem paths
- Java paths
- JVM args
- command lines
- provider payloads
- server addresses
- token-like strings

## Frontend Contract
The frontend renders backend-authored safety state.

It may:
- keep draft form state
- submit commands
- subscribe to progress/status streams
- render backend actions, view models, notices, and details
- show optimistic presentation only where the backend contract explicitly allows it

It must not:
- decide launch readiness
- classify exits
- parse raw JVM args for policy
- choose whether Guardian or Healing wins
- infer install/download repair status
- decide performance health or rollback policy
- turn raw diagnostics into user-facing copy

## Invariants
- Guardian is the single safety authority.
- Self-healing is a Guardian subsystem.
- Lower systems emit facts and execute approved effects; they do not own product safety decisions.
- Frontend renders backend-authored policy.
- No automatic mutation happens without known owned state, journaling, redaction, and loop control.
- Unknown ownership is treated as user-owned.
- Public/exportable diagnostics are redacted before crossing the boundary.
- Every Guardian intervention records evidence, diagnosis, confidence, severity, ownership, action, outcome, and user-facing summary.
- Every non-intervention in a safety-relevant failure should be explainable by mode, ownership, unsupported action, low confidence, suppression, or missing safe workflow.

## Known Boundaries
- Session startup/failure inference still depends on log and process observations, but those are facts, not policy.
- Automatic install repair is currently specific to selected launcher-managed checksum/size/missing-artifact cases.
- Performance artifact publisher signature verification is not implemented.
- Optional telemetry upload is not implemented.

## Change Rule
If Guardian behavior, authority boundaries, self-healing, redaction, launch/session safety, install repair, performance safety, or frontend safety rendering changes, update:
- `docs/GUARDIAN-ARCHITECTURE.md`
- `docs/ARCHITECTURE.md`
- any affected subsystem architecture doc or ADR
- any user-facing copy that describes Guardian mode behavior
