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
The [generated Guardian invariant coverage](GUARDIAN-INVARIANT-COVERAGE.md) is the
drift-tested inventory of current kernel cells, rules, facts, preflight senses, adapters,
launch failure mappings, and repair hands. Its complete machine-readable matrix and the Markdown
projection are regenerated from the same typed coverage value.

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
   Guardian evaluates the strongest diagnosis's declared candidate actions through a closed per-mode action table. The policy module owns the private, serialize-only decision type and exposes immutable accessors; production callers cannot deserialize, construct, or rewrite a decision. Hard redaction, journal, and protected-ownership invariants run first; typed mode, explicit-intent, and unknown-ownership rejections then preserve safe questions, warnings, records, or blocks instead of improvised mutation. Disabled mode uses a diagnosis-level hard-invariant disposition, and active retry-loop suppression uses a diagnosis-level Block disposition. Each admitted invocation of the synchronous launch-preflight, prepare-failure, preset-adjustment, startup-failure, install-assessment, Performance-supervision, and persisted-state-load boundaries evaluates policy once; validation and empty-input rejections that occur before those boundaries evaluate it zero times. This is a per-boundary invariant, not a global limit per operation. Launch preflight adds one typed decision scope: ordered kernel rules resolve readiness admission, managed Java fallback, managed JVM-argument stripping, warnings, and confirmation from the assembled facts. Until the deferred confirmation surface exists, one boundary adapter maps `AskUser` to `Block` for admission while preserving the kernel-authored confirmation copy.

5. Plan and ownership gates
   Any mutating plan must have launcher-managed or composition-managed ownership, an operation journal, a rollback/quarantine/postcondition story where applicable, redaction-ready public output, and loop-control checks.

6. Execution by lower layer
   Execution, Performance, runtime, install, or process helpers perform the concrete effect. Guardian authorizes; lower layers execute.

7. Verification and memory
   The postcondition is checked, the operation journal records success/failure/block/suppression, and failure memory prevents repeated destructive or useless loops.

8. Public outcome
   Guardian emits bounded message/details, intervention summaries, or non-intervention reasons. Raw paths, Java paths, JVM args, command lines, provider payloads, account ids, usernames, tokens, server addresses, and token-like strings do not cross public or exportable boundaries.

Runtime repair, install artifact repair, non-repairable install failure, Performance supervision rejection, persisted-state load, launch preflight, launch failure, and launch recovery outcomes use the central crate-private copy authority. Its request constructors provide closed family shapes, derive repair decisions from terminal status where applicable, preserve the caller's Performance phase, and immediately reduce raw launch evidence to bounded user-visible values; `author_guardian_copy` returns no outcome for unsupported coordinates. `GuardianUserOutcome` is immutable outside that authority: it exposes read-only accessors, has no deserialize path, and cannot be constructed or rewritten by Application. Runtime install details extract the first matching structured evidence and first matching field once inside this authority, sanitize only those dynamic values for the user-visible audience, and enforce byte and collection bounds with stable deduplication. Launch preflight summary, detail, guidance, and typed historical copy also come from this authority: the authored kernel decision selects the copy, the sole temporary `AskUser`-to-`Block` boundary adapter selects the public verdict, and `SafetyOutcome` derives from the authored user outcome. The same module owns the API-local, serialize-only `GuardianSummary`/decision/intervention transport, projects preflight admission and typed recovery directives into it, preserves bounded prior guidance, raw typed intervention evidence, stable public-detail order, deduplication, and silent-intervention behavior, and authors bounded redacted launch notices plus the Watchdog Guardian outcome sentence. Persisted JSON crosses private typed DTOs and is re-projected with the destination audience's redaction and collection bounds; it cannot recreate or mutate the transport directly. Core Launcher retains only factual Guardian mode/context/constants and factual status/outcome transport; it does not deserialize Guardian summaries, choose notice precedence, supply Guardian fallback copy, or brand watchdog outcomes. A failed managed-runtime repair produces a separate immutable runtime-repair admission block while preserving the original kernel/preflight result for proof. Directive descriptions, recovery suppression, and failed-recovery logs are authored from the same closed directive value instead of caller-supplied prose.

