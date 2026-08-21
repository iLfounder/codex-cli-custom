> **Historical personal working note**
>
> This file is preserved for investigation provenance. It predates the current 0.149 design, contains machine-local paths and historical runtime observations, and is not the public installation or runtime contract.

# Codex CLI Custom — 다계정 동일 UUID 안전성 handoff

작성 기준: 2026-08-20
대상 작업대: `/Users/daniel/projects/MacDev-Env/Codex-Cli-Custom`
이 문서는 새 Codex 버전의 소스를 이 디렉터리에 받은 뒤 구현을 재개하기 위한 정본 handoff다.

## 1. 현재 상태

- 이 디렉터리는 아직 Git repository가 아니며, Git 초기화·remote·branch·user 설정은 사용자가 이후 직접 수행한다.
- 새 버전 Codex 소스와 새 binary는 아직 없다.
- 기존 `codex-rs-build-guide.md`는 Claude가 작성한 참고자료로 원문 그대로 보존했다.
- 이전 `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0`은 공식 `rust-v0.147.0` tag `be6e8eac029b183056b7e4402879f15d2c85f61b`의 pristine detached checkout이다. tracked/untracked source 변경과 `codex-rs/target` build cache가 없으므로 옮길 커스텀 코드나 artifact는 없다.
- 따라서 0.147.0 전체를 복사하지 않았다. 필요할 때만 read-only reference로 비교한다.

## 2. 이 작업의 정확한 목표

Relay를 제거하거나 대체하는 작업이 아니다.

목표는 account1~10이 같은 session UUID와 공유 sessions/SQLite를 번갈아 사용할 때:

1. 같은 UUID에 둘 이상의 writer가 동시에 붙지 않게 한다.
2. 명시적인 TUI `/exit` 후 idle thread가 writer를 즉시 반환하게 한다.
3. JSONL append ordinal과 SQLite materialized projection이 어긋나 세션이 TUI 목록에서 누락되는 재발을 막는다.
4. `llc-relay`의 계정 login, app-server lifecycle, exact-session delivery 역할은 유지한다.

비목표:

- Relay 제거
- AccountSwap 복원 또는 재도입
- archive/delete를 release 수단으로 사용
- active turn을 사용자 승인 없이 interrupt
- 손상된 대표 세션을 이 구현 과정에서 자동 복구
- picker의 cwd 필터 정책 변경

## 3. 대표 장애와 확인된 증거

대표 session UUID:

`01a0001f-c81b-7f00-9d74-7c15eaf1a3aa`

확인된 사실:

- canonical JSONL은 존재하고 JSON parsing이 가능했다.
- 크기 `390,496,625 bytes`, 전체 `57,399 lines`였다.
- SQLite `thread_history_1` projection은 다음 ordinal `48,556`, byte offset `356,484,549`에서 정지했다.
- JSONL 물리 48,557번째 줄에서 ordinal이 `48,555 → 48,250`으로 역행했고 `48,250`이 중복되었다.
- 그 뒤 물리 `8,843 lines`가 projection에 반영되지 않았다.
- `logs_2`에서 `expected ordinal 48556, got 48250` 경고가 27회 확인됐다.
- 즉 agent가 기억을 잃은 것이 아니라 source JSONL은 남아 있고 SQLite projection과 TUI 표시가 뒤처진 상태였다.

별개의 표시 문제:

- 기본 picker는 현재 cwd exact filter의 영향을 받는다.
- 대표 세션은 Orbite에서 시작해 후반에 Ananke cwd/name으로 이동했다.
- `--all`에서는 eligible한 세션이었다.
- 이 cwd 문제는 “기본 TUI 목록에서 안 보임” 일부를 설명하지만 ordinal 역행과 projection 정지는 설명하지 못한다.

## 4. 근본 원인

### 4.1 공유 store와 분리된 writer-lock namespace

현재 다계정 설정은 sessions/config/SQLite가 account1을 공유하지만 stock 0.147 writer lock은 각 `CODEX_HOME` 아래에 생성된다.

0.147 source:

- `codex-rs/thread-store/src/local/writer_lock.rs`
  - `WriterLockCoordinator::new(codex_home)`
  - `${CODEX_HOME}/thread-writer-locks/<UUID>.lock`
- `codex-rs/thread-store/src/local/mod.rs`
  - `WriterLockCoordinator::new(&config.codex_home)`

