# Guardian Architecture
Guardian is Axial's horizontal safety intelligence layer. It exists so the launcher can keep working when runtime configuration, downloads, local state, performance state, connectivity, or user overrides are messy, without scattering safety policy across route handlers, Execution primitives, Performance helpers, Healing copy, session heuristics, and frontend code. Guardian is deliberately quiet until needed: it supports the broader launcher and management environment rather than serving as the primary product identity.

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
- Performance owns performance plan resolution, health and composition semantics, mutation logic, rollback snapshots, benchmark/qualification semantics, and performance operation execution.
- State owns live/durable sessions, operation journals, failure memory, proof state, and runtime admission/lifecycle coordination for identity-bound managed composition state.
- Observability owns evidence tiers, redaction, local proof records, and any future telemetry-safe export boundary.
- Interface/API owns DTO and view-model boundaries that let the frontend render without reconstructing policy.
- Frontend owns draft UI state, event wiring, and rendering backend-authored states, actions, notices, and progress.
- Guardian owns safety diagnosis, action selection, self-healing orchestration, public safety outcomes, and safety non-intervention reasons.

Guardian coordinates those systems through structured facts and bounded actions. It should not become a giant list of route-local conditionals, and lower layers should not smuggle product safety decisions back into helpers.

## Current Implemented Surface
Guardian currently has working proof across these areas:

- launch/runtime/JVM preflight facts for undefined, null, empty, missing, probe-failing, probe-timeout, wrong-major, outdated, incompatible, and bad custom Java overrides
- malformed or unsupported JVM argument and preset facts
- create/update JVM preset catalog and normalization for undefined, blank, automatic, supported, unknown, and tampered preset values, with bounded create notices and no raw preset echo
- launch readiness facts for missing metadata, incomplete install markers, missing jars/libraries/assets, and managed-runtime readiness
- managed-runtime ready-marker repair before session creation when ownership, journal, persisted runtime proof/checksum postcondition, and failure-memory gates allow it
- memory, CPU, concurrent launch, active install/download, low disk, and Custom override warnings
- startup stall, pre-boot exit, crash-after-boot, clean external close, and launcher-stop session outcome separation
- bounded terminal fusion for stdout classes, exit status, and typed crash evidence, including OOM, graphics-driver, missing-dependency, mod-transformation, and mod-attributed failures
- one startup recovery attempt when the target and failure memory allow it
- State-backed failure-memory persistence for Guardian suppression windows and occurrence counts across API/desktop restarts
- bounded next-launch advisories for recent accepted crashes, failed repairs, and active repair suppression, with exact instance, mode, recency, and current-intent filtering
- install/download artifact evidence for checksum, size, missing-artifact, metadata, provider, network, permission, ownership, temp-write, promote, and success facts
- Guardian-authored install safety outcomes for provider, network, interrupted, invalid-metadata, permission-denied, temp-write, promotion-failed, and ownership-refused failures that are not artifact-repair candidates
- one-shot launcher-managed artifact repair when checksum, size, or missing selected-artifact facts exactly match a private selected descriptor
- install repair outcome journaling and failure-memory suppression
- State-backed operation-journal persistence for bounded current operation records, including install status/event restart replay of terminal progress and Guardian repair summaries when the transient install session snapshot is gone
- Observability-bounded operation proofs derived from terminal journals, so failed install status and terminal queued Performance operation status can connect operation facts, Guardian diagnosis/action/outcome evidence, and latest verification facts without leaking raw provider, filesystem, or JVM material
- durable Performance operation ownership for queued and synchronous callers: both persist a matching status/journal identity before effects and continue reconciliation independently of request lifetime, while synchronous callers still receive a bounded final payload when persistence remains healthy
- one identity-bound managed-composition runtime authority: registered instance ids resolve to registry-owned storage, per-instance inspection, mutation, and exact recovery are serialized, accepted work survives caller cancellation, active sessions/launch/deletion exclude mutation and recovery through the shared instance lifecycle, indeterminate effects latch fail-closed, and shutdown retries proven recovery after session settlement before closing the instance registry
- performance facts for invalid rules, degraded/fallback/invalid health, user-owned conflicts, repeated failures, and rollback availability
- persisted operation-state load diagnostics for strict-schema performance operation, benchmark suite manifest, and benchmark suite driver records; Guardian maps aggregate load issues to a bounded startup warning instead of letting restart-resume corruption disappear silently
- public/exportable redaction for Guardian outcomes, launch notices, session status/events, install status/events, operation status, performance health/status, operation journals, and local proof exports

