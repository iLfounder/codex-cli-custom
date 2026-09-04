<div align="right">
  <strong>KO</strong> | <a href="README.md">EN</a>
</div>

# Codex CLI Custom

여러 계정과 장기 실행 터미널 세션을 하나의 로컬 app-server에서 다루기 위한 OpenAI Codex 실험적 fork다.

이 fork는 계정 선택, thread 소유권, session 인계, 외부 session 제어를 명시적인 계약으로 제공한다. Credential과 로컬 경로, 외부 workflow 고유 식별자는 비공개로 유지하면서 typed Goal action과 설치 가능한 plugin command도 추가한다.

> 공식 OpenAI 배포물이 아니다. 현재 series는 upstream [`rust-v0.152.0`](https://github.com/openai/codex/releases/tag/rust-v0.152.0), commit `316795b3cf2a45e90d121d9f46499d4658b2645c`을 대상으로 한다.

## Patch series가 추가하는 기능

| Logical patch | 사용자에게 제공되는 결과 (P001–P025의 모든 기능을 유지) |
|---|---|
| U01 (P001) | Thread별 영속 writer authority를 보장하고 stale writer를 거절한다. |
| U02 (P002) | Session·account·Goal·continuity control용 versioned app-server v2 JSON·TypeScript 계약을 제공한다. |
| U03 (P003) | 하나의 app-server에서 격리된 account slot과 local lifecycle을 운용한다. |
| U04 (P004–P005) | Resume/fork/child까지 유지되는 thread-account binding과 turn 전체의 execution account 일관성을 제공한다. |
| U05 (P006–P007) | Session runtime inventory·continuity receipt와 재시작 없는 login·재인증·secondary logout을 제공한다. |
| U06 (P008–P009) | Strict writer relinquish와 thread ID를 유지하는 idle account 전환을 제공한다. |
| U07 (P010–P011) | TUI account/continuity/Goal control과 설치 가능한 plugin command·ephemeral presentation을 제공한다. |
| U08 (P012–P013) | Canonical live-session reconciliation과 fixed/automatic quota-aware rotation·ordinal repair를 제공한다. |
| U09 (P014–P015) | Read-only sibling auth runtime과 실행 전 account·credential revision capture를 제공한다. |
| U10 (P016–P017) | Sanitize된 global account catalog, health/quota projection, revisioned TokenManager 선택을 제공한다. |
| U11 (P018–P019) | Global account 격리 runtime과 credential을 변경하지 않는 TUI health/quota presentation을 제공한다. |
| U12 (P020–P021) | Session lifecycle race/interrupt 일관성과 accounting-only drift에 대한 충돌 인지형 Goal 복구를 제공한다. |
| U13 (P022–P023) | Telemetry/compaction/MCP 수렴, Codex 소유 quota failover/rotation, invocation readiness handshake를 제공한다. |
| U14 (P024) | Supervised canonical control plane, account-neutral UDS, global lifecycle API, reconnect-safe client, bounded OAuth callback을 제공한다. |
| U15 (P025 + reconciliation) | Fresh canonical thread 전 managed slot binding, resume/fork 상속, 다중 행 `FooterBox`/`FooterAdapter`, multiplexer-aware Windows terminal rendering, bounded post-ready JSONL terminal failure, Runtime Artifact v2 정합성, Windows 보안 보강을 제공한다. |
| U16 | `codex exec --json` bootstrap·`turn/start` post-ready 실패를 typed bounded code로 분류한다. |
| U17 | Credential identity를 노출하지 않고 shared owner root를 canonical default account에 binding한다. |
| U18 | Admitted-turn response stream disconnect를 outcome-unknown/no-replay `turn.failed`로 보존한다. |
| U19 | App가 수락한 runtime/inventory state를 semantic footer에 production wiring하고 기존 top-level post-ready JSONL error wire를 복원한다. |
| U20 | Windows native TUI가 SSH loopback tunnel을 통해 macOS canonical app-server에 접속하면서 remote filesystem/runtime authority를 보존하고, 연결이 끊긴 뒤에도 작업을 계속하며 `/agents`와 `/resume`으로 loaded/persisted thread에 재진입할 수 있게 한다. |
| U21 | 선언형 다중 행 footer에서 순서가 보존되는 left/right lane, terminal-safe 색상, model·effort·account·session·thread·runtime·context의 live projection을 제공한다. |

App-server는 opaque account reference와 sanitize된 session state만 노출한다. 외부 workflow role, group ID, 사용자 handle은 저장하지 않는다.
Session-runtime identity도 source 종류와 literal `<workspace>` marker만 유지하며, 로컬 파일시스템
경로나 custom workflow/source payload는 반환하지 않는다.

## 제공되는 interface

- session 목록과 상태: `sessionRuntime/list`, `sessionRuntime/changed`
- account 관리: `accountSlot/list`, `accountSlot/login/start`, `accountSlot/logout`
- global account inventory: `accountSlot/inventoryChanged`, `accountSlot/list`의 health·quota projection
- MCP startup 완료: ready, failed, cancelled server 목록을 담은 `mcpServer/startupCompleted`
- account 전환: `thread/account/switch`
- account 순환: `thread/account/rotation/read`, `thread/account/rotation/update`, `accountSlot/rateLimits/read`
- writer 반환: `thread/relinquish`
- committed clear/new continuity: `thread/start`의 transition field, `thread/transition/commit`, runtime continuity projection
- Goal state: `thread/goal/get`, `thread/goal/create`, `thread/goal/set`, `thread/goal/replace`, `thread/goal/clear`
- plugin command: `pluginCommand/list`, `pluginCommand/invoke`
- ephemeral UI output: `thread/presentation/append`
- `codex exec --json`의 context compaction: `context_compaction` item을 담은 `item.started`, `item.updated`, `item.completed` event

Patch에는 생성된 Rust, JSON Schema, TypeScript 정의가 `codex-rs/app-server-protocol/schema/` 아래에 포함된다.

## Custom footer

TUI는 upstream `tui.status_line` 계약을 유지하면서 선택 가능한 다중 행 `FooterBox`를
추가한다. `FooterAdapter`는 표시 전용 행을 공급하므로 account/plan label, session/runtime
상태, quota, rotation, debug 정보를 조합할 수 있고 credential 접근이나 I/O는 수행하지
않는다. 알 수 없는 adapter ID는 무시하며 managed account는 opaque slot ID로 표시한다.
U19는 accepted runtime의 current slot이 현재 authoritative sanitized inventory와 정확히
일치할 때만 managed account number, slot ID, health, quota를 투영한다. Epoch·revision·thread
fence를 통과하지 못한 값은 비우고, config reload는 기존 account와 official-status-line
projection을 보존하면서 live footer 설정을 갱신한다.

U21은 독립적인 left/right lane을 갖는 선언형 행을 순서대로 배치한다. 지원 변수는
`model`, `reasoning_effort`, `account_email`, `account_plan`, `account_slot`,
`account_slot_label`, `account_slot_health`, `quota`, `session_id`, `session_id_short`,
`session_name`, `handle`, `thread_id`, `thread_name`, `display_handle`, `runtime_state`,
`rotation_state`, `context_usage`다. 값이 없으면 `N/A`로 표시하고 알 수 없는 변수나 색상은
설정 오류로 처리한다. 색상은 `plain`, `dim`, `red`, `green`, `yellow`, `blue`, `magenta`,
`cyan`, `white`, `gray`만 허용한다.

```toml
[tui.footer]
enabled = true
max_rows = 3
border = "rounded"       # none, plain, rounded, double
layout = "stacked"       # stacked 또는 compact
rows = [
  { left = ["model", "reasoning_effort"], right = ["account_slot"] },
  { left = ["display_handle", "session_id_short"], right = ["account_plan"] },
]

[tui.footer.colors]
model = "cyan"
reasoning_effort = "magenta"
account_slot = "green"
```

`rows`가 없으면 기존 `adapter_ids` 설정과 출력이 그대로 유지되고, `rows`가 있으면 이를
우선한다. Footer를 끄면 native status line만 그대로 사용한다.

## `codex exec --json` terminal error

`invocation.ready`가 실제 emit된 뒤 pre-semantic bootstrap 또는 `turn/start`가 실패하면
top-level `{"type":"error","message":"..."}` event 하나만 emit하고 flush한다. Bounded
message는 `codex_exec_bootstrap_failed`, `codex_exec_turn_start_server_failed`,
`codex_exec_turn_start_transport_failed`, `codex_exec_turn_start_deserialize_failed`다.
Bootstrap/server 실패는 non-admitting이 확정되며 transport/deserialization 결과는 unknown이므로
자동 replay하지 않는다. 이는 admitted-turn response stream disconnect를
`codex_exec_response_stream_disconnected` message의 `turn.failed`로 유지하는 U18
outcome-unknown/no-replay 계약과 구분된다.

## Quota-aware 지연과 capacity

Quota-aware rotation은 TokenManager의 최신 sanitize snapshot을 읽을 뿐 provider quota를
차감하거나 account별 별도 capacity pool을 만들지 않는다. 따라서 provider가 반환하는
`server_is_overloaded`/“Selected model is at capacity”는 그 자체로 local quota debit이
아니다. 독립적인 root turn은 동시에 admission될 수 있고 in-flight reservation이 없으면
같은 snapshot을 보고 같은 account/model을 선택할 수 있어 요청이 한 곳에 집중될 수 있다.
Runtime probe는 호출자가 넘긴 후보를 동시 확인하고 directory scan을 공유하며 credential
snapshot hashing을 async executor 밖으로 옮겼지만 revision·identity 검사는 유지한다. 임의의
8개 account batch 제한은 두지 않는다. 코드 검토
범위에서는 process-wide memory leak이나 credential cross-talk의 구체 증거를 찾지 못했다.
호스트 간 속도 비교 시 model/reasoning effort, prewarm, proxy/TLS 경로, 동시성을 동일하게
맞춰야 한다.

## 적용과 build

정확한 upstream commit에만 21개의 logical patch를 적용한다.

```sh
git checkout 316795b3cf2a45e90d121d9f46499d4658b2645c
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD" rust-v0.152.0
```

Applier는 clean tree를 요구하고 각 patch digest를 검증한 뒤 U01–U21을 순서대로 적용하고
최종 Git tree를 확인한다. 과거 `rust-v0.149.0`, `rust-v0.148.0` series는 이름을 명시해
재현할 수 있다. POSIX shell, Git, `sed`, `awk`, `shasum` 또는 `sha256sum`이 필요하다.

`codex-rs`에서 로컬 build:

```sh
perl -0pi -e 's/version = "0\.0\.0"/version = "0.152.0"/g' Cargo.lock
cargo build --locked --release -p codex-cli --bin codex
cargo build --locked --release -p codex-app-server --bin codex-app-server
cargo build --locked --release -p codex-code-mode-host --bin codex-code-mode-host
cargo build --locked --release -p codex-responses-api-proxy --bin codex-responses-api-proxy
CODEX_REPO_ROOT="$(cd .. && pwd)" python3 ../scripts/build_codex_package.py \
  --target aarch64-apple-darwin --variant codex --package-version 0.152.0 \
  --entrypoint-bin target/release/codex \
  --code-mode-host-bin target/release/codex-code-mode-host
CODEX_REPO_ROOT="$(cd .. && pwd)" python3 ../scripts/build_codex_package.py \
  --target aarch64-apple-darwin --variant codex-app-server --package-version 0.152.0 \
  --entrypoint-bin target/release/codex-app-server \
  --code-mode-host-bin target/release/codex-code-mode-host
```

같은 patch source는 host가 달라도 공통으로 사용한다. Windows에서는 MSVC target으로 local
CLI/core check 또는 release build를 수행한다(예: `cargo check -p codex-core --target
x86_64-pc-windows-msvc --locked`, `cargo build -p codex-cli --release --target
x86_64-pc-windows-msvc`). 배포용 macOS arm64 package는 저장소의 GitHub Actions workflow가
검증된 상태로 생성하므로 Mac에서 별도 build할 필요가 없다.

Managed app-server daemon/supervisor는 crate 문서에 명시된 대로 Unix 전용이다. 이는 patch
set에서 기능을 뺀 것이 아니라 daemon process manager의 platform 경계이며, Windows build는
공유 account·protocol·core·TUI·direct transport 경로를 컴파일하고 daemon lifecycle 명령에는
명확한 unsupported-platform 오류를 반환한다.

upstream release tag의 `Cargo.lock`에는 workspace package version이 `0.0.0` placeholder로
남아 있다. 첫 명령은 GitHub Actions build와 동일하게 이 정확한 placeholder만 `0.152.0`으로
정규화한 뒤 Cargo가 locked dependency resolution을 수행하게 한다.
Package builder는 Python 3.10 이상이 필요하다. 또한 upstream source에 고정된 target별
ripgrep과 patched zsh resource를 다운로드하고 digest를 검증한다.
Code Mode host build에는 동일 version의 Codex 배포 V8 archive와 generated binding을
사용하며, workflow가 release checksum manifest와 대조하고 두 digest를 기록한다.

수동 GitHub Actions workflow는 표준 macOS arm64 runner에서 다음 산출물을 만든다.

- `codex-package-aarch64-apple-darwin.tar.gz`: TUI/CLI, 같은 source의 Code Mode host,
  ripgrep, patched zsh, package metadata
- `codex-app-server-package-aarch64-apple-darwin.tar.gz`: app-server, 같은 Code Mode
  host, ripgrep, patched zsh, package metadata
- `codex-responses-api-proxy`: TUI와 app-server runtime에는 필요하지 않은 선택형
  standalone proxy
- `SHA256SUMS`
- `BUILD-METADATA.txt`
- `LICENSE`
- `NOTICE`

Workflow는 두 package layout을 검사하고, 양쪽에 동일한 build의 Code Mode host가
들어갔는지 비교한 뒤 SHA-256 digest와 source-tree provenance를 기록해 업로드한다.

## 기존 custom state store 업그레이드

같은 store를 공유하는 구버전 TUI와 app-server를 모두 종료한다. 0.152 build를 `CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1`로 한 번만 시작한 뒤 변수를 제거한다. Migration은 알려진 legacy schema를 검증한 후에만 적용하며 unknown 또는 partial schema는 거절한다. Migration이 끝난 store를 구버전 binary로 다시 열지 않는다.

## 저장소 구성

- `custom-patches/rust-v0.152.0/`: 현재 21개 logical patch와 digest manifest
- `custom-patches/rust-v0.151.0/`: 재현성을 위해 보존한 이전 15개 patch series
- `custom-patches/rust-v0.149.0/`: 재현성을 위해 보존한 이전 series
- `custom-patches/rust-v0.148.0/`: 재현성을 위해 보존한 이전 series
- `custom-patches/apply-series.sh`: clean-tree patch applier
- `.github/workflows/build-custom-macos-arm64.yml`: 수동 macOS arm64 build
- `.github/workflows/build-custom-windows-x64.yml`: 수동 Windows x64 build

## License

Upstream Codex와 이 patch series는 [`LICENSE`](LICENSE)와 [`NOTICE`](NOTICE)의 조건에 따라 배포된다.