실측 account1~5의 `thread-writer-locks`는 서로 다른 실제 directory/inode였다. 따라서 account2 app-server가 UUID를 쓰고 있어도 account3 app-server는 그 lock을 보지 못하고 같은 account1 JSONL에 두 번째 writer로 붙을 수 있었다.

가장 강한 장애 설명은 stale ordinal을 가진 두 번째 writer가 같은 rollout에 늦게 append해 ordinal 역행을 만들었다는 것이다. 이중 writer는 SQLite 파일 자체의 low-level corruption보다는 JSONL event order를 깨뜨리고 materializer를 멈추게 한다.

### 4.2 `/exit`는 writer release가 아니다

0.147 source:

- `codex-rs/tui/src/app/thread_routing.rs`
  - explicit exit는 exact thread에 `thread/unsubscribe`를 보낸다.
- `codex-rs/tui/src/app/event_dispatch.rs`
  - TUI는 이 요청에 최대 약 2초만 기다리고 종료한다.
- `codex-rs/app-server/src/request_processors/thread_processor.rs`
  - `thread/unsubscribe`는 subscriber만 제거한다.
- `codex-rs/app-server/src/request_processors/thread_lifecycle.rs`
  - no-subscriber와 inactive가 모두 유지된 뒤 30분 후 unload를 시도한다.
  - shutdown wait는 최대 10초다.
- `codex-rs/thread-store/src/local/live_writer.rs`
  - 정상 shutdown에서 recorder를 닫고 SQLite materialization을 수행한 뒤 live recorder를 제거한다.
  - recorder가 제거돼 writer guard가 drop되어야 kernel lock이 반환된다.

따라서 TUI process가 종료됐다는 사실만으로 app-server writer가 반환됐다고 볼 수 없다. Active turn 중 exit하면 turn이 끝난 뒤부터 idle retention이 다시 계산될 수 있다.

## 5. Custom Codex에서 구현할 최소 해법

두 패치는 서로 다른 보장을 담당한다. 둘 다 필요하다.

### Patch A — writer lock을 공유 state root에 결속

목표:

- 같은 sessions/SQLite authority를 쓰는 모든 account가 동일 UUID lock file을 사용한다.
- 한 account가 writer인 동안 다른 account의 resume/new/fork writer 생성은 `active writer` conflict로 안전하게 실패한다.

권장 구현:

- 새 버전에서도 `LocalThreadStoreConfig`가 `sqlite: SqliteConfig`를 보유한다면 writer lock root를 `config.codex_home` 대신 `config.sqlite.home()`에서 파생한다.
- 즉 logical path는 `${sqlite_home}/thread-writer-locks/<UUID>.lock`이다.
- 임의 symlink topology가 아니라 source-level authority를 일치시킨다.
- `sqlite_home`이 기본값으로 `CODEX_HOME`인 일반 single-account 설치에서는 기존 동작이 유지된다.

필수 테스트:

1. 서로 다른 `codex_home`, 동일한 `sqlite_home`을 가진 store A/B를 만든다.
2. A가 UUID lock을 보유한 동안 B acquire가 conflict인지 확인한다.
3. A guard drop 뒤 B acquire가 성공하는지 확인한다.
4. 서로 다른 `sqlite_home`은 같은 UUID라도 독립적으로 동작하는지 확인한다.
5. coordination lock과 stale-file cleanup도 동일 shared root를 사용하는지 확인한다.

주의:

- writer lock root만 바꾸면 무결성은 확보되지만, old app-server가 30분 동안 lock을 들고 있어 다음 account resume이 오래 막힐 수 있다.
- stock binary와 patched binary를 같은 shared store에 섞으면 stock 쪽이 옛 account-local lock을 사용해 안전 보장이 깨진다.

### Patch B — explicit idle relinquish

목표:

- ordinary disconnect/reconnect 유예는 기존 30분 정책을 유지한다.
- 사용자가 명시적으로 `/exit`하거나 Relay가 account handoff를 요청한 경우에만 exact UUID를 즉시 flush, materialize, unload하고 writer를 반환한다.

권장 API:

- app-server v2에 명시적인 `thread/relinquish` RPC를 추가한다.
- 기존 `thread/unsubscribe`의 일반 의미를 바꾸지 않는다.
- bool flag보다 의미가 드러나는 별도 RPC와 구조화된 response를 우선한다.

권장 response 상태:

- `released`
- `notLoaded`
- `activeTurn`
- `otherSubscribers`
- `shutdownTimedOut`

안전 조건:

