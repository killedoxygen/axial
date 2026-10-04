# Conventions

The preserved application lives in `legacy/`. Its conventions remain at `legacy/docs/CONVENTIONS.md` and govern that reference tree. Do not modify legacy source during the rewrite. The superseded rewrite-added preference bridge is removed; predecessor-profile compatibility is outside this breaking pre-release.

## Replacement

- Follow `docs/adr/0007-feature-owned-rewrite.md`, `docs/rewrite/delivery.md`, and `docs/rewrite/work-packages.json`.
- Preserve all non-Guardian behavior and the current UI. Reuse views, assets, styling and interactions; no redesign.
- Rust product logic belongs in feature-owned modules under `core/app`; HTTP adapters in `apps/api`; native-only integration in `apps/desktop`.
- Retain proven leaf implementations through narrow boundaries. Do not depend on the legacy Application/State/Guardian orchestration in the replacement.
- Generated public types have one owner. Private feature interfaces must be agreed with their consumers before integration. Keep domain-specific lifecycles distinct.
- Preact, named exports, signals/actions and small workflow machines remain the frontend conventions. No manual mirrors of backend state.
- Preserve exact filesystem authority and retained operation lifetimes; do not simplify safe ownership into caller-authored paths.
- Use behavior-based names, focused tests and normal functions. No generic framework without a current repeated requirement.
- Use `apply_patch` for file edits. Never overwrite another owner's files, manifests, registrations or generated output. Request shared changes from the integration owner.
- Only the integration owner runs shared Cargo/build commands. Workers supply focused test commands and may run isolated non-writing checks. Capture test output to logs and report tails.
- Keep the replacement profile, keyring namespace and mutable payloads independent from the old application. No installed-user or production mutations.
- Tests and source inventory are not proof of runtime parity. Keep incomplete features visibly incomplete; no fake success paths.

## Ownership

`docs/rewrite/work-packages.json` is the package inventory. During initial execution, the main integration owner owns root manifests, crate roots, module registrations, generated wire integration and shared verification. Feature owners may author private source and unit tests within their assigned paths, but cannot declare integrated completion until real consumers pass.

The user authorized a new branch and archival relocation. `legacy/` retains the original project, including local generated files and build output. The original branch remains available in Git. Do not delete legacy files or use the baseline's external library as a writable replacement profile.
