<div align="right">
  <strong>KO</strong> | <a href="README.md">EN</a>
</div>

# Codex CLI Custom

## 이 fork를 만든 이유

Stock Codex는 인증과 runtime 소유권의 많은 부분을 process 단위로 다룬다. 하나의 장기 실행 app-server와 TUI가 여러 계정을 처리하고, 각 thread를 올바른 계정에 결속하며, 명시적으로 닫힌 thread를 다른 process가 history 손상 없이 이어받아야 할 때는 이 경계만으로 부족하다.

이 fork는 암묵적인 경계를 명시적인 contract로 바꾼다.

- 하나의 app-server 안에서 격리된 여러 account slot을 운용한다.
- 각 turn은 불변의 execution account를 사용하고, account 전환은 보호된 next-turn 동작으로 수행한다.
- single-writer authority와 strict handoff를 영속적으로 관리한다.
- 외부 consumer가 사용할 수 있도록 sanitize된 session identity, lifecycle, persistence, allowed-control state를 제공한다.
- account별 앱을 빠져나와 다른 앱에 붙지 않고도 TUI에서 account와 session을 제어한다.
- 설치 가능한 structured plugin command와 bounded UI-only presentation component를 제공한다.

목표는 workflow나 relay system을 대체하는 것이 아니다. PID, socket, title, cwd, timeout을 추측하지 않고도 외부 system이 app-server의 정본 상태와 안전한 control을 소비하게 하는 것이다.

> **Experimental:** 공식 OpenAI 배포물이 아니다.

## 0.149 공개 준비 상태

