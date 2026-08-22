<div align="right">
  <a href="README.ko.md">KO</a> | <strong>EN</strong>
</div>

# Codex CLI Custom

## Why this fork exists

Stock Codex treats authentication and much of runtime ownership as process-level concerns. That becomes limiting when one long-lived app-server and TUI must serve several accounts, keep each thread bound to the correct account, and let another process take over an explicitly closed thread without corrupting its history.

This fork turns those implicit boundaries into explicit contracts:

- multiple isolated account slots inside one app-server;
- an immutable execution account for each turn, with a guarded next-turn switch;
- durable single-writer authority and strict handoff;
- sanitized session identity, lifecycle, persistence, and allowed-control state for external consumers;
- TUI controls that do not require leaving one account-specific app and attaching to another; and
- installable, structured plugin commands plus bounded UI-only presentation components.

The goal is not to replace workflow or relay systems. It is to give them authoritative app-server state and safe controls instead of forcing them to infer ownership from a PID, socket, title, current directory, or timeout.

> **Experimental:** This is not an official OpenAI distribution.

## 0.149 publication status

The target is upstream [`rust-v0.149.0`](https://github.com/openai/codex/releases/tag/rust-v0.149.0), commit `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`.

| Area | Current status |
|---|---|
| P001–P011 implementation and focused checks | Complete |
| Ordered 0.149 patch export and clean-apply verification | Complete; 11 patches reproduce tree `4d993b8b5960de01c6d2bafb4c1a7749c51280db` |
| macOS arm64 release build and artifacts | Complete; [run 32537192127](https://github.com/iLfounder/codex-cli-custom/actions/runs/32537192127), artifact `9467506046` |
| Final independent reviews | One internal Codex and one external Opus fresh-context review completed; confirmed findings were source-checked and corrected |
| 0.149 publication | Complete |

The current candidate is [`custom-patches/rust-v0.149.0`](custom-patches/rust-v0.149.0/). The files under [`custom-patches/rust-v0.148.0`](custom-patches/rust-v0.148.0/) are retained only as the previous release series.

## Intended P001–P011 boundaries

Each number is a reviewable feature patch. The series is ordered because later patches consume earlier contracts; the numbering also makes future upstream updates a bounded per-feature port instead of one large fork merge.

### P001 — Shared writer authority

**Why:** Two account homes may share one session and SQLite store, so account-local lock files cannot prevent duplicate writers.

**Boundary:** Put durable `storeId` and monotonic `writerGeneration` authority in the shared SQLite root, retain advisory process locks, and reject stale ownership before mutation. No automatic writer stealing.

### P002 — Session runtime protocol

**Why:** TUI, relay adapters, and external orchestrators need one stable app-server v2 contract before control behavior is added.

**Boundary:** Define bounded `sessionRuntime`, account-slot, login, relinquish, and switch DTOs, methods, notifications, pagination, and compile-safe stubs. It does not implement the controls themselves.

### P003 — Multi-account registry

**Why:** A single server must host several accounts without exposing credential paths or silently falling back to the process-default identity.

**Boundary:** Add a host-managed account-slot manifest, private per-slot auth homes and model caches, a compatibility default slot, revision-bound listing, and fail-closed handling for unsupported process-global external/workload identities.

### P004 — Durable execution binding and history

**Why:** Resume, fork, child, and review threads must continue under the account that owns their work.

**Boundary:** Persist thread-to-slot binding with generation CAS, inherit it across thread creation paths, and record immutable turn provenance. A slot ID alone is never accepted as fresh credential identity.

### P005 — Account propagation to every consumer

**Why:** Switching the model client is insufficient if connectors, apps, plugins, MCP, telemetry, memory, review, or cost polling still use default or stale credentials.

**Boundary:** Capture one account runtime per turn and propagate it through every credential-sensitive consumer, including account-scoped services and caches. Mid-turn credential mixing remains forbidden.

### P006 — Externally visible session runtime

**Why:** External management should know what a session is doing and what it may safely do next without guessing.

**Boundary:** Publish sanitized, revisioned snapshots and sequenced notifications for stable identity, lifecycle and waiting state, subscribers, writer authority, persistence health/position, account binding, and currently allowed actions. Operation replay is bounded and no credential paths or secrets are exposed.

### P007 — Zero-restart account registration

**Why:** Adding or reauthenticating an account should not require restarting the app-server or disconnecting every TUI.

**Boundary:** Provide slot-scoped API-key, browser, device-code, and external-refresh login operations plus secondary-slot logout. Exact connection ownership and generation CAS reject late OAuth or same-slot completions that have already been superseded.

### P008 — Strict writer relinquish

**Why:** Closing a TUI is not proof that the writer has flushed, materialized, and released the thread for another account or process.

**Boundary:** Serialize new work against the close transition; require flush, materialization, path sync, recorder shutdown, and exact-generation release to succeed; retain the owner on failure; and publish terminal `NotLoaded`, `Released`, and matching `ThreadClosed` before reopening admission.

### P009 — Zero-restart execution-account switch

**Why:** An attached idle thread should be able to change its owning account without leaving the TUI or reconnecting to another account-specific app-server.

**Boundary:** Prepare the full target runtime before durable binding CAS and infallible pointer publication. The active turn keeps its captured account; the next turn uses the target. MCP, plugins, realtime, telemetry/network provenance, Guardian sampling, Goal runtime, and other persistent account-bound consumers are rebuilt or refreshed, including same-slot reauthentication.

**Status:** Implemented in P009.

### P010 — TUI account, exit, clear, and new-thread controls

**Why:** The safety contracts are useful only if the terminal client exposes them without pretending a timeout or disconnect was success.

**Boundary:** Add an account picker and account/logout controls, make explicit exit wait for strict terminal release, and expose typed `threadClear`/`threadNew` agent controls to both new and legacy-resumed threads. Clear/new replies first and changes the UI only after the exact successful completion event.

**Status:** Implemented in P010.

### P011 — Installable structured plugin commands and ephemeral presentation

**Why:** Skills-as-text-slash-commands are not a sufficient component model. Plugins need typed actions and relay-friendly UI elements without injecting control data into the model transcript.

**Boundary:** Preserve legacy plugin command paths while adding a normalized contribution overlay. Commands use canonical `/namespace:name` and may use `/name` only when unique. Targets are limited to a bounded prompt, an exact MCP tool, an allowlisted Rust app-server action such as goal get/set/clear, or a packaged executable with fixed argv, no shell, and the existing approval/sandbox path.

Plugins may append bounded card, notice, or progress items to the exact thread's current subscribers and TUI. These items are ephemeral: they never enter rollout history, model context, or durable conversation history. `llc-relay` remains the routing and message-job authority.

**Status:** Implemented in P011.

## Runtime and relay boundary

The custom app-server is the authority for account execution, thread writer ownership, persistence state, and safe control admission. External systems can consume those values and associate sessions with workflow roles, but this fork does not make a relay job equivalent to workflow state or responsibility assignment.

`llc-relay` continues to move messages among Codex and Claude sessions. Plugin cards/notices/progress are a typed presentation surface for current subscribers, not a replacement transport, acknowledgement ledger, or proof that an agent completed a relayed request.

## Packaging and updates

The 0.149 candidate is eleven ordered exact-base patches with a digest manifest and clean-tree applier. Apply it only to the exact upstream commit:

```sh
git checkout 758ef40f50c1a458425c7cfbf1eb12cbc07af0b0
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD"
```

The applier rejects a dirty or wrong-base worktree, verifies every patch digest, applies P001–P011 in order, and requires final tree `4d993b8b5960de01c6d2bafb4c1a7749c51280db`.

This separation is the maintenance strategy: when upstream advances, each P-number can be inspected, adapted, and verified against its own feature boundary.

### Upgrading an existing 0.148 custom state store

The older custom series used migration numbers that official 0.149 later claimed. The new binary therefore stops without changing the database when it detects those legacy rows. Stop every older TUI and app-server sharing that state store, then start the 0.149 build once with `CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1`. Exact checksums, table definitions, and custom migration metadata are revalidated before the atomic adoption. Unknown or partial schemas remain fail-closed. Remove the variable after the one-time cutover; old binaries must not reopen the migrated store.

## Build, review, and publication

[Actions run 32537192127](https://github.com/iLfounder/codex-cli-custom/actions/runs/32537192127) clean-applied P001–P011 and built both release binaries on macOS arm64. Artifact `9467506046` records patched tree `4d993b8b5960de01c6d2bafb4c1a7749c51280db`; deterministic Cargo.lock normalization produced compiled tree `c7033f3d9b42fd45391232ef7a266872bb233e20` and lock SHA-256 `598546764c876b93c6a59edcd5879b8c1b17cdf7c8df023fb051bcfb755a81f4`. The GitHub artifact digest is `sha256:3ec9679a8a81bb868993080a33dd51c4be11734806dcb2a46127877e8bd9733e`.

The internal Codex and external Opus reviewers inspected the same frozen predecessor candidate independently. The lead verified each finding against source and call chains, applied only the confirmed defects, regenerated the exact eleven-patch series, and rebuilt the corrected tree above.

## Historical working notes

The sanitized design history and earlier build notes are preserved as non-authoritative background under [`docs/handoff.md`](docs/handoff.md) and [`docs/codex-rs-build-guide.md`](docs/codex-rs-build-guide.md). They predate the current 0.149 design and are not the public runtime contract.

## License

Upstream Codex and this patch series are distributed under the terms in [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