## Core Data Model
Launch proof evidence uses a sealed projection from the central copy module for its tone, centrally authored decision label, and first bounded safe detail. The report adapter serializes that projection without reproducing Guardian label selection or detail-selection policy.

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
- performance rules, composition, health, and rollback facts
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
- private, kind-typed repair authorization
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

Core Minecraft derives a bounded deterministic known-good artifact inventory as factual metadata, not as a Guardian decision or repair capability. Sealed producer inputs bind authenticated vanilla metadata, exact loader source proofs, a checksum-validated asset index, and a checksum-validated managed-runtime component manifest before derivation emits closed safe-relative launcher-owned entries. Every regular file has an exact SHA-1 and mandatory numeric size sourced only from authenticated retained bytes, authenticated declarations, or the same full-hash observation that established the digest. Invalid paths, missing or invalid checksums or sizes, identities, conflicts, impossible runtime ancestor trees, and bounds fail closed; public errors carry no provider paths. Runtime manifest proof bytes are canonical, and runtime executable and ready-marker semantics are explicit. Checksumless profile sources must cross a fresh bounded stream and have their observed hash and size authored into the exact materialized version JSON before the receipt exists; neither a filesystem walk, State inventory, persisted evidence, nor current disk bytes can bless content. Forge/NeoForge selected-plan declarations are separate from retained embedded or processor-output sources and from the post-publication identities minted at their actual managed destinations. Receipt derivation owns all three stages as one consuming obligation: it can produce a pending receipt before effects, but no final receipt exists until retained publication identities complete that obligation exactly once. A final receipt has one public consuming transition into an unforgeable move-only activation source; it has no raw inventory conversion, clone, or serde form, and cannot be converted into reconstruction authority. Canonical loader identity is encoded only in the strict `loader-v2` id and exact materialized profile contract; no duplicate sidecar or known-good entry exists. User-owned and unknown-owned roots such as mods, config, saves, and resource packs are absent by construction. State startup requires the exclusive known-good persistence owner and has no volatile fallback. It consumes the activation source only behind its private boundary before install success, freezes at most the registry's 1024 exact matches, and independently revalidates every instance identity and normalized root under its lifecycle gate. Persistence and any required exact snapshot revision are admitted before live activation; one candidate failure neither prevents a later valid activation nor revokes an earlier one. State persists a strict rebuildable v4 per-instance snapshot, rejects v3 and incomplete-integrity bytes without a compatibility reader, and silently replaces missing, malformed, or stale bytes from an accepted fresh inventory. Instance deletion reserves retirement without changing live or persisted authority and commits only after registry absence; a present-instance failure compensates the reservation unchanged. After absence, writer or exact snapshot deletion failure retains a bounded cleanup obligation for close, and restart rediscovers the remaining canonical snapshot for retry without loading it into runtime authority. Persisted snapshots are never loaded into runtime authority. There is still no launch-preflight verification tier or new Guardian repair authority; the inventory remains distinct from the private selected descriptors that authorize today's bounded artifact repair.

Vanilla reconstruction is a separate Core producer path, not a conversion from an install receipt. It starts from the fresh fixed Mojang manifest and consumes authenticated unmaterialized version and asset-index sources, a source-only managed-runtime component manifest, and one-to-one incomplete-library observations. It performs no final launcher writes and yields a distinct move-only reconstruction receipt whose only outward transition is the same activation source. The private inventory derivation is shared with vanilla installation, making identical authenticated inputs inventory-identical without allowing reconstruction to publish install success.

Loader reconstruction extends that same effect-free Core primitive to Fabric and Quilt profiles and earliest pre-installer Forge. It emits factual reconstruction authority only: no repair descriptor, Guardian authorization, or mutation capability is created, and source acquisition or proof failure fails closed. Installed files, catalog caches, persisted known-good snapshots, and other local evidence cannot substitute for a missing or invalid fresh source. Application orchestration and State reconciliation/activation wiring remain pending; until those boundaries consume the move-only reconstruction result, Core reconstruction alone changes no live or durable authority.

