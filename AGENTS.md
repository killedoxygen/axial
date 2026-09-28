# Agents for Axial

Read and follow [docs/CONVENTIONS.md](docs/CONVENTIONS.md) before making changes.

Active rewrite development is on `main`. Preserve existing commit history; the earlier rewrite branch is a recovery checkpoint, not the working branch. Full non-Guardian parity remains the completion condition.

## Architecture quality

- Review each integrated feature and the scheduled review's changed scope for architectural drift, not just test failures.
- Let the containing feature supply its namespace. Avoid repeated feature prefixes, redundant folder levels, and one-file wrapper directories unless they clarify a real boundary. Do not rename stable public contracts merely for style.
- Keep one owner for each behavior, state and wire contract. Reuse the existing workflow before adding another coordinator, persistence table, adapter or implementation. Accepted work owns its completion and cleanup; a disposable request waiter must not own those effects.
- Prefer direct functions and concrete types. Add an abstraction only for a current repeated need or a genuine external boundary; remove superseded paths when their replacement is verified. Check actual entrypoints and existing preconditions before adding machinery for a suspected race.
- Keep code, comments and tests concise but explicit. Compose required production owners in fixtures: an earlier dependency refusal does not prove the intended failure boundary. Avoid parallel decision models tested only against themselves. Exercise actual serialized responses, including omitted empty fields and wire enum values; omit comments that restate code.
- Preserve typed contracts at their owner: use their codecs, bounds and lossless wire encodings, not unrelated free-text heuristics or JavaScript-rounded integers. Sanitizing public errors must retain safe failure causes; never log raw credentials, command arguments or provider details.
- Bound total batch work, not just individual items. Prepare shared immutable inputs once per projection; keep exact verification at mutation admission.
- Validate imported history against its source schema and recorded time, not today's plan or a later snapshot. Preserve historical references as evidence; never fabricate live authority to fit current runtime types.
- Treat lost mutation responses as unknown outcomes, not success or cancellation. Classify refusals by the owner's commit boundary, not HTTP status class alone. Reconcile through the existing operation/status owner; do not blindly replay writes or add a parallel client journal.
- Verify expected affected rows before acknowledging durable writes; SQL success alone does not prove publication. Commit proof and its acknowledgement atomically.
- Fence asynchronous snapshot publication against every independently changing revision in that projection, including ownership that changes and returns to its original value while a read is pending. A fresh request alone does not prevent stale publication; keep the fence in the existing state owner.
- Create native filesystem fixtures under a canonicalized temporary parent. Do not weaken path admission to accommodate host aliases. Portable tests must not assume filesystem case sensitivity, host-specific error-code meanings or visible scratch names.
- Simplicity must preserve feature parity, UI behavior, filesystem authority, concurrency and recovery guarantees. Keep verified immutable inputs separate from process-writable scratch. Best-effort metadata is not an unresolved file effect and must not acquire shutdown-blocking recovery machinery.
- Correct evidenced drift within current ownership and verify the change. Record findings in [the architecture review](docs/rewrite/results/architecture-review.md); add only recurring, actionable lessons here, merging existing rules instead of accumulating anecdotes.
