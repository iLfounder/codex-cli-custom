<div align="right">
  <strong>KO</strong> | <a href="README.md">EN</a>
</div>

# Codex CLI Custom

여러 계정과 장기 실행 터미널 세션을 하나의 로컬 app-server에서 다루기 위한 OpenAI Codex 실험적 fork다.

이 fork는 계정 선택, thread 소유권, session 인계, 외부 session 제어를 명시적인 계약으로 제공한다. Credential과 로컬 경로, 외부 workflow 고유 식별자는 비공개로 유지하면서 typed Goal action과 설치 가능한 plugin command도 추가한다.

> 공식 OpenAI 배포물이 아니다. 현재 series는 upstream [`rust-v0.149.0`](https://github.com/openai/codex/releases/tag/rust-v0.149.0), commit `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`을 대상으로 한다.

## Patch series가 추가하는 기능

| Patch | 사용자에게 제공되는 결과 |
|---|---|
| P001 | Thread마다 하나의 영속 writer authority를 두고 stale writer를 거절한다. |
| P002 | Session, account, Goal, continuity control을 위한 versioned app-server v2 JSON·TypeScript 계약을 제공한다. |
| P003 | 하나의 app-server에서 격리된 여러 account slot을 운용한다. |
| P004 | Resume, fork, child thread에도 유지되는 thread-account binding과 versioned Goal state를 제공한다. |
| P005 | 한 turn에서 model, MCP, app, plugin, hook, telemetry가 같은 execution account를 사용한다. |
| P006 | `sessionRuntime/list`, runtime change event, 허용 action, committed `/clear`·`/new` continuity receipt를 제공한다. |
| P007 | App-server 재시작 없이 account login, 재인증, secondary account logout을 수행한다. |
| P008 | 명시적인 `released` 또는 `failed` 결과가 있는 strict `thread/relinquish`를 제공한다. |
| P009 | Thread ID를 유지한 채 idle thread의 account를 전환한다. |
| P010 | TUI `/account`, `/logout`, `/exit`, `/clear`, `/new`, `/goal`과 agent 요청형 clear/new control을 제공한다. |
| P011 | 설치 가능한 `/namespace:name` plugin command와 ephemeral card, notice, progress presentation을 제공한다. |

App-server는 opaque account reference와 sanitize된 session state만 노출한다. 외부 workflow role, group ID, 사용자 handle은 저장하지 않는다.

## 제공되는 interface

- session 목록과 상태: `sessionRuntime/list`, `sessionRuntime/changed`
- account 관리: `accountSlot/list`, `accountSlot/login/start`, `accountSlot/logout`
- account 전환: `thread/account/switch`
- writer 반환: `thread/relinquish`
- committed clear/new continuity: `thread/start`의 transition field, `thread/transition/commit`, runtime continuity projection
- Goal state: `thread/goal/get`, `thread/goal/create`, `thread/goal/set`, `thread/goal/replace`, `thread/goal/clear`
- plugin command: `pluginCommand/list`, `pluginCommand/invoke`
- ephemeral UI output: `thread/presentation/append`

Patch에는 생성된 Rust, JSON Schema, TypeScript 정의가 `codex-rs/app-server-protocol/schema/` 아래에 포함된다.

## 적용과 build

정확한 upstream commit에만 열한 개 patch를 적용한다.

```sh
git checkout 758ef40f50c1a458425c7cfbf1eb12cbc07af0b0
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD"
```

Applier는 clean tree를 요구하고 각 patch digest를 검증한 뒤 P001–P011을 순서대로 적용하고 최종 Git tree를 확인한다. POSIX shell, Git, `sed`, `awk`, `shasum` 또는 `sha256sum`이 필요하다.

`codex-rs`에서 로컬 build:

```sh
cargo build --locked --release -p codex-cli --bin codex
cargo build --locked --release -p codex-app-server --bin codex-app-server
```

수동 GitHub Actions workflow는 표준 macOS arm64 runner에서 다음 산출물을 만든다.

- `codex`
- `codex-app-server`
- `SHA256SUMS`
- `BUILD-METADATA.txt`
- `LICENSE`
- `NOTICE`

업로드 artifact에는 압축 archive와 SHA-256 checksum이 들어간다.

## 기존 0.148 custom state store 업그레이드

같은 store를 공유하는 구버전 TUI와 app-server를 모두 종료한다. 0.149 build를 `CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1`로 한 번만 시작한 뒤 변수를 제거한다. Migration은 알려진 legacy schema를 검증한 후에만 적용하며 unknown 또는 partial schema는 거절한다. Migration이 끝난 store를 구버전 binary로 다시 열지 않는다.

## 저장소 구성

- `custom-patches/rust-v0.149.0/`: 현재 ordered series와 digest manifest
- `custom-patches/rust-v0.148.0/`: 재현성을 위해 보존한 이전 series
- `custom-patches/apply-series.sh`: clean-tree patch applier
- `.github/workflows/build-custom-macos-arm64.yml`: 수동 macOS arm64 build

## License

Upstream Codex와 이 patch series는 [`LICENSE`](LICENSE)와 [`NOTICE`](NOTICE)의 조건에 따라 배포된다.
