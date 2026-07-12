# Docs Index
This is the entrypoint for the repo docs. Keep this short, current, and useful.

## Reading order
1. `docs/CONVENTIONS.md`
2. `docs/DESIGN.md` for frontend/UI work
3. `docs/ARCHITECTURE.md`
4. subsystem architecture docs relevant to the work
5. `docs/adr/` for major design decisions and tradeoffs

## Doc roles
### `docs/CONVENTIONS.md`
Working rules for contributors.

Use it for:
- codebase conventions
- repo rules
- documentation maintenance rules

Do not use it for:
- pipeline walkthroughs
- subsystem design explanation
- historical rationale

### `docs/ARCHITECTURE.md`
The high-level map of the launcher as it exists today.

Use it for:
- top-level topology
- end-to-end flows
- ownership boundaries between major layers
- where to look in the codebase

Do not use it for:
- all subsystem details
- decision history
- speculative future design

### `docs/DESIGN.md`
Frontend/product design guardrails for the desktop launcher.

Use it for:
- UI workflow changes
- visual/layout guardrails
- protected frontend surfaces
- deciding when desktop smoke is needed

Do not use it for:
- backend architecture
- product roadmap status
- historical implementation logs

### Subsystem architecture docs
Current subsystem docs:
- `docs/DISCORD-RPC.md`
- `docs/GUARDIAN-ARCHITECTURE.md`
- `docs/ICON-ASSETS.md`
- `docs/LOADER-ARCHITECTURE.md`
- `docs/TELEMETRY.md`
- `docs/VERSION-METADATA-ARCHITECTURE.md`

Use them for:
- one subsystem’s responsibilities
- its internal pipeline
- boundaries with neighboring layers
- the contract the rest of the system should rely on

`docs/GUARDIAN-ARCHITECTURE.md` is also the safety model entrypoint: Guardian is the horizontal safety and self-healing orchestrator across launch, runtime/JVM, install/download, performance, operation journals, redaction, and frontend notice boundaries.

Do not use them for:
- broad repo onboarding
- unrelated design decisions
- temporary implementation notes

### Generated coverage docs
`docs/GUARDIAN-INVARIANT-COVERAGE.md` is the human-readable projection of the strict
Guardian invariant coverage artifact. It is regenerated with the machine-readable JSON
fixture and must not be edited by hand.

### `plans/stabilization/`
Ignored local target architecture specs and stabilization execution control plane.

Use it for:
- stabilization system specs
- execution plans and active `PROGRESS.md`
- Guardian reasoning, repair, redaction, frontend-removal, and proof method specs

Do not use it for:
- historical logs
- broad product roadmap notes
- unplanned implementation drift

### `docs/adr/`
Architecture Decision Records.

Use ADRs for:
- major decisions that need rationale
- tradeoffs
- rejected alternatives
- policy shifts that future contributors will otherwise re-debate

Do not use ADRs for:
- current-state walkthroughs
- low-level implementation detail
- transient TODOs

## When to update what
- If the current pipeline changes: update `docs/ARCHITECTURE.md`.
- If frontend design guardrails change: update `docs/DESIGN.md`.
- If one subsystem changes internally: update that subsystem’s architecture doc.
- If Guardian invariant coverage changes: regenerate its JSON and Markdown artifacts together.
- If stabilization execution state changes: update local `plans/stabilization/execution/PROGRESS.md` as current state only.
- If the rules for working in the repo change: update `docs/CONVENTIONS.md`.
- If a major decision is made and the reasoning matters long-term: add an ADR.
- If the docs structure itself changes: update this file.

## Current gaps
- There is still no dedicated onboarding doc for a new contributor reading the codebase for the first time.
- There is still no explicit product/domain glossary.
- The ADR set is new and should stay selective instead of turning into a changelog.