`apps/api/src/application/authority.rs` is the local proof gate for this model. Its tests enforce route adapter boundaries, frontend non-policy rendering, Execution non-policy, required source/control-plane reproducibility, and a quality-gate failure-scenario matrix that points every required Guardian failure scenario at local behavior tests.

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
   Guardian evaluates an ordered declarative rule table once per rule. Each matching rule emits at most one diagnosis with domain, diagnosis id, confidence, severity, ownership, and a public reason template. Facts in the same rule family fuse into distinct source fact ids and deterministically deduplicated real targets; ownership resolves conservatively to the least-trusted supporting owner. Diagnosis order follows the earliest matching input fact, with rule order as the tie-breaker.

3. Rule priority
   Each resolved rule carries a closed typed priority band. Priority bands preserve the declared cross-family ordering for records, degraded state, repairable corruption, launch blockers, ownership boundaries, and critical failures; stable rule order breaks equal-band ties. Phase-matched condition facts may select a rule clause, but they do not become diagnosis evidence, targets, or ownership inputs.

4. Action selection
   Guardian evaluates the strongest diagnosis's declared candidate actions through a closed per-mode action table. Hard redaction, journal, and protected-ownership invariants run first; typed mode, explicit-intent, and unknown-ownership rejections then preserve safe questions, warnings, records, or blocks instead of improvised mutation. Disabled mode uses a diagnosis-level hard-invariant disposition, and active retry-loop suppression uses a diagnosis-level Block disposition. Launch preflight adds one typed decision scope: ordered kernel rules resolve readiness admission, managed Java fallback, managed JVM-argument stripping, warnings, and confirmation from the assembled facts. Until the deferred confirmation surface exists, one boundary adapter maps `AskUser` to `Block` for admission while preserving the kernel-authored confirmation copy.

5. Plan and ownership gates
   Any mutating plan must have launcher-managed or composition-managed ownership, an operation journal, a rollback/quarantine/postcondition story where applicable, redaction-ready public output, and loop-control checks.

6. Execution by lower layer
   Execution, Performance, runtime, install, or process helpers perform the concrete effect. Guardian authorizes; lower layers execute.

7. Verification and memory
   The postcondition is checked, the operation journal records success/failure/block/suppression, and failure memory prevents repeated destructive or useless loops.

8. Public outcome
   Guardian emits bounded message/details, intervention summaries, or non-intervention reasons. Raw paths, Java paths, JVM args, command lines, provider payloads, account ids, usernames, tokens, server addresses, and token-like strings do not cross public or exportable boundaries.

Runtime repair, install artifact repair, non-repairable install failure, Performance supervision rejection, and persisted-state load outcomes use one iterable typed copy table. Its crate-private request constructors provide closed family shapes, derive repair decisions from terminal status where applicable, and preserve the caller's Performance phase; the copy authority returns no outcome for unsupported coordinates. Runtime install details extract the first matching structured evidence and first matching field once inside this authority, sanitize only those dynamic values for the user-visible audience, and enforce byte and collection bounds with stable deduplication. Launch preflight, launch failure, and launch recovery copy remain in their existing Guardian authors until those families are migrated.

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
- managed-runtime ready-marker repair after persisted manifest/checksum proof verification
- selected launcher-managed artifact quarantine/redownload/promote
- selected missing launcher-managed artifact download/promote
- performance fallback/degraded decisions and rollback eligibility through the Performance system

It does not own raw provider selection, arbitrary file deletion, user-owned file mutation, or unbounded retry loops.

## Launch Runtime And JVM
Runtime, JVM, and launch preparation code emit facts and provide execution hooks. Guardian decides whether to keep a requested runtime, switch to managed runtime, strip or preserve raw JVM args, disable custom GC, downgrade a preset, warn, repair, or block.