## Launch Runtime And JVM
Runtime, JVM, and launch preparation code emit facts and provide execution hooks. Guardian decides whether to keep a requested runtime, switch to managed runtime, strip or preserve raw JVM args, disable custom GC, downgrade a preset, warn, repair, or block.

Explicit Java override probes are bounded. A Java executable that fails, hangs, emits unparseable version output, reports the wrong major version, or reports an unsafe Java 8 update is converted into a redacted runtime fact for Guardian instead of blocking preflight indefinitely or leaking the raw path. Successful direct-launch preflight owns an opaque executable receipt that preparation revalidates before reuse; standalone preflight and managed-Java intervention discard it, and executable drift forces a fresh probe. State briefly coalesces only neutral failures under a bounded executable-snapshot and requirement key, never healthy verdicts, receipts, target facts, or compatibility failures.

Managed runtime absence and managed runtime corruption are separate safety shapes. Expected absence in Managed mode remains recoverable so Execution can ensure/download the runtime. A present launcher-owned managed runtime with a missing ready marker, corrupt ready marker, missing persisted component-manifest proof, checksum drift, missing Java executable, or non-executable Java executable is treated as corrupt state: Guardian plans the bounded repair, records journal and failure-memory evidence, verifies the full content postcondition, and blocks before session creation when repair cannot make the runtime ready. Execution refuses to recreate a ready marker when the runtime contents cannot be verified from persisted proof.

Guardian launch outcomes use one closed typed directive vocabulary for preflight intervention, prepare recovery, startup recovery, preset compatibility, journaling, failure-memory fingerprints, and public descriptions. Typed reasons distinguish the same action at different execution stages, so Application executes only the exact stage-specific directive and rejects cross-stage values. Each recovery-capable variant owns its action, diagnosis compatibility, intent-fingerprint axis and tag, operation step id, journal fact id, and execution meaning; there is no separate kind/effect pair or free-form description to reconcile. Preflight materializes directives directly from its effective kernel action, and Application executes them without inferring the intervention from diagnosis ids, raw Java paths, raw JVM args, or frontend state.

For next-launch advisories, Application reads one bounded failure-memory snapshot from State after capturing the current launch identity, intent, and resource budget. Guardian accepts only valid entries for the exact canonical instance and Guardian mode within the recent UTC window; repair and suppression entries must also match a private SHA-256 fingerprint binding the recovery kind, canonical target version, and the exact relevant requested Java, ordered JVM arguments, or preset value. Invalid or oversized intent cannot authorize or suppress recovery. Guardian emits at most three public historical facts: `recent_startup_failure`, `recent_repair_failed`, and `repair_suppressed_until`. These facts can add warning-only details and guidance to the existing preflight response, but never block, create directives, or change session admission. OOM history receives a concrete memory increase only when the current host budget, active launch allocations, and reserved headroom prove it safe; otherwise Guardian says that larger headroom could not be verified. Active suppression copy reports the bounded expiry time in UTC. Failed preset recovery produces generic review guidance because next-launch preflight does not hold authoritative evidence of the prior effective preset.

Instance create/update also uses Guardian-owned JVM preset normalization. A closed preset-id vocabulary and minimal typed resolution distinguish Automatic, a supported explicit preset, and an unknown value reset to Automatic without retaining the input. The central Guardian copy authority iterates that vocabulary to author the create-view catalog and is the only author of the bounded unknown-reset `guardian_notice`; Application only places those typed projections in its response. Instance update applies the same normalization before persistence. Raw Java path and raw JVM argument overrides may be stored for backend launch preparation, but update responses redact those strings and launch preflight turns them into Guardian facts before any user-facing outcome.

