# Codex CLI Custom

OpenAI Codex `rust-v0.148.0`에 멀티 계정 세션 운용, 외부 세션 상태·제어,
TUI 계정 전환, 동적 skill slash command를 추가하는 순서형 patch series다.

이 저장소는 upstream 전체를 복제하지 않는다. 빌드 시 공식
[`openai/codex`](https://github.com/openai/codex) 소스의 정확한 commit을 받은 뒤,
검증된 P001–P011 patch를 순서대로 적용한다.

> Experimental: 공식 OpenAI 배포물이 아니며, 현재 series는
> `rust-v0.148.0` 전용이다.

## 기준과 재현성

- upstream tag: `rust-v0.148.0`
- upstream commit: `3ba0f711642a888aec92a611a3f3b2211157ff89`
- patch 적용 후 tree: `fe1cec7cc8a29dedd89896c4459474fb5cf2d54e`
- manifest: `custom-patches/rust-v0.148.0/series.toml`
- 적용기: `custom-patches/apply-series.sh`

적용기는 clean worktree와 정확한 upstream commit을 요구하고, 11개 patch의 SHA-256과
최종 Git tree를 확인한다. P번호는 의존 순서이므로 일부를 건너뛰거나 순서를 바꾸는
용도가 아니다.

## Patch series

### P001 — Shared writer authority

계정별 `CODEX_HOME`이 달라도 같은 `sqlite_home`을 사용하는 세션은 하나의 writer
authority를 공유한다. 영속적인 store ID와 writer generation을 SQLite에서 관리하고,
thread writer lock의 현재 소유 상태를 추측 없이 조회할 수 있게 한다.

### P002 — Session runtime protocol

app-server v2에 session runtime snapshot, operation, strict relinquish, execution-account
switch, account slot 조회·로그인에 필요한 DTO, method, notification 계약을 추가한다.
후속 patch가 구현할 surface를 먼저 고정하는 protocol patch다.

### P003 — Multi-account registry

한 app-server process 안에 여러 account slot을 등록하고 조회할 수 있는 registry를
추가한다. 기본 계정은 기존 동작을 감싼 virtual slot으로 유지하고, 추가 계정은
slot별 private home, managed credential loading, revision-bound pagination을 사용한다.
process-global external/workload identity와 충돌하는 환경은 fail-closed 처리한다.

### P004 — Durable execution binding and history

thread와 execution account의 결합을 SQLite에 영속화하고 generation CAS로 갱신한다.
resume, fork, child/review session이 정확한 결합을 상속하며, 각 turn에는 당시 account
binding provenance가 불변 history로 남는다.

### P005 — Propagate execution account to auth consumers

model client, connector, app, plugin, MCP, extension, memory·review 경로가 process-global
credential을 다시 만들지 않고 thread/turn에 캡처된 account context를 사용하게 한다.
slot별 auth-dependent service와 cache를 분리하고, turn 도중 credential mixing을 막는다.

### P006 — Publish session runtime state

외부 orchestrator가 추측 없이 소비할 수 있는 `sessionRuntime` snapshot engine을
구현한다. loaded state, active turn, waiting reason, subscribers, writer authority,
persistence position·health, account binding, 허용 가능한 control action을 sanitized
snapshot과 revision/sequence 기반 notification으로 제공한다.

### P007 — Live account registration

app-server를 재시작하지 않고 account slot을 추가·재인증할 수 있는 login lifecycle을
구현한다. API key, browser, device-code, external refresh 흐름을 slot-scoped operation으로
관리하며, 동시 browser owner와 늦게 도착한 응답을 정확한 connection·generation으로
검증한다.

### P008 — Strict thread writer relinquish

다른 process나 계정이 thread를 안전하게 이어받을 수 있도록 strict relinquish를
추가한다. 새 turn과 제어 전환을 직렬화하고, flush → materialize → sync → recorder
shutdown이 모두 성공한 뒤에만 writer guard를 해제한다. 실패하면 기존 writer를
보존하고 원인을 stable 상태로 발행한다.

### P009 — Hot execution-account switch

app-server와 TUI를 끊지 않고 idle thread의 execution account를 전환한다. 대상 account
runtime을 먼저 준비하고 durable binding CAS가 성공한 뒤 in-memory pointer를 교체한다.
진행 중인 turn은 기존 account snapshot을 유지하고 다음 turn부터 새 account를 쓴다.

### P010 — TUI session and account controls

TUI에 account picker와 `/account`, slot-scoped `/logout`, strict shutdown/release 흐름을
연결한다. TUI 종료는 writer release와 terminal `ThreadClosed`를 모두 확인한 뒤
완료하며, timeout만으로 소유권 해제를 가정하지 않는다.

### P011 — Enabled skills as slash commands

현재 thread/account/cwd에서 활성화된 skill을 `/name` 또는 `/namespace:name` 형태의
slash command로 노출한다. builtin·service-tier·skill 충돌과 중복 이름을 결정론적으로
처리하고, account/cwd가 바뀐 뒤 도착한 오래된 skill 목록은 generation fence로 버린다.

## 로컬 적용

```bash
git init upstream-codex
git -C upstream-codex remote add origin https://github.com/openai/codex.git
git -C upstream-codex fetch --depth=1 origin 3ba0f711642a888aec92a611a3f3b2211157ff89
git -C upstream-codex checkout --detach FETCH_HEAD
git -C upstream-codex config user.name patch-applier
git -C upstream-codex config user.email patch-applier@localhost
./custom-patches/apply-series.sh upstream-codex
```

## 빌드

GitHub Actions의 `Build custom Codex for macOS arm64` workflow는 수동 실행만 허용한다.
표준 `macos-15` runner에서 patch를 새로 적용하고 다음 두 binary를 release mode로 만든다.

- `codex`
- `codex-app-server`

완료 artifact에는 두 binary, SHA-256 목록, upstream commit·patched tree·toolchain 정보를
담은 build metadata가 포함된다. 로컬에서는 patch 적용 후 다음처럼 빌드할 수 있다.

```bash
cd upstream-codex/codex-rs
cargo build --locked --release -p codex-cli --bin codex
cargo build --locked --release -p codex-app-server --bin codex-app-server
```

## License

Upstream Codex와 이 patch series는 저장소의 `LICENSE`와 `NOTICE`를 따른다.
