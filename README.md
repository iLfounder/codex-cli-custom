<div align="right">
  <a href="README.ko.md">KO</a> | <strong>EN</strong>
</div>

# Codex CLI Custom

An experimental fork of OpenAI Codex for people who keep several accounts and long-running terminal sessions in one local app-server.

The fork makes account selection, thread ownership, session handoff, and external session control explicit. It also adds typed Goal actions and installable plugin commands while keeping credentials, local paths, and workflow-specific identities private.

> This is an unofficial distribution. The current series targets upstream [`rust-v0.149.1`](https://github.com/openai/codex/releases/tag/rust-v0.149.1), commit `ff29a44391deccde0aba0f8390337d7f3c319ea4`.

Upstream 0.149.1 adds explicit source labels for new `codex exec` threads, identifies detached
memory consolidation work, and provides opt-in image-aware remote compaction budgeting. The custom
session, account, Goal, continuity, and plugin contracts remain unchanged from the 0.149.0 series.

## What the patch series adds

| Patch | User-facing result |
|---|---|
| P001 | One durable writer authority per thread; stale writers are rejected. |
| P002 | Versioned app-server v2 JSON and TypeScript contracts for session, account, Goal, and continuity controls. |
| P003 | Multiple isolated account slots in one app-server. |
| P004 | Durable thread-to-account binding across resume, fork, and child threads, plus versioned Goal state. |
| P005 | The same execution account is used by the model, MCP, apps, plugins, hooks, and telemetry for a turn. |
| P006 | `sessionRuntime/list`, runtime change events, allowed actions, and committed `/clear` and `/new` continuity receipts. |
| P007 | Account login, reauthentication, and secondary-account logout without restarting the app-server. |
| P008 | Strict `thread/relinquish` with explicit `released` or `failed` terminal results. |
| P009 | Idle-thread account switching while keeping the same thread ID. |
| P010 | TUI `/account`, `/logout`, `/exit`, `/clear`, `/new`, and `/goal` controls, including typed agent-requested clear/new. |
| P011 | Installable `/namespace:name` plugin commands and ephemeral card, notice, and progress presentation. |
| P012 | Accurate live-turn projection: completed turns no longer keep idle sessions falsely marked active. |

The app-server exposes opaque account references and sanitized session state. It never stores external workflow roles, group IDs, or user handles.

## Interfaces produced

- session inventory and state: `sessionRuntime/list`, `sessionRuntime/changed`
- account management: `accountSlot/list`, `accountSlot/login/start`, `accountSlot/logout`
- account switching: `thread/account/switch`
- writer release: `thread/relinquish`
- committed clear/new continuity: transition fields on `thread/start`, `thread/transition/commit`, and runtime continuity projections
- Goal state: `thread/goal/get`, `thread/goal/create`, `thread/goal/set`, `thread/goal/replace`, `thread/goal/clear`
- plugin commands: `pluginCommand/list`, `pluginCommand/invoke`
- ephemeral UI output: `thread/presentation/append`

Generated Rust, JSON Schema, and TypeScript definitions are included by the patches under `codex-rs/app-server-protocol/schema/`.

## Apply and build

Apply the twelve patches only to the exact upstream commit:

```sh
git checkout ff29a44391deccde0aba0f8390337d7f3c319ea4
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD"
```

The applier requires a clean tree, verifies every patch digest, applies P001–P012 in order, and verifies the final Git tree. It needs a POSIX shell, Git, `sed`, `awk`, and either `shasum` or `sha256sum`.

Build locally from `codex-rs`:

```sh
perl -0pi -e 's/version = "0\.0\.0"/version = "0.149.1"/g' Cargo.lock
cargo build --locked --release -p codex-cli --bin codex
cargo build --locked --release -p codex-app-server --bin codex-app-server
cargo build --locked --release -p codex-code-mode-host --bin codex-code-mode-host
cargo build --locked --release -p codex-responses-api-proxy --bin codex-responses-api-proxy
CODEX_REPO_ROOT="$(cd .. && pwd)" python3 ../scripts/build_codex_package.py \
  --target aarch64-apple-darwin --variant codex --package-version 0.149.1 \
  --entrypoint-bin target/release/codex \
  --code-mode-host-bin target/release/codex-code-mode-host
CODEX_REPO_ROOT="$(cd .. && pwd)" python3 ../scripts/build_codex_package.py \
  --target aarch64-apple-darwin --variant codex-app-server --package-version 0.149.1 \
  --entrypoint-bin target/release/codex-app-server \
  --code-mode-host-bin target/release/codex-code-mode-host
```

The tagged upstream source keeps workspace-package versions as `0.0.0` placeholders in
`Cargo.lock`. The first command normalizes only those exact placeholders to `0.149.1`, matching the GitHub
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

## Upgrading a 0.148 custom state store

Stop every older TUI and app-server sharing the store. Start the 0.149.x build once with `CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1`, then remove the variable. The migration validates the known legacy schema before adoption and rejects unknown or partial schemas. Do not reopen the migrated store with an older binary.

## Repository layout

- `custom-patches/rust-v0.149.1/`: current manifest reusing P001–P011 from 0.149.0 and adding the 0.149.1 live-turn projection fix
- `custom-patches/rust-v0.149.0/`: previous series retained for reproducibility
- `custom-patches/rust-v0.148.0/`: previous series retained for reproducibility
- `custom-patches/apply-series.sh`: clean-tree patch applier
- `.github/workflows/build-custom-macos-arm64.yml`: manual macOS arm64 build

## License

Upstream Codex and this patch series are distributed under [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