1. 요청 connection의 subscription을 해제한다.
2. 다른 subscriber가 없음을 확인한다.
3. thread가 inactive임을 확인한다.
4. 기존 `shutdown_and_wait`/thread-store shutdown 경로로 JSONL flush와 SQLite materialization을 끝낸다.
5. `ThreadManager`와 thread state/watch registry에서 제거한다.
6. writer guard가 drop된 뒤에만 `released`를 응답한다.
7. active turn 또는 다른 subscriber가 있으면 writer를 강제로 빼앗지 않고 구조화된 상태를 반환한다.

재사용 후보:

- `app-server/src/request_processors/thread_lifecycle.rs`
  - `wait_for_thread_shutdown`
  - `unload_thread_without_subscribers`
- `app-server/src/request_processors/thread_processor.rs`
  - `thread_unsubscribe_response_inner`
  - `finalize_thread_teardown`
- `core/src/thread_manager.rs`
  - `remove_thread`
- `thread-store/src/local/live_writer.rs`
  - `shutdown_thread`

TUI 변경:

- explicit `/exit`, `/quit`과 graceful double Ctrl-C/D의 `ShutdownFirst` 경로는 `thread/relinquish`를 호출한다.
- 단순 socket EOF, crash, terminal loss는 ordinary disconnect로 남긴다.
- 현재 약 2초 UI timeout은 app-server의 최대 10초 shutdown 계약과 맞지 않는다. 새 RPC response를 기다릴 bounded budget과 timeout UX를 별도로 정한다.
- timeout을 release 성공으로 취급하지 않는다.

## 6. Relay와의 경계

Relay의 기존 역할은 유지한다.

- `llc-relay codexN login`: registered account home의 app-server login authority
- account별 app-server lifecycle과 owner marker/socket
- scheduler, MCP, `llc-relay send`의 exact-session delivery
- `thread/resume`, turn start/steer, status 관측

Shared writer lock이 corruption 방지의 최종 authority다. 이 목표만 보면 Relay에 복잡한 영속 handoff state machine은 필요 없다.

다만 “반드시 사용자가 선택한 target account가 다음 writer가 되어야 한다”는 결정론까지 원하면 source relinquish부터 target attach까지 Relay adapter delivery를 잠깐 막는 per-UUID transition lease가 필요하다. 이 lease는 무결성이 아니라 target 선점 경쟁을 막는 UX 계층이다.

Relay delivery도 writer가 될 수 있다. notLoaded UUID에 예약 메시지나 MCP/send가 들어오면 adapter가 `resumeThread`를 호출하기 때문이다. Shared lock이 있으면 중복 writer 대신 한쪽이 conflict로 안전하게 실패한다.

## 7. 새 버전 source를 받은 직후 확인할 것

새 source에 이미 같은 문제가 고쳐졌을 수 있으므로 patch부터 적용하지 않는다.

먼저 찾는다:

```bash
cd codex-rs
rg -n "thread-writer-locks|WriterLockCoordinator::new|THREAD_UNLOADING_DELAY"
rg -n "thread/unsubscribe|thread/relinquish|thread/unload"
rg -n "shutdown_current_thread|shutdown_and_wait|remove_thread"
```

판정:

- writer lock이 여전히 `codex_home` 기준이면 Patch A 필요.
- explicit exit가 여전히 unsubscribe뿐이고 idle retention 뒤에만 unload하면 Patch B 필요.
- upstream에 `thread/relinquish`/`thread/unload`가 생겼다면 새 구현을 만들지 말고 그 계약을 검증해 재사용한다.
- app-server protocol 변경은 v2에만 추가하고 Rust/TypeScript schema와 `app-server/README.md`를 함께 갱신한다.

0.147 reference files:

- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/thread-store/src/local/writer_lock.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/thread-store/src/local/mod.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/thread-store/src/local/live_writer.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/app-server/src/request_processors/thread_lifecycle.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/app-server/src/request_processors/thread_processor.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/tui/src/app/thread_routing.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/tui/src/app/event_dispatch.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/app-server/tests/suite/v2/thread_unsubscribe.rs`
- `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0/codex-rs/thread-store/src/local/writer_lock_tests.rs`

## 8. 빌드 주의사항

### 8.1 기존 guide의 중요 정정

`codex-rs-build-guide.md`는 참고자료지만, 0.147 실측과 다른 부분이 있다.

- guide의 `fat LTO + codegen-units=1` 진단은 0.147에 해당하지 않는다.
- 실제 0.147 `[profile.release]`는 `lto = "thin"`, `codegen-units = 4`, `debug = "line-tables-only"`, `strip = false`다.
- 0.147 workspace default member는 135개다.
- 현재 머신에는 `sccache`가 설치되어 있지 않다.
- 0.147 checkout에 `target/`이 없으므로 새 빌드는 cold build다.
- 10 logical CPU, 16GB RAM 환경에서 swap 사용 이력이 약 19GB까지 올라가 있었다. 현재 한 시점의 memory free는 충분했지만, release link와 다수 process가 겹치면 swap thrashing 가능성이 있다.

따라서 “LTO만 끄면 전부 해결”로 단정하지 않는다. 반복 build가 하루를 소모한 원인은 release LTO, cold/missing target cache, 넓은 test/build 범위, 16GB 메모리 압박이 겹친 것으로 본다.

### 8.2 반복 개발 loop

첫 단계는 debug build다.

```bash
cd codex-rs

