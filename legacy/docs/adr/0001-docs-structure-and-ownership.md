# ADR 0001: Docs Structure And Ownership
Status: Accepted

## Context
The repo has grown enough that architecture information was spreading across a few large markdown files without a clear entrypoint or clear ownership boundaries.

That created a few problems:
- it was not obvious where to start reading
- it was not obvious which doc owned which kind of information
- current-state docs and decision-history docs were getting mixed together
- architecture changes could be documented, but the docs as a system still felt disorganized

## Decision
Use a four-part documentation structure:

1. `docs/README.md`
The docs index and reading guide.

2. `docs/CONVENTIONS.md`
Contributor rules and maintenance expectations.

3. `docs/ARCHITECTURE.md`
The current high-level launcher map and end-to-end flows.

4. subsystem architecture docs + `docs/adr/`
Subsystem docs describe current state.
ADRs capture why major decisions were made.

## Consequences
Positive:
- easier onboarding
- clearer doc ownership
- architecture docs can stay focused on current behavior
- rationale no longer needs to be stuffed into the same file as the current system map

Tradeoff:
- contributors now need to decide whether a change belongs in a current-state architecture doc, an ADR, or both

Rule of thumb:
- current behavior -> architecture doc
- durable rationale -> ADR
