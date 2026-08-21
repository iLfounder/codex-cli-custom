<div align="right">
  <a href="README.ko.md">KO</a> | <strong>EN</strong>
</div>

# Codex CLI Custom

## Why this patch series exists

Running several accounts through one long-lived app-server and TUI creates a problem that process-level identity cannot solve: every session needs an explicit execution account, durable writer ownership, and a safe way to hand that ownership to another process. External orchestrators also need authoritative session state and permitted controls instead of inferring them from files, process lifetime, or timing.

This ordered patch series makes those boundaries explicit. It supports multiple account slots in one app-server/TUI, keeps authentication isolated per session and turn, enables sessions serving different workflow roles to retain the correct account context, provides observable session state and guarded control operations, and exposes enabled skills as slash commands.

This repository does not duplicate the full upstream source. It checks out one exact [`openai/codex`](https://github.com/openai/codex) commit and applies the verified P001–P011 series in order.

> **Experimental:** This is not an official OpenAI distribution. The current series applies only to `rust-v0.148.0`.

## Base and reproducibility

- Upstream tag: `rust-v0.148.0`
- Upstream commit: `3ba0f711642a888aec92a611a3f3b2211157ff89`
- Tree after all patches: `fe1cec7cc8a29dedd89896c4459474fb5cf2d54e`
- Manifest: [`custom-patches/rust-v0.148.0/series.toml`](custom-patches/rust-v0.148.0/series.toml)
- Applier: [`custom-patches/apply-series.sh`](custom-patches/apply-series.sh)

The applier requires a clean worktree at the exact upstream commit, verifies the SHA-256 digest of every patch, and checks the final Git tree. Patch numbers are an ordered dependency chain; they are not independent options and must not be skipped or reordered.

## Patch series

### P001 — Shared writer authority

**Intent:** Make session ownership unambiguous even when different account homes share the same session store. A persistent store ID and writer generation are kept in SQLite, while thread writer ownership can be probed without mutating it.

### P002 — Session runtime protocol

**Intent:** Give clients a stable contract before adding runtime behavior. App-server v2 gains DTOs, methods, and notifications for runtime snapshots and operations, strict relinquish, execution-account switching, and account-slot listing and login.

### P003 — Multi-account registry

**Intent:** Keep several accounts available inside one app-server without collapsing their identities. The default account remains a virtual compatibility slot, while additional slots use private homes, managed credential loading, revision-bound pagination, and fail-closed checks for conflicting process-wide identities.

### P004 — Durable execution binding and history

**Intent:** Ensure a thread resumes under the same execution account that previously owned its work. Thread-to-account bindings are persisted and updated with generation CAS; resume, fork, child, and review sessions inherit the binding, and each turn records immutable binding provenance.

### P005 — Propagate execution account to auth consumers

**Intent:** Prevent credentials or account-scoped state from crossing session boundaries. Model, connector, app, plugin, MCP, extension, memory, and review paths consume the account context captured for the thread or turn, with account-scoped services and caches.

### P006 — Publish session runtime state

**Intent:** Let external controllers observe and act on sessions without guessing. A sanitized `sessionRuntime` snapshot reports lifecycle and waiting state, subscribers, writer authority, persistence health and position, account binding, and currently allowed actions through revisioned snapshots and sequenced notifications.

### P007 — Live account registration

**Intent:** Add or reauthenticate an account slot without restarting the app-server. API-key, browser, device-code, and external-refresh login flows run as slot-scoped operations, with connection and generation checks protecting browser ownership and late responses.

### P008 — Strict thread writer relinquish

**Intent:** Release a session only after its durable state is safe for another owner to continue. New turns and control transitions are serialized, and the writer guard is released only after flush, materialization, sync, and recorder shutdown all succeed; failures preserve ownership and publish a stable cause.

### P009 — Hot execution-account switch

**Intent:** Switch an idle thread to another account without disconnecting the app-server or TUI. The target runtime is prepared before a durable binding CAS updates the in-memory pointer, while an active turn keeps its captured account and the next turn receives the new one.

### P010 — TUI session and account controls

**Intent:** Make multi-account session control usable directly from the terminal. The TUI adds an account picker, `/account`, slot-scoped `/logout`, and strict shutdown/release handling that waits for both writer release and terminal `ThreadClosed` instead of treating a timeout as success.

### P011 — Enabled skills as slash commands

**Intent:** Make the skills enabled for the current thread, account, and working directory directly discoverable and runnable. Skills appear as `/name` or `/namespace:name`; deterministic collision handling covers built-ins, service tiers, and duplicate names, while generation fencing rejects stale skill lists after an account or directory change.

## Apply locally

The target repository must already have a Git commit identity configured because the applier uses `git am`.

```bash
git init upstream-codex
git -C upstream-codex remote add origin https://github.com/openai/codex.git
git -C upstream-codex fetch --depth=1 origin 3ba0f711642a888aec92a611a3f3b2211157ff89
git -C upstream-codex checkout --detach FETCH_HEAD
./custom-patches/apply-series.sh upstream-codex
```

## Build and artifacts

The [`Build custom Codex for macOS arm64`](.github/workflows/build-custom-macos-arm64.yml) GitHub Actions workflow is manual-only. On a standard `macos-15` runner it reapplies the series and builds these release binaries:

- `codex`
- `codex-app-server`

Its 14-day artifact contains both stripped binaries, their SHA-256 list, and build metadata recording the upstream commit, patched tree, runner, Rust compiler, Cargo, and macOS versions. To build locally after applying the series:

```bash
cd upstream-codex/codex-rs
cargo build --release -p codex-cli --bin codex
cargo build --release -p codex-app-server --bin codex-app-server
```

## License

Upstream Codex and this patch series are distributed under the terms in [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