# backend/RPC 반복
cargo build -p codex-app-server --bin codex-app-server --jobs 4

# TUI까지 포함한 실제 codex binary
cargo build -p codex-cli --bin codex --jobs 4
```

원칙:

- 같은 checkout과 같은 `target/`을 계속 유지한다.
- 습관적으로 `cargo clean`하지 않는다.
- 수정마다 `--release`를 빌드하지 않는다.
- `--workspace`, `--all-features`, `--all-targets`를 기본값으로 쓰지 않는다.
- toolchain/profile/RUSTFLAGS를 자주 바꾸면 cache key가 달라져 대량 재compile될 수 있다.
- 동시에 여러 Rust build를 실행하지 않는다.

### 8.3 최소 검증

새 버전의 `AGENTS.md`를 먼저 읽고 그 버전의 정확한 package name과 test 명령을 따른다. 0.147 기준 최소 후보:

```bash
just fmt
just test -p codex-thread-store
just test -p codex-app-server-protocol
just test -p codex-app-server
just test -p codex-tui
```

매 수정마다 네 package를 전부 돌리지 않는다. 변경한 crate의 focused test를 먼저 돌리고, protocol/TUI 계약이 안정된 뒤 필요한 인접 test를 한 번 수행한다. Workspace 전체 `just test`는 사용자의 별도 승인 없이는 실행하지 않는다.

### 8.4 최종 local release

패치 검증과 smoke가 끝난 뒤에만 실제 `codex` binary를 한 번 release build한다.

```bash
cd codex-rs