Launch preparation and launch-session execution are backend-owned under the Application launch modules. Guardian centrally projects the typed preflight mode, effective verdict, and diagnosis count into the bounded `guardian_launch_safety_decision` stage-evidence record; Application composes that record with its independently authored Performance input evidence. API launch routes adapt HTTP/SSE transport and call those Application entrypoints; they do not own Guardian preflight, runtime repair, startup recovery, stage copy, or session outcome policy. The frontend does not decide readiness, compute effective memory policy, classify Java/JVM failures, choose Guardian/Healing precedence, or synthesize crash warnings.

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
Install/download systems emit redacted facts and keep private selected descriptors for repair authorization. `apps/api/src/application/install.rs` owns install operation identity, live install queue command/status view models, duplicate suppression, retry queue placement, pending queue removal, worker coordination, journal recording, progress redaction, loader install coordination, Guardian artifact repair invocation, repair outcome shaping, install status responses, and backend-authored failed-install view models for public failure copy, retry availability, and repair action state. `apps/api/src/state/installs.rs` stores live install sessions and the in-memory queue snapshot; the queue is not the durable proof surface. `apps/api/src/application/install/stream.rs` owns install and loader progress stream preparation, history replay, terminal handling, closed-channel cleanup, missing-session copy, and progress event serialization. Install and loader routes parse requests, call Application entrypoints, and serialize backend-authored responses; they do not own repair policy, retry safety, operation journal semantics, queue copy, or stream policy.

Guardian can repair only launcher-managed artifacts when the failed checksum, size, or selected-missing-artifact fact exactly matches a private selected descriptor. Core Minecraft emits selected facts/descriptors with Guardian-safe semantic target ids and requires valid SHA-1 metadata for selected launcher-managed artifacts; missing or invalid checksum metadata fails closed as metadata evidence instead of being treated as verified. Application adapts each terminal failure to one evidence vector. Every nonempty vector enters one move-only Guardian install assessment; an empty lower-level fact payload produces no assessment or repair. The assessment owns the sole policy decision and its optional terminal projection; a repair branch must consume the same assessment to mint either a `QuarantineRedownload` or `MissingDownload` authorization and cannot rebuild evidence or request a second decision. Runtime ready-marker repair uses the same consuming authorization pattern. Artifact authorizations own the validated descriptor and require its semantic target to equal the accepted policy target, so exact executors accept no separate destination or source. Only the exact typed executor can consume each authorization. At execution time, live failure-memory suppression, journal transitions, target/root or existence checks, and effect postconditions still gate mutation. Missing-download execution rechecks that the destination remains absent before downloading/promoting without quarantine; runtime execution rebinds the authorized target to the owned runtime root before recreating its marker. Provider, network, and interrupted failures produce retry-oriented Guardian outcomes. Invalid provider metadata, locked launcher-managed files, filesystem permission denial, temp-write failure, atomic promotion failure, and ownership refusal produce blocking Guardian outcomes and never mutate automatically. File locks and permission failures retain distinct diagnoses and bounded guidance without exposing paths. Temp-discard and success facts do not trigger automatic artifact repair or terminal Guardian outcomes today; they remain bounded evidence/status inputs and must not expose raw provider or filesystem output.

After Guardian repairs a launcher-managed install artifact, one Application-owned resume gate shared by vanilla and loader workers may resume the same install once; blocked, failed, suppressed, and absent repair outcomes do not spend it. Guardian repair status alone is not install success: the resumed worker must complete the full install before success progress is emitted. If the resumed install fails, the final backend failure remains terminal and journaled. Loader library downloads emit the same selected library evidence as vanilla library downloads, so loader library artifact failures can feed this same repair path.

Loader failures follow the same authority split. Core owns separate closed vocabularies for pre-operation request/catalog/build failures and active-install Guardian failures. Provider categories exist independently at those boundaries; sharing a safe wire spelling does not merge their types or ownership. Pre-operation failures return before install-session or operation-journal allocation. The Core worker then returns a closed typed active/base/artifact result: active failures own their consistent kind and source, the base variant owns its lower-level error plus facts and private descriptors, and the artifact variant owns its facts and private descriptors. Application exhaustively consumes each result once, sending active failures only to the Guardian recorder and delegated payloads only through the real base-install or artifact fact and repair path. If a pre-operation source defensively reaches worker conversion, Core normalizes it to an active `install_execution_failed` failure rather than relying on an unreachable branch. Each active-install kind becomes Guardian evidence and a diagnosis. Application records active provider failures into the install operation journal, and Guardian authors retry or repeated-failure block outcomes. Installer execution and processor failures use launcher-managed version targets and blocking copy in the Installing phase. State failure memory suppresses repeated loader-provider retries within the cooldown so connectivity failures cannot spin indefinitely.