Explicit Java override probes are bounded. A Java executable that fails, hangs, emits unparseable version output, reports the wrong major version, or reports an unsafe Java 8 update is converted into a redacted runtime fact for Guardian instead of blocking preflight indefinitely or leaking the raw path. Successful direct-launch preflight owns an opaque executable receipt that preparation revalidates before reuse; standalone preflight and managed-Java intervention discard it, and executable drift forces a fresh probe. State briefly coalesces only neutral failures under a bounded executable-snapshot and requirement key, never healthy verdicts, receipts, target facts, or compatibility failures.

Managed runtime absence and managed runtime corruption are separate safety shapes. Expected absence in Managed mode remains recoverable so Execution can ensure/download the runtime. A present launcher-owned managed runtime with a missing ready marker, corrupt ready marker, missing persisted component-manifest proof, checksum drift, missing Java executable, or non-executable Java executable is treated as corrupt state: Guardian plans the bounded repair, records journal and failure-memory evidence, verifies the full content postcondition, and blocks before session creation when repair cannot make the runtime ready. Execution refuses to recreate a ready marker when the runtime contents cannot be verified from persisted proof.

Guardian preflight outcomes include typed execution directives for the current safe launch-attempt interventions, such as using managed Java for the attempt or stripping explicit JVM args for the attempt. Guardian materializes those directives directly from its effective kernel action; Application launch code executes them without inferring the intervention from diagnosis ids, raw Java paths, raw JVM args, or frontend state.

For next-launch advisories, Application reads one bounded failure-memory snapshot from State after capturing the current launch identity, intent, and resource budget. Guardian accepts only valid entries for the exact canonical instance and Guardian mode within the recent UTC window; repair and suppression entries must also match a private SHA-256 fingerprint binding the recovery kind, canonical target version, and the exact relevant requested Java, ordered JVM arguments, or preset value. Invalid or oversized intent cannot authorize or suppress recovery. Guardian emits at most three public historical facts: `recent_startup_failure`, `recent_repair_failed`, and `repair_suppressed_until`. These facts can add warning-only details and guidance to the existing preflight response, but never block, create directives, or change session admission. OOM history receives a concrete memory increase only when the current host budget, active launch allocations, and reserved headroom prove it safe; otherwise Guardian says that larger headroom could not be verified. Active suppression copy reports the bounded expiry time in UTC. Failed preset recovery produces generic review guidance because next-launch preflight does not hold authoritative evidence of the prior effective preset.

Instance create/update also uses Guardian-owned JVM preset normalization. The create-view DTO exposes the supported preset catalog. Undefined, blank, or `auto` preset input stores Automatic; supported ids store that explicit preset; unknown or tampered create input resets to Automatic and returns a bounded `guardian_notice`. Instance update applies the same preset normalization before persistence. Raw Java path and raw JVM argument overrides may be stored for backend launch preparation, but update responses redact those strings and launch preflight turns them into Guardian facts before any user-facing outcome.

Launch preparation and launch-session execution are backend-owned under the Application launch modules. API launch routes adapt HTTP/SSE transport and call those Application entrypoints; they do not own Guardian preflight, runtime repair, startup recovery, or session outcome policy. The frontend does not decide readiness, compute effective memory policy, classify Java/JVM failures, choose Guardian/Healing precedence, or synthesize crash warnings.

## Session Outcomes
Session code collects observations and preserves bounded history. It distinguishes:
- clean external close after boot
- launcher stop
- startup crash before boot
- startup stall
- crash after boot
- unknown or failed exits

Guardian owns the safety interpretation when those observations affect user-facing outcomes. Closing Minecraft through the game window after startup is a clean `ExternalUserClosed` outcome and does not produce a crash warning unless the backend explicitly authors a notice. Startup-failure log signals are timestamped session facts; only signals still correlated with the terminal exit window can drive a failure outcome, while stale signals remain historical evidence. Loader bootstrap markers such as ModLauncher/FML output require failure context before they become a loader-bootstrap failure; ordinary loader progress logs remain startup activity only. The process supervisor keeps startup monitoring open until a real boot marker is observed; ordinary stdout/stderr activity is preserved as evidence but does not prove the game opened successfully. If the boot marker never arrives and the watchdog fires, State records a stalled startup outcome with redacted Execution process evidence, and Guardian authors the public recovery or block outcome.

