# ADR 0004: Performance Internal Namespace Ownership
Status: Accepted

## Context
Performance graph mutations must preserve unknown or concurrently replaced files in
the live `mods/` directory while remaining recoverable after interruption. State,
artifact intent, rollback data, and cleanup receipts therefore need one authority
boundary that does not depend on reconstructing ambient paths.

## Decision
Reserve the complete instance-local `.axial-performance/**` namespace for launcher
state. It is not a user-content namespace. The live `mods/` namespace remains
user-controlled: unknown files are user-owned, and a managed mutation may not
delete or replace content that does not match admitted managed metadata.

Production Performance code receives a held filesystem directory capability and
resolves every managed descendant relative to it. It does not retain a parallel raw
root path. State publication uses fixed staged, backup, deletion-intent, and
deletion-park bindings. Artifact additions write strict intent metadata before
create-only publication. Artifact removals move exact admitted files into a
digest-bound reserved directory until the committed state chooses restoration or
deletion. Rollback candidates contain strict metadata and verified artifact copies,
then publish as one no-replace directory move. None of these protocols uses hard
links.

Cleanup validates size and digests before moving the exact admitted file into
`.axial-performance/quarantine/<sha512>/`. It then admits that reserved binding as a
typed park and settles removal. Restart recovery performs a bounded
capability-relative enumeration, validates topology, rehashes each retained file,
and reconstructs the park only when the directory digest and bytes agree. Unknown,
mismatched, or malformed entries are preserved and keep mutation latched. The
authority never extends back to deletion of a live `mods/` binding.

Publication, commit, rollback restore, and external target-effect boundaries prove
the cleanup quarantine empty before effects. A normal successful transaction leaves
the quarantine empty.

## Consequences
Positive:
- successful mutations reclaim temporary parked artifacts instead of consuming
  quarantine capacity indefinitely
- live user content keeps the exact identity and replacement protections of the
  managed-composition boundary
- strict intent, removal, rollback, and cleanup residue can reconcile after restart
- malformed or unverifiable residue remains fail-closed

Tradeoffs:
- processes with direct filesystem access must not use `.axial-performance/**` for
  user content or mutate it while the launcher is operating
- restart cleanup rehashes retained internal files before reconstructing typed
  deletion authority
- observable direct external mutation of the reserved namespace makes recovery fail
  closed until the instance data is reset
- on Linux, an actively malicious same-UID writer can race the unavoidable
  name-based unlink after identity validation; that actor is outside the preservation
  guarantee and already has direct deletion authority over application data