Loader base-version dependencies also use the install integrity model. Loader install only treats a base Minecraft version as already installed when its version metadata, client jar, incomplete marker state, and selected base libraries are ready; otherwise it reruns the vanilla install path so inherited loader libraries cannot stay missing behind an existing base JSON/JAR pair. When a loader install must install or repair a base Minecraft version internally, core returns the same redacted vanilla install facts and selected descriptors to Application through a bounded base-install failure instead of hiding the lower-level install failure. Application records those facts/outcomes and can invoke Guardian artifact repair using the descriptors. If no lower-level safety facts are available, the dependent loader operation records `install_dependency_failed` and blocks with Guardian-authored public copy.

## Performance
Performance owns plan resolution, health and composition semantics, composition locks, managed artifact mutation logic, rollback snapshots, benchmark/qualification semantics, and durable performance operations. State owns the runtime authority that decides which registered instance identity that logic may inspect or mutate and when access is admitted.

`apps/api/src/application/performance.rs` and `apps/api/src/application/performance/workflow.rs` own performance route orchestration: status and refresh response shaping, plan and health result carriers, Guardian performance fact adaptation, proof/view-model construction, rollback response semantics, durable operation creation/resume/status, terminal operation-proof composition, and identity-only command staging. `apps/api/src/routes/performance.rs` only parses HTTP requests, calls those Application entrypoints, and serializes backend-authored responses. Production callers do not pass raw instance or `mods/` paths into core state, health, rollback, or mutation helpers.

For instance-scoped work, State requires a canonical registered instance id and derives its exact storage from the instance registry. It serializes inspection, health, evidence, rollback list/preflight, install, remove, rollback, and recovery per instance while permitting unrelated instances to progress. Reads use only this managed per-instance boundary and remain available while a game session runs when the instance is clean; a latched read cannot recover until the active session ends. Every admission takes the shared instance lifecycle boundary, resolves the registry entry again, and rechecks session state before any mutation or recovery. Launch and instance deletion use that same lifecycle boundary, so they cannot overlap those effects, and queued or resumed work cannot recreate a deleted instance. Deletion owns non-destructive Known-good and Performance retirement reservations before registry mutation. Registry absence is the sole retirement commit point: present-instance failure drops both reservations unchanged, while committed Performance retirement removes only its pointer-matching owner entry rather than retaining a tombstone. The admitted delete request transfers this whole transaction through its request producer handoff; create compensation uses a child of its already-live producer, so neither path reopens global producer admission during request drain.

An accepted mutation moves to an App-owned task before its caller waits, so caller cancellation cannot abandon or overlap the effect. If the effect outcome is indeterminate, State latches that instance. A later admission or shutdown retry moves recovery to an App-owned task under the same instance lifecycle and managed gates. Performance reconciles only current strict protocols and clears the latch only after proving state publication/deletion, removal and replacement backups, managed download temps, rollback publication/restore/retention, strict metadata, and every tracked artifact digest. First-time install and rollback publication retains an exact digest- and identity-bound hardlink obligation until strict state commits or compensation completes, so recovery can distinguish launcher-owned uncommitted bytes from matching user replacements. Unknown, corrupt, symlinked, nonregular, oversized, conflicting, or user-replaced material is preserved and remains latched; there is no manual clear or compatibility path. Shutdown never runs this recovery until session settlement has succeeded.

Guardian consumes Performance facts for invalid remote rules, degraded or fallback health, invalid ownership, and rollback availability. Guardian can recommend or record degraded/fallback/rollback-safe states, but concrete composition mutation stays in Performance behind State admission and must respect composition-managed ownership. Guardian does not bypass identity, lifecycle, or indeterminate-state gates.

