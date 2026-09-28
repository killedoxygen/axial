# Retained Instance Deletion Transactions

## Status

Accepted.

## Context

Instance deletion crosses registry persistence, a canonical instance tree,
instance lifecycle, managed Performance state, known-good inventories, and user-mod
witnesses. Treating those effects as independent cleanup made cancellation and
restart ambiguous: filesystem deletion could precede durable registry absence,
post-commit cleanup failure could be reported as a failed deletion, and shutdown
had no exact owner for unfinished work.

## Decision

State owns one capacity-one deletion coordinator and one move-only retained
transaction carrier. Delete-files parks an existing canonical tree in one
deterministic, identity-bound tombstone or proves the canonical tree already
absent before persisting a registry snapshot containing one structured
pending-deletion marker. Keep-files creates neither tombstone nor marker and may
retire only an already-existing in-memory Performance entry.

The successful registry persistence acknowledgement is the only commit point.
Retryable preparation, persistence, and restoration remain inside the original
joinable producer with capped exponential backoff. They continue until the
registry commit is durable or no effect/restoration is proven; if request drain
starts first, that owner returns the exact carrier to the coordinator before it
exits. A proven no-effect/restoration or other terminal pre-commit failure
reopens auxiliary reservations and returns an error. After commit, State retires
lifecycle, Performance, known-good, and witness state before removing the parked
tree and clearing the marker. Only post-commit retryable work transfers to the
preclaimed tracked producer. Public deletion returns success after durable commit
even when that producer still owns cleanup.

Startup accepts only the closed matrix of no work, restore one live parked tree,
or complete one matching pending deletion. Every mixed, duplicate, or mismatched
topology fails closed. Startup waiter cancellation transfers an active carrier to
bounded self-shutdown rather than retaining an unreturned application state.
Shutdown closes deletion after producers drain and before dependent stores.

## Consequences

Registry mutation remains blocked while an exact carrier is unsettled. Deletion
throughput is intentionally serialized, but ownership, restart behavior, and the
authoritative result are unambiguous. Persistent local failure can keep shutdown
incomplete; it cannot be downgraded to success or abandoned as best-effort cleanup.