For a natural failed exit, Execution may collect one correlated crash report or JVM fatal-error artifact from the actual launched game directory before State publishes the terminal record. User stop, watchdog, launch-failure termination, process replacement, shutdown, and successful natural exit never collect. Core Launcher, not State or Guardian, owns source-specific parsing and constructs `CrashEvidence` from closed phases plus field-specific bounded values. A Minecraft report cannot author native-frame evidence, an `hs_err` file cannot author mod attribution, and every deserialized public field is revalidated. Raw reports never enter session state, proof storage, Guardian facts, telemetry, or frontend payloads; only the typed evidence is retained in the terminal session and strict launch proof. Collection absence, saturation, timeout, replacement, read failure, and parse failure are normal no-evidence outcomes.

At exit, State passes the fresh unique stdout classes, exact exit code, and optional typed evidence to one Core Launcher fusion call. Only exit code zero is clean; other exits rank the closed class vocabulary independently of log order. Lifecycle-authored `startup_stalled` remains outside ordinary fusion. Exact OOM evidence outranks artifact hints, graphics-driver classification requires an `hs_err` native frame in a closed driver-module table, missing-dependency and mod-transformation classes require closed loader/throwable markers, and mod attribution requires a complete Minecraft report with parser-authored suspected mods. Generic native crashes, render prose, class-not-found errors, mod inventories, and stack-package guesses do not become these richer classes.

OOM and the four richer crash classes are UserOwned observations with no automatic recovery directive. Before boot, Guardian blocks with class-specific bounded copy and the session/proof records `StartupFailed`; typed mod attribution may name only the first validated suspected mod. After a real boot marker, the Application-owned terminal observer requires the accepted class plus `CrashedAfterBoot`, authors warned copy, updates the existing proof, records exactly one instance observation, and releases the transferred session hold. Other terminal classes use the ordinary proof path. State still owns process facts and terminal outcome classification, not Guardian policy.

Java signature failures from launcher-managed jars are classified separately from generic startup crashes. Known `SecurityException` signer mismatch and invalid signature digest shapes produce the `launcher_managed_artifact_signature` failure class, a launcher-managed artifact signature corruption fact, bounded repair guidance, and no raw JVM log output.

## Install And Download
Install/download systems emit redacted facts and keep private selected descriptors for repair planning. `apps/api/src/application/install.rs` owns install operation identity, live install queue command/status view models, duplicate suppression, retry queue placement, pending queue removal, worker coordination, journal recording, progress redaction, loader install coordination, Guardian artifact repair invocation, repair outcome shaping, install status responses, and backend-authored failed-install view models for public failure copy, retry availability, and repair action state. `apps/api/src/state/installs.rs` stores live install sessions and the in-memory queue snapshot; the queue is not the durable proof surface. `apps/api/src/application/install/stream.rs` owns install and loader progress stream preparation, history replay, terminal handling, closed-channel cleanup, missing-session copy, and progress event serialization. Install and loader routes parse requests, call Application entrypoints, and serialize backend-authored responses; they do not own repair policy, retry safety, operation journal semantics, queue copy, or stream policy.

Guardian can repair only launcher-managed artifacts when the failed checksum, size, or selected-missing-artifact fact exactly matches a private selected descriptor and ownership/postcondition gates pass. Core Minecraft emits selected facts/descriptors with Guardian-safe semantic target ids and requires valid SHA-1 metadata for selected launcher-managed artifacts; missing or invalid checksum metadata fails closed as metadata evidence instead of being treated as verified. Application adapts those facts to Guardian evidence, and Guardian selects the missing-artifact repair plan that verifies the destination is still absent before downloading/promoting without quarantine. Provider, network, and interrupted failures produce retry-oriented Guardian outcomes. Invalid provider metadata, filesystem permission denial, temp-write failure, atomic promotion failure, and ownership refusal produce blocking Guardian outcomes and never mutate automatically. Temp-discard and success facts do not trigger automatic artifact repair or terminal Guardian outcomes today; they remain bounded evidence/status inputs and must not expose raw provider or filesystem output.

After Guardian repairs a launcher-managed install artifact, Application may resume the same install once. Guardian repair status alone is not install success: the resumed worker must complete the full install before success progress is emitted. If the resumed install fails, the final backend failure remains terminal and journaled. Loader library downloads emit the same selected library evidence as vanilla library downloads, so loader library artifact failures can feed this same repair path.

