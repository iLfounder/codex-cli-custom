<div align="right">
  <a href="README.ko.md">KO</a> | <strong>EN</strong>
</div>

# Codex CLI Custom

An experimental fork of OpenAI Codex for people who keep several accounts and long-running terminal sessions in one local app-server.

The fork makes account selection, thread ownership, session handoff, and external session control explicit. It also adds typed Goal actions and installable plugin commands while keeping credentials, local paths, and workflow-specific identities private.

> This is an unofficial distribution. The current series targets upstream [`rust-v0.151.0`](https://github.com/openai/codex/releases/tag/rust-v0.151.0), commit `78c290807ce710180111df227df3b7a4fe845452`.

## What the patch series adds

| Logical patch | User-facing result (the complete P001–P025 feature set is retained) |
|---|---|
| U01 (P001) | Durable per-thread writer authority; stale writers are rejected. |
| U02 (P002) | Versioned app-server v2 JSON/TypeScript contracts for session, account, Goal, and continuity controls. |
| U03 (P003) | Isolated account slots in one app-server, with local account lifecycle operations. |
| U04 (P004–P005) | Durable thread/account binding and one execution account shared by model, MCP, apps, plugins, hooks, and telemetry. |
| U05 (P006–P007) | Session-runtime inventory and continuity receipts, plus login, reauthentication, and secondary-account logout without restart. |
| U06 (P008–P009) | Strict writer relinquish and idle-thread account switching while preserving thread identity. |
| U07 (P010–P011) | TUI account/continuity/Goal controls and installable plugin commands with ephemeral presentation. |
| U08 (P012–P013) | Canonical live-session account reconciliation and fixed/automatic quota-aware account rotation with ordinal repair. |
| U09 (P014–P015) | Read-only sibling authentication runtimes and exact per-turn account/credential-revision capture. |
| U10 (P016–P017) | Sanitized global account catalog with health/quota projections and revisioned TokenManager selection. |
| U11 (P018–P019) | Isolated global execution runtimes and TUI account/rotation presentation without local credential mutation. |
| U12 (P020–P021) | Session lifecycle race/interrupt consistency and conflict-aware Goal recovery across accounting-only drift. |
| U13 (P022–P023) | Final telemetry/compaction and MCP convergence, Codex-owned quota failover/rotation, and invocation readiness handshake. |
| U14 (P024) | Supervised canonical control plane, account-neutral local UDS, global lifecycle APIs, reconnect-safe clients, and bounded OAuth callback compatibility. |
| U15 (P025 + reconciliation) | Managed-slot binding before fresh canonical threads, inherited resume/fork bindings, multi-row `FooterBox`/`FooterAdapter`, generated-contract reconciliation, and Windows security hardening. |

The app-server exposes opaque account references and sanitized session state. It never stores external workflow roles, group IDs, or user handles.
Session-runtime identity keeps only a source kind and the literal `<workspace>` marker; it does not
return local filesystem paths or custom workflow/source payloads.

## Interfaces produced

- session inventory and state: `sessionRuntime/list`, `sessionRuntime/changed`
- account management: `accountSlot/list`, `accountSlot/login/start`, `accountSlot/logout`
- global account inventory: `accountSlot/inventoryChanged`, plus health and quota projections in `accountSlot/list`
- MCP startup completion: `mcpServer/startupCompleted` with ready, failed, and cancelled server lists
- account switching: `thread/account/switch`
- account rotation: `thread/account/rotation/read`, `thread/account/rotation/update`, `accountSlot/rateLimits/read`
- writer release: `thread/relinquish`
- committed clear/new continuity: transition fields on `thread/start`, `thread/transition/commit`, and runtime continuity projections
- Goal state: `thread/goal/get`, `thread/goal/create`, `thread/goal/set`, `thread/goal/replace`, `thread/goal/clear`
- plugin commands: `pluginCommand/list`, `pluginCommand/invoke`
- ephemeral UI output: `thread/presentation/append`
- context compaction in `codex exec --json`: `item.started`, `item.updated`, and `item.completed` events with a `context_compaction` item

Generated Rust, JSON Schema, and TypeScript definitions are included by the patches under `codex-rs/app-server-protocol/schema/`.

## Custom footer

The TUI keeps the upstream `tui.status_line` contract and adds an optional multi-row
`FooterBox`. `FooterAdapter` instances supply display-only rows, so account/plan labels,
session/runtime state, quota, rotation, and debug fields can be composed without giving the
footer access to credentials or performing I/O. Unknown adapter IDs are ignored and managed
accounts are represented by opaque slot identifiers.

Example configuration:

```toml
[tui.footer]
enabled = true
max_rows = 3
border = "rounded"       # none, plain, rounded, or double
layout = "stacked"       # stacked or compact
adapter_ids = ["official-statusline", "account", "session", "quota"]
```

`max_rows` may be increased for additional rows; a disabled footer leaves the native status
line unchanged.

## Quota-aware latency and capacity

Quota-aware rotation reads TokenManager's latest sanitized snapshots; it does not decrement a
provider quota or create a separate per-account capacity pool. The provider's
`server_is_overloaded`/“Selected model is at capacity” response is therefore not itself a local
quota debit. Independent root turns can still be admitted concurrently and choose the same
account/model from the same snapshot when no in-flight reservation exists, which can concentrate
requests. Runtime probes for the caller-supplied candidates run concurrently, share one directory
scan, and move credential snapshot hashing off the async executor while retaining revision and
identity checks; there is no arbitrary eight-account batch cap.
The code review found no concrete process-wide memory leak or credential cross-talk; compare the
same model/reasoning effort, prewarm setting, proxy/TLS path, and concurrency when comparing hosts.

## Apply and build

Apply the fifteen logical patches only to the exact upstream commit:

```sh
git checkout 78c290807ce710180111df227df3b7a4fe845452
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD" rust-v0.151.0
```

The applier requires a clean tree, verifies every patch digest, applies U01–U15 in order, and
verifies the final Git tree. The historical `rust-v0.149.0` and `rust-v0.148.0` series remain
available by passing their series name explicitly. The script needs a POSIX shell, Git, `sed`,
`awk`, and either `shasum` or `sha256sum`.

Build locally from `codex-rs`:

```sh
perl -0pi -e 's/version = "0\.0\.0"/version = "0.151.0"/g' Cargo.lock
cargo build --locked --release -p codex-cli --bin codex
cargo build --locked --release -p codex-app-server --bin codex-app-server
cargo build --locked --release -p codex-code-mode-host --bin codex-code-mode-host
cargo build --locked --release -p codex-responses-api-proxy --bin codex-responses-api-proxy
CODEX_REPO_ROOT="$(cd .. && pwd)" python3 ../scripts/build_codex_package.py \
  --target aarch64-apple-darwin --variant codex --package-version 0.151.0 \
  --entrypoint-bin target/release/codex \
  --code-mode-host-bin target/release/codex-code-mode-host
CODEX_REPO_ROOT="$(cd .. && pwd)" python3 ../scripts/build_codex_package.py \
  --target aarch64-apple-darwin --variant codex-app-server --package-version 0.151.0 \
  --entrypoint-bin target/release/codex-app-server \
  --code-mode-host-bin target/release/codex-code-mode-host
```

The same patched source is portable across hosts. On Windows, use the MSVC target for the
local CLI/core check or release build (for example, `cargo check -p codex-core --target
x86_64-pc-windows-msvc --locked` and `cargo build -p codex-cli --release --target
x86_64-pc-windows-msvc`). The distributable macOS arm64 packages are produced by the checked-in
GitHub Actions workflow; no local Mac build is required.

The managed app-server daemon/supervisor remains explicitly Unix-only (as documented by its
crate); this is a platform boundary of the daemon process manager, not a removal from the patch
set. Windows builds still compile the shared account, protocol, core, TUI, and direct transport
paths and return a clear unsupported-platform error for daemon lifecycle commands.

The tagged upstream source keeps workspace-package versions as `0.0.0` placeholders in
`Cargo.lock`. The first command normalizes only those exact placeholders, matching the GitHub
Actions build, before Cargo performs locked dependency resolution.
The package builder requires Python 3.10 or newer. It also fetches and verifies the
target-specific ripgrep and patched zsh resources defined by the upstream source.
The Code Mode host build uses the matching Codex-published V8 archive and generated binding;
the workflow verifies both against their release checksum manifest and records their digests.

The manual GitHub Actions workflow uses a standard macOS arm64 runner and produces:

- `codex-package-aarch64-apple-darwin.tar.gz`: the TUI/CLI, its matching Code Mode host,
  ripgrep, patched zsh, and package metadata
- `codex-app-server-package-aarch64-apple-darwin.tar.gz`: app-server, the same matching
  Code Mode host, ripgrep, patched zsh, and package metadata
- `codex-responses-api-proxy`: an optional standalone proxy; it is not required by the TUI
  or app-server at runtime
- `SHA256SUMS`
- `BUILD-METADATA.txt`
- `LICENSE`
- `NOTICE`

The workflow checks both package layouts, verifies that they contain the exact same built
Code Mode host, and records SHA-256 digests and source-tree provenance before upload.

## Upgrading an older custom state store

Stop every older TUI and app-server sharing the store. Start the 0.151 build once with `CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1`, then remove the variable. The migration validates the known legacy schema before adoption and rejects unknown or partial schemas. Do not reopen the migrated store with an older binary.

## Repository layout

- `custom-patches/rust-v0.151.0/`: current fifteen-patch ordered series and digest manifest
- `custom-patches/rust-v0.149.0/`: preserved historical ordered series
- `custom-patches/rust-v0.148.0/`: previous series retained for reproducibility
- `custom-patches/apply-series.sh`: clean-tree patch applier
- `.github/workflows/build-custom-macos-arm64.yml`: manual macOS arm64 build

## License

Upstream Codex and this patch series are distributed under [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