CARGO_PROFILE_RELEASE_LTO=false \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
CARGO_PROFILE_RELEASE_INCREMENTAL=true \
CARGO_PROFILE_RELEASE_DEBUG=0 \
cargo build --release -p codex-cli --bin codex --jobs 4
```

이 설정은 correctness를 바꾸지 않는다. 공식 release보다 binary size 또는 일부 runtime performance가 불리할 수 있지만 writer lock/RPC 패치 검증과 로컬 운영에는 영향이 작다.

최초 1회에는 `--timings`를 추가해 긴 구간이 compile인지 final link인지 확인한다. 개선 전후 timing을 섞지 말고 같은 target/cache 조건에서 비교한다.

### 8.5 binary 배치 전

- binary의 `codex --version`을 기록한다.
- source commit SHA와 dirty 여부를 기록한다.
- `otool`/codesign/quarantine 상태가 필요한 배치 방식인지 확인한다.
- Relay가 standalone `codex-app-server`가 아니라 `codex app-server ...`를 실행하므로 최종 배치 대상은 `codex-cli`의 `codex` binary다.
- TUI와 account1~10 app-server가 모두 같은 patched build를 사용해야 shared-lock 보장이 성립한다.

## 9. 필수 acceptance smoke

### A. Single-writer

1. 격리 home A/B가 sessions와 SQLite home을 공유하게 구성한다.
2. A에서 UUID를 resume해 writer lock을 보유한다.
3. B에서 같은 UUID resume이 exact active-writer conflict로 실패하는지 확인한다.
4. conflict 동안 JSONL hash/size/마지막 ordinal이 예상 밖으로 변하지 않는지 확인한다.

### B. Explicit idle exit

1. A의 idle UUID를 명시적으로 `/exit`한다.
2. `thread/relinquish` response가 `released`인지 확인한다.
3. canonical UUID lock을 nonblocking acquire할 수 있는지 확인한다.
4. B resume이 한 번 성공하는지 확인한다.

### C. Active turn과 다른 subscriber

- active turn 중 relinquish는 `activeTurn`으로 거절되고 turn과 writer가 유지돼야 한다.
- 다른 subscriber가 있으면 `otherSubscribers`로 거절돼야 한다.
- 자동 interrupt와 writer 강탈은 없어야 한다.

### D. 비정상 종료

- TUI SIGKILL/socket EOF는 explicit relinquish로 오인하지 않는다.
- ordinary retention 또는 Relay의 명시적 후속 relinquish만 적용한다.
- PID 소멸만으로 release 성공을 판정하지 않는다.

### E. Projection 무결성

- cross-account handoff 전후 JSONL ordinal이 단조 증가해야 한다.
- SQLite `thread_history` materialization이 마지막 JSONL event까지 따라와야 한다.
- `expected ordinal X, got Y`가 없어야 한다.

### F. 회귀 경계

- 같은 app-server의 다른 UUID/TUI가 영향을 받지 않는다.
- `llc-relay codexN login`, initialize, loaded list, resume, idle turn/start, active turn/steer가 유지된다.
- name/picker/`--last` 동작은 이 패치가 임의 변경하지 않는다.

## 10. 실제 cutover 주의

새 shared-lock root 패치는 실행 중인 stock app-server와 혼용하면 안 된다.

이유:

- old stock process는 account-local writer lock을 계속 보유한다.
- new patched process는 sqlite-home writer lock을 본다.
- 두 namespace가 동시에 살아 있으면 패치 전과 같은 이중 writer가 다시 가능하다.

따라서 실제 전환은 별도 승인 아래 다음 순서로 한다.

1. 대상 binary/source provenance와 focused smoke 완료
2. Relay delivery와 직접 TUI writer 생성 중지
3. account1~10의 old TUI/app-server writer FD가 모두 사라졌는지 확인
4. 모든 account app-server와 TUI binary를 동일 patched build로 전환
5. account 하나로 canary initialize/login/resume/exit/re-resume
6. 나머지 account 순차 admission
7. shared lock realpath와 live FD readback

이 문서 작성 시점에는 process stop/restart, runtime 설정, account auth, SQLite, sessions, writer-lock topology를 변경하지 않았다.

## 11. Relay 호환성 참고

과거 0.148 격리 감사에서는 Relay가 사용하는 주요 app-server 계약이 유지됐다.

- `initialize`
- `thread/list`, `thread/loaded/list`, `thread/read`, `thread/resume`
- `thread/name/set`
- `turn/start`, `turn/steer`
- `mcpServer/tool/call`
- login과 remote-control

당시 실제 버전 전환 blocker는 `llc-relay/scripts/install-local-plugin.sh`의 exact `0.147.0` pin이었다. 새 버전에서 같은 gate가 남아 있는지 다시 확인한다. 살아 있는 old app-server/TUI는 CLI 파일만 교체해도 자동 업데이트되지 않으므로 process별 binary version 혼합을 금지한다.

## 12. 관련 정본과 식별자

- LLC work: `01a01d28-e370-7c21-933f-91b556744948`
- 대표 손상/누락 session: `01a0001f-c81b-7f00-9d74-7c15eaf1a3aa`
- Relay/app-server 담당 session: `019fdf43-0e2b-7dd0-a37f-96e43357a06d`
- 0.147 reference checkout: `/Users/daniel/projects/MacDev-Env/Codex-Cli-0.147.0`
- Relay repo: `/Volumes/WorkSSD/projects/Intuilogic/91-LLC-Dev/llc-relay`
- shell surface: `/Volumes/WorkSSD/projects/Intuilogic/91-LLC-Dev/llc-codex-cli-resume`

현재 LLC draft r5는 stock-only 해법을 전제로 작성되어 있다. Custom Codex 방향이 확정되면 그대로 구현계획으로 승격하지 말고 목표를 custom shared-lock + explicit relinquish로 revise해야 한다.

## 13. 다음 session의 시작 순서

1. 이 디렉터리에서 Git/source 준비 상태와 새 version SHA를 확인한다.
2. 새 source의 가장 가까운 `AGENTS.md`를 읽는다.
3. Section 7의 `rg`로 upstream 수정 여부를 먼저 판정한다.
4. LLC work를 custom-build 방향으로 revise한다.
5. Patch A를 먼저 구현하고 cross-home single-writer test를 통과시킨다.
6. Patch B와 v2 protocol/TUI behavior를 구현한다.
7. debug focused test와 격리 A/B smoke를 반복한다.
8. 안정된 뒤 fast local release를 한 번 만들고 Relay canary 계획을 확정한다.