Loader provider fetches follow the same authority split. Core classifies provider network failures, timeouts, HTTP status families, oversized bodies, schema drift, and missing artifacts as structured loader failure kinds without embedding raw URLs, response bodies, or provider errors in public status. Stale loader cache fallback remains observable through bounded availability fields instead of hiding the provider failure. Application records loader provider failures into the install operation journal, and Guardian authors retry or repeated-failure block outcomes. State failure memory suppresses repeated loader-provider retries within the cooldown so connectivity failures cannot spin indefinitely.

Loader base-version dependencies also use the install integrity model. Loader install only treats a base Minecraft version as already installed when its version metadata, client jar, incomplete marker state, and selected base libraries are ready; otherwise it reruns the vanilla install path so inherited loader libraries cannot stay missing behind an existing base JSON/JAR pair. When a loader install must install or repair a base Minecraft version internally, core returns the same redacted vanilla install facts and selected descriptors to Application through a bounded base-install failure instead of hiding the lower-level install failure. Application records those facts/outcomes and can invoke Guardian artifact repair using the descriptors. If no lower-level safety facts are available, the dependent loader operation records `install_dependency_failed` and blocks with Guardian-authored public copy.

## Performance
Performance owns plan resolution, health and composition semantics, composition locks, managed artifact mutation logic, rollback snapshots, benchmark/qualification semantics, and durable performance operations. State owns the runtime authority that decides which registered instance identity that logic may inspect or mutate and when access is admitted.

`apps/api/src/application/performance.rs` and `apps/api/src/application/performance/workflow.rs` own performance route orchestration: status and refresh response shaping, plan and health result carriers, Guardian performance fact adaptation, proof/view-model construction, rollback response semantics, durable operation creation/resume/status, terminal operation-proof composition, and identity-only command staging. `apps/api/src/routes/performance.rs` only parses HTTP requests, calls those Application entrypoints, and serializes backend-authored responses. Production callers do not pass raw instance or `mods/` paths into core state, health, rollback, or mutation helpers.

For instance-scoped work, State requires a canonical registered instance id and derives its exact storage from the instance registry. It serializes inspection, health, evidence, rollback list/preflight, install, remove, rollback, and recovery per instance while permitting unrelated instances to progress. Reads use only this managed per-instance boundary and remain available while a game session runs when the instance is clean; a latched read cannot recover until the active session ends. Every admission takes the shared instance lifecycle boundary, resolves the registry entry again, and rechecks session state before any mutation or recovery. Launch and instance deletion use that same lifecycle boundary, so they cannot overlap those effects, and queued or resumed work cannot recreate a deleted instance.

An accepted mutation moves to an App-owned task before its caller waits, so caller cancellation cannot abandon or overlap the effect. If the effect outcome is indeterminate, State latches that instance. A later admission or shutdown retry moves recovery to an App-owned task under the same instance lifecycle and managed gates. Performance reconciles only current strict protocols and clears the latch only after proving state publication/deletion, removal and replacement backups, managed download temps, rollback publication/restore/retention, strict metadata, and every tracked artifact digest. First-time install and rollback publication retains an exact digest- and identity-bound hardlink obligation until strict state commits or compensation completes, so recovery can distinguish launcher-owned uncommitted bytes from matching user replacements. Unknown, corrupt, symlinked, nonregular, oversized, conflicting, or user-replaced material is preserved and remains latched; there is no manual clear or compatibility path. Shutdown never runs this recovery until session settlement has succeeded.

Guardian consumes Performance facts for invalid remote rules, degraded or fallback health, invalid ownership, repeated failure, and rollback availability. Guardian can recommend or record degraded/fallback/rollback-safe states, but concrete composition mutation stays in Performance behind State admission and must respect composition-managed ownership. Guardian does not bypass identity, lifecycle, or indeterminate-state gates.

Benchmark qualification status labels/tones, target labels, suite/schema labels, missing-evidence summaries, suite/evidence summaries, and per-target required/suite/proof/missing copy are Performance-authored view-model fields. Performance rule-status Settings copy/tone and Performance health summaries are also backend-authored: source/channel labels, validation tone, warning display copy, health copy, bounded composition descriptor, apply/repair action availability, and rollback action availability come from Application/Performance shaping, not frontend token interpretation. Queued Performance operation status responses are also Performance Application-authored view-model boundaries: terminality, completion, progress phase/count, title/detail, and tone come from backend status shaping, not frontend token interpretation. Benchmark suite-driver stop/resume/qualification-check availability is Launch Application workflow state because that boundary owns suite driver lifecycle, active-session checks, and explicit resume/stop behavior. Launch proof outcome, comparison, Guardian/Healing evidence precedence, and resource-budget pressure/details are also Launch Application-authored export view-model fields built from sanitized local proof records. The frontend renders those fields and does not reconstruct Guardian or Performance severity from raw status tokens.