대상 upstream은 [`rust-v0.149.0`](https://github.com/openai/codex/releases/tag/rust-v0.149.0), commit `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`이다.

| 영역 | 현재 상태 |
|---|---|
| P001–P011 구현과 focused check | 완료 |
| 순서형 0.149 patch export와 clean-apply 검증 | 완료; 11개 patch가 tree `e5915796b021e81b88bd40406b62fa6c3bf89e76`를 재현함 |
| macOS arm64 release build와 artifact | 완료; [run 32517743230](https://github.com/iLfounder/codex-cli-custom/actions/runs/32517743230), artifact `9462237252`, digest `sha256:4845802188c3f2a1bf3f030d1dcd8409d47278198015a205ceb55b9864ea0f42` |
| 최종 독립 review | build된 final tree를 대상으로 진행 중 |
| 0.149 publication | Pending |

현재 candidate는 [`custom-patches/rust-v0.149.0`](custom-patches/rust-v0.149.0/)이다. [`custom-patches/rust-v0.148.0`](custom-patches/rust-v0.148.0/)은 이전 release series로만 보존한다.

## P001–P011의 의도된 경계

각 번호는 독립적으로 검토 가능한 기능 patch다. 뒤 patch가 앞 contract를 소비하므로 적용 순서는 고정된다. 번호 분리는 향후 upstream update를 하나의 큰 fork merge가 아니라 기능별 port로 제한하는 유지보수 전략이기도 하다.

### P001 — Shared writer authority

**이유:** 서로 다른 account home이 하나의 session과 SQLite store를 공유하면 account-local lock file만으로 중복 writer를 막을 수 없다.

**경계:** shared SQLite root에 영속적인 `storeId`와 단조 증가하는 `writerGeneration` authority를 두고 advisory process lock을 유지하며, stale owner는 mutation 전에 거절한다. writer 자동 강탈은 하지 않는다.

### P002 — Session runtime protocol

**이유:** TUI, relay adapter, 외부 orchestrator가 control 구현보다 먼저 의존할 안정적인 app-server v2 contract가 필요하다.

**경계:** bounded `sessionRuntime`, account-slot, login, relinquish, switch DTO·method·notification·pagination과 compile-safe stub을 정의한다. 실제 control 동작은 구현하지 않는다.

### P003 — Multi-account registry

**이유:** 하나의 server가 credential path를 노출하거나 process-default identity로 조용히 fallback하지 않고 여러 account를 보유해야 한다.

**경계:** host-managed account-slot manifest, slot별 private auth home과 model cache, 호환 default slot, revision-bound listing을 제공한다. 지원하지 않는 process-global external/workload identity에서는 fail closed 한다.

### P004 — Durable execution binding and history

**이유:** resume·fork·child·review thread는 작업을 소유한 account로 계속 실행돼야 한다.

**경계:** thread-slot binding을 generation CAS와 함께 영속화하고 thread 생성 경로에 상속하며, 각 turn에 불변 provenance를 기록한다. slot ID만으로 credential identity가 최신이라고 판단하지 않는다.

### P005 — 모든 consumer에 account 전파

**이유:** model client만 바꿔도 connector, app, plugin, MCP, telemetry, memory, review, cost polling이 default 또는 stale credential을 사용하면 격리가 깨진다.

**경계:** turn마다 하나의 account runtime을 capture하고 account별 service·cache를 포함한 모든 credential-sensitive consumer에 전파한다. mid-turn credential mixing은 허용하지 않는다.

### P006 — 외부에 보이는 session runtime

**이유:** 외부 관리자는 session이 무엇을 하고 있고 다음에 어떤 동작이 안전한지 추측 없이 알아야 한다.

**경계:** stable identity, lifecycle·waiting state, subscriber, writer authority, persistence health·position, account binding, 현재 허용된 action을 sanitize된 revisioned snapshot과 sequenced notification으로 제공한다. operation replay는 bounded하며 credential path나 secret을 노출하지 않는다.

### P007 — 무중단 account 등록

**이유:** account 추가와 재인증 때문에 app-server를 재시작하거나 모든 TUI를 끊어서는 안 된다.

**경계:** slot-scoped API-key, browser, device-code, external-refresh login operation과 secondary-slot logout을 제공한다. exact connection ownership과 generation CAS로 이미 교체된 늦은 OAuth·same-slot completion을 거절한다.

### P008 — Strict writer relinquish

**이유:** TUI가 닫혔다는 사실만으로 writer가 flush·materialize를 끝내고 다른 account/process에 thread를 반환했다고 볼 수 없다.

**경계:** 새 작업과 close transition을 직렬화하고 flush, materialization, path sync, recorder shutdown, exact-generation release가 모두 성공해야 반환한다. 실패하면 기존 owner를 유지하며, admission을 다시 열기 전에 terminal `NotLoaded`, `Released`, matching `ThreadClosed`를 발행한다.

### P009 — 무중단 execution-account 전환

**이유:** attach된 idle thread가 TUI를 떠나거나 account별 app-server에 다시 연결하지 않고 owner account를 바꿀 수 있어야 한다.

**경계:** complete target runtime을 먼저 준비한 뒤 durable binding CAS와 실패하지 않는 pointer publish를 수행한다. 진행 중인 turn은 capture한 account를 유지하고 다음 turn부터 target을 사용한다. same-slot 재인증을 포함해 MCP, plugin, realtime, telemetry/network provenance, Guardian sampler, Goal runtime 등 장기 account-bound consumer를 rebuild 또는 refresh한다.

**상태:** P009에서 구현 완료.

### P010 — TUI account, exit, clear, new-thread control

**이유:** timeout이나 disconnect를 성공으로 오인하지 않으면서 terminal에서 직접 safety contract를 사용할 수 있어야 한다.

**경계:** account picker와 account/logout control을 추가하고, explicit exit는 strict terminal release를 기다린다. typed `threadClear`/`threadNew` agent control을 신규 thread와 legacy-resumed thread 모두에 제공한다. clear/new는 먼저 응답하고 exact successful completion event 뒤에만 UI를 전환한다.

**상태:** P010에서 구현 완료.

### P011 — 설치 가능한 structured plugin command와 ephemeral presentation

**이유:** skill을 text slash command로 노출하는 것만으로는 충분한 component model이 아니다. Plugin은 model transcript에 control data를 넣지 않으면서 typed action과 relay-friendly UI element를 제공해야 한다.

**경계:** legacy plugin command path를 보존하면서 normalized contribution overlay를 추가한다. Command의 canonical 이름은 `/namespace:name`이며 `/name`은 유일할 때만 허용한다. Target은 bounded prompt, exact MCP tool, goal get/set/clear 같은 allowlisted Rust app-server action, 또는 fixed argv·no shell·기존 approval/sandbox를 사용하는 packaged executable로 제한한다.

Plugin은 exact thread의 현재 subscriber와 TUI에 bounded card, notice, progress item을 append할 수 있다. 이 item은 ephemeral이며 rollout history, model context, durable conversation history에 들어가지 않는다. `llc-relay`가 routing과 message-job authority로 계속 남는다.

**상태:** P011에서 구현 완료.

## Runtime과 Relay의 경계

Custom app-server는 account execution, thread writer ownership, persistence state, safe control admission의 authority다. 외부 system은 이 값을 소비하고 session을 workflow role과 연결할 수 있지만, 이 fork가 relay job을 workflow state나 responsibility assignment와 동일한 entity로 만들지는 않는다.

`llc-relay`는 Codex와 Claude session 사이의 message transport를 계속 담당한다. Plugin card/notice/progress는 현재 subscriber를 위한 typed presentation surface일 뿐, transport·acknowledgement ledger·Agent 작업 완료 증명이 아니다.

## Packaging과 update

0.149 candidate는 digest manifest와 clean-tree applier를 포함한 열한 개의 ordered exact-base patch다. 정확한 upstream commit에만 적용한다.

```sh
git checkout 758ef40f50c1a458425c7cfbf1eb12cbc07af0b0
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD"
```

Applier는 dirty 또는 잘못된 base worktree를 거절하고, 각 patch digest를 검증하며, P001–P011을 순서대로 적용한 뒤 final tree `e5915796b021e81b88bd40406b62fa6c3bf89e76`를 요구한다.

이 분리가 유지보수 방식이다. upstream이 갱신되면 각 P번호를 자기 기능 경계 안에서 inspect·adapt·verify할 수 있다.

### 기존 0.148 custom state store 업그레이드

이전 custom series가 사용한 migration 번호를 official 0.149가 나중에 사용했다. 따라서 새 binary는 legacy row를 발견하면 DB를 바꾸지 않고 중단한다. 같은 state store를 공유하는 모든 구버전 TUI와 app-server를 먼저 종료한 뒤, 0.149 build를 `CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1`로 한 번만 시작한다. atomic adoption 전에 exact checksum, table definition, custom migration metadata를 다시 검증하며 unknown 또는 partial schema는 계속 fail-closed다. 일회성 cutover가 끝나면 변수를 제거하고 old binary로 해당 store를 다시 열지 않는다.

## Build, review, publication

0.149를 최종 검토·공개 완료라고 부르기 전에 다음 작업이 남아 있다.

1. build된 동일 final candidate에 대한 fresh-context 독립 review 2회 완료
2. material finding이 있으면 반영하고 최종 publication 상태 갱신

[Actions run 32517743230](https://github.com/iLfounder/codex-cli-custom/actions/runs/32517743230)은 tree `e5915796b021e81b88bd40406b62fa6c3bf89e76`를 build했고 artifact `9462237252`를 digest `sha256:4845802188c3f2a1bf3f030d1dcd8409d47278198015a205ceb55b9864ea0f42`로 업로드했다.

## 과거 작업 참고자료

식별 정보를 제거한 초기 설계 조사와 build note는 비정본 배경자료로 [`docs/handoff.md`](docs/handoff.md)와 [`docs/codex-rs-build-guide.md`](docs/codex-rs-build-guide.md)에 보존한다. 현재 0.149 설계보다 오래된 자료이며 public runtime contract가 아니다.

## License

Upstream Codex와 이 patch series는 [`LICENSE`](LICENSE)와 [`NOTICE`](NOTICE)의 조건에 따라 배포된다.
