# Conventions

- Preserve all non-Guardian behaviour. Redesign UI only at the owner's request; otherwise UI changes fix a specific, named problem.
- Product logic lives in feature modules under `core/app`; HTTP adapters in `apps/api`; native-only integration in `apps/desktop`.
- Prefer plain functions and concrete types. Add an abstraction only for a current repeated need or a real external boundary.
- Name things by purpose and let the enclosing module supply context.
- Frontend: Preact, named exports, signals and actions. Use the ts-rs types in `frontend/src/generated` (`task contracts:generate`); no manual mirrors of backend state, enums or option lists.
- Frontend layout: one folder per feature (`launch/`, `install/`, `instances/`…) owning its `api.ts` (every call and path for that feature), its state and its presenters. Components call a feature's `api.ts` or state, never the transport. Shared plumbing lives in `platform/` (transport, native bridge, events) and `app/` (bootstrap, routing, shell, toasts).
- File effects go through `core/fs` capabilities. Never weaken path admission or recovery to make a test pass.
- Tests exercise real behaviour: recovery, filesystem safety, serialized responses, user-visible results. Do not test source text, documentation or test tooling.
- Run shared Cargo commands through `node scripts/cargo-target.mjs run -- cargo ...` so concurrent builds do not collide.
- Commits follow Conventional Commits. Keep run logs and local evidence in the ignored `.run-logs/`, not in Git.