Strict current-schema restart-resume records for performance operations and benchmark suite drivers, plus strict current-schema benchmark suite manifests, are State-owned launcher data. When those records cannot be trusted at startup because the directory cannot be read, an entry cannot be read, a status file is invalid, an id or filename is unsafe/noncanonical, timestamps are invalid, or records conflict, State records only bounded load-issue kinds/counts. Guardian converts the aggregate into the `persisted_state_schema_invalid` diagnosis and a redacted startup warning on `/api/v1/status`. Invalid nonterminal Performance status is retained only for fail-safe terminal reconciliation and is never replayed; journal-only Performance operations are likewise terminalized without replay. Rejected suite manifests cannot become runtime mutation inputs, and runtime suite readers use only committed admitted memory. This path never rewrites or deletes user-owned or unknown-owned files.

Guardian failure memory itself is State-owned launcher data. State loads and saves the strict snapshot under `<config_dir>/guardian/failure-memory.json`, validates redacted descriptors, bounds retention, and writes through the managed atomic file capability. State stores and returns the bounded snapshot; it does not select advisory entries, decide safety policy, or author public copy. Guardian remains the policy owner: it records observations, filters the Application-supplied snapshot, chooses suppression/repair/retry/block/degrade behavior, and lets expired launch-recovery suppression windows allow a new bounded attempt instead of permanently blocking recovery. Launch-recovery outcomes use the canonical instance identity as their durable target, while session ids remain transient tracing and operation-lifecycle context, so a suppression window can govern a later session for the same instance without affecting another instance. That instance target remains `LauncherManaged` because the authorized effect is a launcher-owned, one-attempt process override; it does not claim ownership of or mutate the instance's user-owned files. Each accepted terminal launch-crash class records one instance-targeted observation keyed by its stable class name and increments only its occurrence count; it does not claim an action, repair attempt, suppression window, fallback, user decision, or content/intent hash.

Operation journals are also State-owned launcher data. State loads and saves a bounded strict snapshot under `<config_dir>/state/operation-journals.json`, validates structured ids and redacted public facts, prunes current-state retention, and writes through the shared persistence coordinator. Immediate transitions stay outside the committed public view until physical commit; accepted work and exact retry reconciliation remain store-owned if a caller is cancelled. Guardian and Application author facts, diagnosis ids, repair outcomes, and workflow progress; State stores them without deciding safety policy, privacy policy, retry eligibility, repair authorization, proof export, or telemetry. Install status and event streams can use a restart-loaded journal to replay bounded terminal progress and Guardian repair summaries when the transient install session snapshot no longer exists. Queued and synchronous Performance execution persist a durable operation status id plus matching journal identity, so restart can validate effect-started, terminal-intent, terminal, and proof records before publishing or resuming anything. Observability can compact a matching terminal journal into a redacted operation proof for install and Performance status surfaces, preserving Guardian-authored diagnosis/outcome evidence and latest generated facts while dropping or redacting sensitive free text.

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
- decide auth/account/skin readiness or action availability
- turn raw diagnostics into user-facing copy

## Invariants
- Guardian is the single safety authority.
- Self-healing is a Guardian subsystem.
- Lower systems emit facts and execute approved effects; they do not own product safety decisions.
- Frontend renders backend-authored policy.
- No automatic mutation happens without known owned state, journaling, redaction, and loop control.
- Production managed-composition access starts from a canonical registered instance id and crosses the State-owned runtime authority; raw paths and direct core helpers are not production capabilities.
- Managed composition mutation cannot overlap an active session, launch, or instance deletion, and an accepted mutation is not owned by the request lifetime.
- Indeterminate managed composition effects remain latched until an owned retry proves every current obligation and tracked byte; ambiguous state blocks access and clean shutdown instead of being reported as success.
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
