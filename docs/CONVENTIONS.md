# Conventions

- Preserve the current UI and all non-Guardian behaviour. UI changes fix a specific, named problem; no redesigns.
- Product logic lives in feature modules under `core/app`; HTTP adapters in `apps/api`; native-only integration in `apps/desktop`. Do not depend on `legacy/`.
- Prefer plain functions and concrete types. Add an abstraction only for a current repeated need or a real external boundary.
- Name things by purpose and let the enclosing module supply context.
- Frontend: Preact, named exports, signals and actions. Use generated wire types; no manual mirrors of backend state.
- File effects go through `core/fs` capabilities. Never weaken path admission or recovery to make a test pass.
- Tests exercise real behaviour: recovery, filesystem safety, serialized responses, user-visible results. Do not test source text, documentation or test tooling.
- Run shared Cargo commands through `node scripts/cargo-target.mjs run -- cargo ...` so concurrent builds do not collide.
- Commits follow Conventional Commits. Keep run logs and local evidence in the ignored `.rewrite-logs/`, not in Git.
