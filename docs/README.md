# Docs Index
This is the entrypoint for the repo docs. Keep this short, current, and useful.

## Reading order
1. `docs/CONVENTIONS.md`
2. `docs/ARCHITECTURE.md`
3. subsystem architecture docs relevant to the work
4. `docs/adr/` for major design decisions and tradeoffs

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

### Subsystem architecture docs
Current subsystem docs:
- `docs/GUARDIAN-ARCHITECTURE.md`
- `docs/LOADER-ARCHITECTURE.md`
- `docs/VERSION-METADATA-ARCHITECTURE.md`

Use them for:
- one subsystem’s responsibilities
- its internal pipeline
- boundaries with neighboring layers
- the contract the rest of the system should rely on

Do not use them for:
- broad repo onboarding
- unrelated design decisions
- temporary implementation notes

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
- If one subsystem changes internally: update that subsystem’s architecture doc.
- If the rules for working in the repo change: update `docs/CONVENTIONS.md`.
- If a major decision is made and the reasoning matters long-term: add an ADR.
- If the docs structure itself changes: update this file.

## Current gaps
- There is still no dedicated onboarding doc for a new contributor reading the codebase for the first time.
- There is still no explicit product/domain glossary.
- The ADR set is new and should stay selective instead of turning into a changelog.
