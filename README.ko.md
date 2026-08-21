<div align="right">
  <strong>KO</strong> | <a href="README.md">EN</a>
</div>

# Codex CLI Custom

## 이 패치 시리즈를 만든 이유

하나의 장기 실행 app-server와 TUI에서 여러 계정을 운용하면 process 단위 identity만으로는 세션의 실행 계정과 writer 소유권을 안전하게 구분할 수 없다. 각 세션에는 명시적인 실행 계정과 영속적인 writer 소유권이 필요하고, 다른 process가 이어받을 때도 추측이 아니라 검증된 handoff가 필요하다. 외부 orchestrator 역시 파일, process 생명주기, timing을 추정하는 대신 세션의 정확한 상태와 허용된 control을 읽을 수 있어야 한다.

이 순서형 patch series는 그 경계를 명시한다. 한 app-server/TUI 안에서 여러 account slot을 운용하고, 서로 다른 workflow 역할을 맡는 session이 올바른 account context를 유지하도록 session·turn별 auth를 격리하며, 관측 가능한 session state와 보호된 control operation을 제공하고, 활성화된 skill을 slash command로 노출한다.

이 저장소는 upstream 전체를 복제하지 않는다. [`openai/codex`](https://github.com/openai/codex)의 정확한 commit을 받은 뒤 검증된 P001–P011 series를 순서대로 적용한다.

> **Experimental:** 공식 OpenAI 배포물이 아니며, 현재 series는 `rust-v0.148.0`에만 적용된다.

## 기준과 재현성

- upstream tag: `rust-v0.148.0`
- upstream commit: `3ba0f711642a888aec92a611a3f3b2211157ff89`
- 전체 patch 적용 후 tree: `fe1cec7cc8a29dedd89896c4459474fb5cf2d54e`
- manifest: [`custom-patches/rust-v0.148.0/series.toml`](custom-patches/rust-v0.148.0/series.toml)
- 적용기: [`custom-patches/apply-series.sh`](custom-patches/apply-series.sh)

적용기는 clean worktree와 정확한 upstream commit을 요구하고, 각 patch의 SHA-256과 최종 Git tree를 검증한다. P번호는 순서가 고정된 의존 chain이므로 일부를 건너뛰거나 순서를 바꾸는 선택지가 아니다.

## Patch series

### P001 — Shared writer authority

**취지:** 서로 다른 account home이 같은 session store를 사용해도 소유권을 모호하지 않게 만든다. 영속적인 store ID와 writer generation을 SQLite에 보관하고, thread writer의 현재 소유 상태를 mutation 없이 조회한다.

### P002 — Session runtime protocol

**취지:** runtime 동작을 구현하기 전에 client가 의존할 안정적인 contract를 먼저 고정한다. app-server v2에 runtime snapshot·operation, strict relinquish, execution-account switch, account slot 조회·login을 위한 DTO, method, notification을 추가한다.

### P003 — Multi-account registry

**취지:** 하나의 app-server 안에서 여러 account를 identity 혼합 없이 사용할 수 있게 한다. 기본 account는 호환성을 위한 virtual slot으로 유지하고, 추가 slot은 private home, managed credential loading, revision-bound pagination과 process 전역 identity 충돌에 대한 fail-closed 검사를 사용한다.

### P004 — Durable execution binding and history

**취지:** thread가 이전 작업을 소유했던 동일한 execution account로 다시 시작되도록 한다. thread-account binding을 영속화하고 generation CAS로 갱신하며, resume·fork·child·review session이 binding을 상속하고 각 turn은 불변의 binding provenance를 기록한다.

### P005 — Propagate execution account to auth consumers

**취지:** credential과 account-scoped state가 session 경계를 넘어 섞이지 않게 한다. model, connector, app, plugin, MCP, extension, memory, review 경로는 thread 또는 turn에 capture된 account context와 account별 service·cache를 사용한다.

### P006 — Publish session runtime state

**취지:** 외부 controller가 추측 없이 session을 관측하고 제어하게 한다. sanitized `sessionRuntime` snapshot은 lifecycle·waiting state, subscriber, writer authority, persistence health·position, account binding, 현재 허용된 action을 revisioned snapshot과 sequenced notification으로 제공한다.

### P007 — Live account registration

**취지:** app-server를 재시작하지 않고 account slot을 추가하거나 재인증하게 한다. API key, browser, device-code, external-refresh login을 slot-scoped operation으로 실행하고, connection·generation 검사로 browser ownership과 늦게 도착한 응답을 보호한다.

### P008 — Strict thread writer relinquish

**취지:** 다른 owner가 이어받아도 안전할 만큼 state가 영속화된 뒤에만 session을 release한다. 새 turn과 control transition을 직렬화하고 flush, materialization, sync, recorder shutdown이 모두 성공해야 writer guard를 해제하며, 실패 시 소유권을 보존하고 stable cause를 발행한다.

### P009 — Hot execution-account switch

**취지:** app-server나 TUI 연결을 끊지 않고 idle thread의 account를 전환한다. target runtime을 먼저 준비한 뒤 durable binding CAS로 in-memory pointer를 갱신하며, 진행 중인 turn은 capture된 account를 유지하고 다음 turn부터 새 account를 사용한다.

### P010 — TUI session and account controls

**취지:** multi-account session control을 terminal에서 직접 사용할 수 있게 한다. TUI에 account picker, `/account`, slot-scoped `/logout`, strict shutdown/release를 연결하고, timeout을 성공으로 간주하지 않고 writer release와 terminal `ThreadClosed`를 모두 기다린다.

### P011 — Enabled skills as slash commands

**취지:** 현재 thread·account·working directory에서 활성화된 skill을 바로 찾고 실행하게 한다. skill은 `/name` 또는 `/namespace:name`으로 나타나며 builtin·service tier·중복 이름 충돌을 결정론적으로 처리하고, account나 directory 변경 후 도착한 오래된 목록은 generation fence로 버린다.

## 로컬 적용

적용기는 `git am`을 사용하므로 target repository에 Git commit identity가 미리 설정되어 있어야 한다.

```bash
git init upstream-codex
git -C upstream-codex remote add origin https://github.com/openai/codex.git
git -C upstream-codex fetch --depth=1 origin 3ba0f711642a888aec92a611a3f3b2211157ff89
git -C upstream-codex checkout --detach FETCH_HEAD
./custom-patches/apply-series.sh upstream-codex
```

## 빌드와 artifact

GitHub Actions의 [`Build custom Codex for macOS arm64`](.github/workflows/build-custom-macos-arm64.yml) workflow는 수동 실행만 허용한다. 표준 `macos-15` runner에서 series를 다시 적용하고 다음 release binary를 빌드한다.

- `codex`
- `codex-app-server`

14일간 보관되는 artifact에는 strip된 두 binary, SHA-256 목록, upstream commit·patched tree·runner·Rust compiler·Cargo·macOS version을 기록한 build metadata가 포함된다. 로컬에서는 series 적용 후 다음처럼 빌드할 수 있다.

```bash
cd upstream-codex/codex-rs
cargo build --release -p codex-cli --bin codex
cargo build --release -p codex-app-server --bin codex-app-server
```

## License

Upstream Codex와 이 patch series는 [`LICENSE`](LICENSE)와 [`NOTICE`](NOTICE)의 조건에 따라 배포된다.
