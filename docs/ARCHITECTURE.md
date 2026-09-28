# Current architecture

The repository is in an incomplete rewrite, on `feat/clean-rewrite`. The preserved application is under `legacy/`; its architecture is documented in [legacy/docs/ARCHITECTURE.md](../legacy/docs/ARCHITECTURE.md).

The replacement currently consists of feature-owned Rust source under `core/app`, retained capability and domain leaf crates under `core/fs`, `core/resource`, `core/minecraft` and `core/performance`, local HTTP transport under `apps/api`, and native shell source under `apps/desktop`. The existing Preact interface is retained under `frontend`.

The source is not yet a fully connected application. `core/app/src/lib.rs` shows the currently compiled application modules; unregistered source is not build evidence. Composition and routes are being integrated explicitly, with shared build writers serialized by `scripts/cargo-target.mjs`.

The target boundaries and rationale are in [the architecture decision](adr/0007-feature-owned-rewrite.md). The [current integration status](rewrite/results/integration.md) distinguishes evidence from remaining work. All non-Guardian behavior and the existing UI remain required.