Benchmark qualification status labels/tones, target labels, suite/schema labels, missing-evidence summaries, suite/evidence summaries, and per-target required/suite/proof/missing copy are Performance-authored view-model fields. Performance rule-status Settings copy/tone and Performance health summaries are also backend-authored: source/channel labels, validation tone, warning display copy, health copy, bounded composition descriptor, apply/repair action availability, and rollback action availability come from Application/Performance shaping, not frontend token interpretation. Queued Performance operation status responses are also Performance Application-authored view-model boundaries: terminality, completion, progress phase/count, title/detail, and tone come from backend status shaping, not frontend token interpretation. Benchmark suite-driver stop/resume/qualification-check availability is Launch Application workflow state because that boundary owns suite driver lifecycle, active-session checks, and explicit resume/stop behavior. Launch proof outcome, comparison, Guardian/Healing evidence precedence, and resource-budget pressure/details are also Launch Application-authored export view-model fields built from sanitized local proof records. The frontend renders those fields and does not reconstruct Guardian or Performance severity from raw status tokens.

Strict current-schema restart-resume records for performance operations and benchmark suite drivers, plus strict current-schema benchmark suite manifests, are State-owned launcher data. When those records cannot be trusted at startup because the directory cannot be read, an entry cannot be read, a status file is invalid, an id or filename is unsafe/noncanonical, timestamps are invalid, or records conflict, State records only bounded load-issue kinds/counts. Guardian converts the aggregate into the `persisted_state_schema_invalid` diagnosis and a redacted startup warning on `/api/v1/status`. Invalid nonterminal Performance status is retained only for fail-safe terminal reconciliation and is never replayed; journal-only Performance operations are likewise terminalized without replay. Rejected suite manifests cannot become runtime mutation inputs, and runtime suite readers use only committed admitted memory. This path never rewrites or deletes user-owned or unknown-owned files.

Guardian failure memory itself is State-owned launcher data. State loads and saves the strict snapshot under `<config_dir>/guardian/failure-memory.json`, validates redacted descriptors, bounds retention, and writes through the managed atomic file capability. State stores and returns the bounded snapshot; it does not select advisory entries, decide safety policy, or author public copy. Guardian remains the policy owner: it records observations, filters the Application-supplied snapshot, chooses suppression/repair/retry/block/degrade behavior, and lets expired launch-recovery suppression windows allow a new bounded attempt instead of permanently blocking recovery. A suppressed or pre-effect blocked repair is journal-only and preserves the prior attempted `Failed` or `Repaired` memory entry, including its count and expiry; only an attempted terminal result or a later successful retry replaces that loop-control state. Launch-recovery outcomes use the canonical instance identity as their durable target, while session ids remain transient tracing and operation-lifecycle context, so a suppression window can govern a later session for the same instance without affecting another instance. That instance target remains `LauncherManaged` because the authorized effect is a launcher-owned, one-attempt process override; it does not claim ownership of or mutate the instance's user-owned files. Each accepted terminal launch-crash class records one instance-targeted observation keyed by its stable class name and increments only its occurrence count; it does not claim an action, repair attempt, suppression window, fallback, user decision, or content/intent hash.

Operation journals are also State-owned launcher data. State loads and saves a bounded strict snapshot under `<config_dir>/state/operation-journals.json`, validates structured ids and redacted public facts, prunes current-state retention, and writes through the shared persistence coordinator. Immediate transitions stay outside the committed public view until physical commit; accepted work and exact retry reconciliation remain store-owned if a caller is cancelled. Guardian and Application author facts, diagnosis ids, repair outcomes, and workflow progress; State stores them without deciding safety policy, privacy policy, retry eligibility, repair authorization, proof export, or telemetry. Install status and event streams can use a restart-loaded journal to replay bounded terminal progress and Guardian repair summaries when the transient install session snapshot no longer exists. Queued and synchronous Performance execution persist a durable operation status id plus matching journal identity, so restart can validate effect-started, terminal-intent, terminal, and proof records before publishing or resuming anything. Observability can compact a matching terminal journal into a redacted operation proof for install and Performance status surfaces, preserving Guardian-authored diagnosis/outcome evidence and latest generated facts while dropping or redacting sensitive free text.

## Observability, Redaction, And Telemetry
Non-repairable install outcomes persist only a closed decision id plus the authored summary and optional bounded detail. Replay validates those facts against the typed diagnosis/decision copy coordinate and reauthors guidance from the central copy table; it never expands persisted guidance tokens or trusts journal prose as guidance. Missing, malformed, unsafe, or inconsistent facts yield no Guardian public outcome instead of fallback copy, and Application carries no retry fallback prose.

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
