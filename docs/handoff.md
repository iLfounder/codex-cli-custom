# Historical design handoff

This is a sanitized technical summary of the investigation that preceded the
current `rust-v0.149.0` patch series. It contains no live runtime identifiers,
account inventory, or machine-local paths. The current contract is the numbered
patch series and the top-level README.

## Original failure mode

Multiple Codex homes could share session JSONL and SQLite state while placing
writer locks under different home directories. Two processes could therefore
believe they owned the same thread and append concurrently. A damaged append
sequence could leave the canonical JSONL ahead of the SQLite projection, making
an intact thread appear missing or stale in the TUI.

The separate TUI working-directory filter could also hide a valid thread. That
display issue did not explain the writer collision or projection lag.

## Design conclusions

The investigation established these boundaries:

1. Writer coordination must use the SQLite authority home, not the caller's
   authentication home.
2. A thread writer needs a persistent store identity and monotonic generation,
   not a PID, socket, or process-local boolean.
3. Ordinary disconnect is not proof of durable release. Explicit relinquish
   must flush, materialize, synchronize, close, and release in that order.
4. Archive and delete are lifecycle operations, not substitutes for writer
   handoff.
5. Runtime state exposed to external controllers must be sanitized and fenced
   by epoch, revision, and generation.
6. An execution account belongs to a thread and turn. Authentication resources,
   model clients, MCP, plugins, telemetry, and other long-lived consumers must
   move together at a next-turn boundary.

## Mapping to the current patch series

- P001 establishes shared writer authority and generation fencing.
- P004 persists thread and turn execution-account provenance.
- P006 publishes sanitized runtime snapshots and operation state.
- P008 implements strict relinquish and terminal notification pairing.
- P009 switches the complete account-bound runtime without changing the thread
  identifier or rewriting history.
- P010 exposes release-aware TUI controls.
- P011 adds bounded plugin commands and non-persistent presentation surfaces.

## Upgrade boundary

All processes sharing one state store must use a compatible patched build during
cutover. Replacing only the CLI file does not update already-running TUI or
app-server processes. Stop old owners cleanly, apply the ordered patch series to
the exact upstream base, build both binaries, and start the new owners from the
same release.

This note is background only. Exact source commit, patch digests, final tree,
build workflow, and current completion status are maintained in the repository
root and `custom-patches/rust-v0.149.0/series.toml`.
