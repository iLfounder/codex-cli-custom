> **Historical personal working note**
>
> This file is preserved as background material. Its dated measurements and recommendations are not proof of the pending 0.149 build, review, artifacts, or publication.

# codex-rs 커스텀 패치 빌드 가이드

> 대상: `openai/codex` 포크에 커스텀 패치를 유지하며 `app-server` / `cli`를 직접 빌드하는 경우
> 환경: macOS (Apple Silicon, M5 mini) 로컬 + GitHub Actions
> 작성 기준일: 2026-08-20

---

## 0. 요약 (TL;DR)

| 항목 | 결론 |
|---|---|
| 빌드가 몇 시간 걸리는 원인 | `fat LTO` + `codegen-units = 1` → 링크 단계 싱글스레드 + 대용량 RAM 점유 |
| 로컬 최우선 조치 | LTO 끄고 codegen-units 늘리기 (환경변수 오버라이드) |
| 정상 기대치 | 클린 릴리스 빌드 15~30분, 증분 5~10분 |
| 클라우드가 더 빠른가 | **아니오.** GitHub 무료 macOS 러너는 3 vCPU / 7GB로 M5 mini보다 느림 |
| 클라우드의 실익 | 속도가 아니라 **오프로딩** (내 맥이 1시간 안 묶임) |
| 프라이빗 저장소 무료 한도 | 월 2회는 가능, 3회는 아슬아슬 (10배 배율 때문) |
| 권장 구성 | **포크를 퍼블릭으로 → GitHub Actions 무제한 무료** + 로컬은 dev 프로파일로 빠른 반복 |

---

## 1. 문제 진단

### 1.1 원인

codex-rs의 릴리스 프로파일은 **LTO 활성화 + 심볼 스트립 + 단일 codegen unit** 조합이다.
(`codex-rs/Cargo.toml`의 `[profile.release]` 블록)

이 조합의 문제:

- **fat LTO**는 최종 링크 시점에 전체 크레이트 그래프의 LLVM IR을 한 프로세스에 올린다. 사실상 싱글스레드이며 메모리를 크게 먹는다. codex-rs 규모면 10GB 이상 점유할 수 있다.
- **`codegen-units = 1`**은 병렬 코드 생성을 포기한다. 코어를 아무리 늘려도 이 단계는 안 빨라진다.

### 1.2 실측 레퍼런스

codex 저장소 자체 PR에서 나온 수치:

| 조건 | 시간 | 출처 |
|---|---|---|
| 클린 릴리스 빌드 | ~18분 | PR #21574 |
| 증분 릴리스 빌드 | ~12분 | PR #21574 |
| LTO 끈 `profiling` 프로파일 — 클린 | ~13분 | PR #21574 |
| LTO 끈 `profiling` 프로파일 — 증분 | ~6분 | PR #21574 |
| M4 Max 16코어, codegen-units=1, 빈 타겟 디렉터리 | 981초 (~16분) | PR #27702 |
| M4 Max 16코어, codegen-units=4 | 507초 (~8.5분) | PR #27702 |
| GitHub CI macOS Cargo 스텝 (크리티컬 패스) | 55분 22초 | PR #27702 |
| GitHub CI 전체 워크플로 | 71분 28초 | PR #27702 |

**→ M5 mini에서 몇 시간이 나온다면 CPU가 아니라 메모리 병목이다.**

### 1.3 확인 방법

1. **Activity Monitor → 메모리 압력(Memory Pressure) 그래프**
   빌드 중 노랑/빨강으로 넘어가면 스왑 진입 → 원인 확정.

2. **`cargo build --timings`**
   HTML 리포트 생성. 마지막에 **긴 단일 막대 하나**만 보이면 LTO 링크 병목 확정.

---

## 2. 로컬 빌드

### 2.1 프로파일 오버라이드 (핵심)

패치를 유지 중이라면 `Cargo.toml`을 직접 수정하지 말 것 — 업스트림 리베이스마다 충돌한다.
**환경변수 오버라이드**가 깔끔하다.

```bash
# ~/.zshrc 또는 빌드 스크립트에
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_INCREMENTAL=true
export CARGO_PROFILE_RELEASE_DEBUG=0
```

또는 `~/.cargo/config.toml`에:

```toml
[profile.release]
lto = false
codegen-units = 16
incremental = true
debug = 0
```

> **주의:** 이 설정으로 나온 바이너리는 런타임 성능이 공식 릴리스보다 소폭 낮고 크기가 크다.
> 패치 검증·일상 사용에는 무관하다. 최종 배포본이 필요하면 그때만 원래 프로파일로 한 번 돌린다.

### 2.2 빌드할 대상 좁히기

```bash
# 워크스페이스 전체가 아니라 필요한 크레이트만
cargo build --release -p codex-app-server

# 패치 동작 확인 단계라면 dev 프로파일로 충분
cargo build -p codex-app-server
```

**절대 하지 말 것:**

- `--all-features` — 안 쓰는 기능까지 컴파일
- `--all-targets` — 테스트·벤치·예제 타겟까지 빌드, 작업량 2배 이상
- 습관적인 `cargo clean` — 증분 캐시를 통째로 버림

> 크레이트 이름은 `cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'`로 확인할 것.
> 포크 버전에 따라 패키지명이 다를 수 있다.

### 2.3 macOS 특화 설정

```toml
# codex-rs/Cargo.toml 또는 ~/.cargo/config.toml
[profile.dev]
split-debuginfo = "unpacked"    # dsymutil 우회, macOS에서 체감 큼
debug = "line-tables-only"      # 디버그 정보 최소화
```

**Xcode Command Line Tools를 최신으로 유지할 것.**
Apple의 새 링커(ld-prime)가 구버전 대비 훨씬 빠르다.
macOS에서는 `mold`를 쓸 수 없으므로 링커 쪽 레버는 이것뿐이다.

```bash
xcode-select --install
softwareupdate --list   # CLT 업데이트 확인
```

### 2.4 sccache

의존성 크레이트 재컴파일을 캐시한다. 브랜치 전환·업스트림 머지가 잦을수록 효과가 크다.

```bash
brew install sccache
export RUSTC_WRAPPER=sccache
sccache --show-stats     # 적중률 확인
```

> `incremental = true`와 sccache는 상성이 나쁠 수 있다. 둘 다 켜고 `--timings`로 비교해볼 것.

### 2.5 로컬 빌드 체크리스트

- [ ] 메모리 압력 그래프 확인 — 노랑/빨강이면 LTO부터 끈다
- [ ] `CARGO_PROFILE_RELEASE_LTO=false` 적용
- [ ] `-p <crate>`로 대상 한정
- [ ] `--all-features` / `--all-targets` 제거
- [ ] Xcode CLT 최신화
- [ ] `cargo build --timings`로 개선 전후 비교

---

## 3. GitHub Actions 빌드

### 3.1 러너 사양 — 먼저 알아야 할 것

| 러너 | CPU | RAM | 무료 분 사용 | 비고 |
|---|---|---|---|---|
| `macos-15` (표준, arm64) | 3 vCPU | **7 GB** | ⭕ 가능 | 스토리지 14GB |
| macOS larger (`-xlarge`) | 5 vCPU (M2 Pro) | 14 GB | ❌ 항상 과금 | Team/Enterprise 플랜 전용 |
| Ubuntu larger | 최대 96 vCPU | 최대 384 GB | ❌ 항상 과금 | macOS 바이너리 못 만듦 |

**핵심:** 무료로 쓸 수 있는 macOS 러너는 3 vCPU / 7GB다.
M5 mini보다 느리고, RAM은 더 빡빡하다. **CI에서도 반드시 LTO를 꺼야 한다.**

> 리눅스에서 macOS 크로스컴파일은 Apple SDK 라이선스 문제로 실질적으로 불가능하다.
> macOS 바이너리가 필요하면 macOS 러너를 써야 한다.

### 3.2 요금 구조 (2026년 1월 인하 반영)

| 항목 | 값 |
|---|---|
| 퍼블릭 저장소 | **무제한 무료** (표준 호스티드 러너, 배율 차감 없음) |
| Free 플랜 무료 분 (프라이빗) | 2,000분/월 |
| Team 플랜 무료 분 (프라이빗) | 3,000분/월 ($4/user/월) |
| Linux x64 초과분 | $0.006/분 |
| Linux arm64 초과분 | $0.005/분 |
| Windows 초과분 | $0.010/분 |
| **macOS 초과분** | **$0.062/분 (Linux의 약 10배)** |
| 셀프호스티드 러너 | 무료 (2026년 3월 예정이던 $0.002/분 요금은 무기한 연기, 8월 현재 미시행) |

### 3.3 ⚠️ 10배 배율 함정

**무료 분은 "Linux 환산" 기준이다. macOS 빌드는 실제 경과 시간의 10배로 차감된다.**

```
빌드 1회 70분 (실제 경과)
  → 700분 차감

Free 플랜  2,000분 ÷ 700 = 2.8회/월
Team 플랜  3,000분 ÷ 700 = 4.2회/월
```

추가 주의사항:

- GitHub는 **잡마다 분 단위로 올림** 처리한다. 잡이 여러 개면 손해가 누적된다.
- 같은 계정의 **다른 프라이빗 저장소 워크플로도 같은 풀에서 차감**된다.
- codex 원본 워크플로를 그대로 쓰면 **전 플랫폼 매트릭스**가 돌아 한 번에 한도가 날아간다.
  → **macOS arm64 잡 하나만 남기고 전부 쳐낼 것.**

### 3.4 권장: 포크를 퍼블릭으로

커스텀 패치가 영업비밀이 아니라면 **퍼블릭 포크가 압도적으로 유리하다.**

- 무제한 무료, 배율 차감 없음
- 빌드 횟수 눈치 볼 일 없음
- 실패해서 재실행해도 부담 없음

프라이빗이 꼭 필요하면 → Team 플랜(3,000분)이 macOS larger runner 과금보다 싸다.

### 3.5 워크플로 예시

`.github/workflows/build-patched.yml`

```yaml
name: build-patched-codex

on:
  workflow_dispatch:          # 수동 트리거 (버전 업 때만 돌림)
  push:
    tags: ['patched-v*']

jobs:
  macos-arm64:
    runs-on: macos-15         # 표준 러너 = 무료 분 대상
    timeout-minutes: 180      # 기본 6시간이지만 명시적으로 제한
    defaults:
      run:
        working-directory: codex-rs
    env:
      CARGO_TERM_COLOR: always
      # ★ CI에서도 LTO는 반드시 끈다 (7GB RAM 러너)
      CARGO_PROFILE_RELEASE_LTO: "false"
      CARGO_PROFILE_RELEASE_CODEGEN_UNITS: "16"
      CARGO_PROFILE_RELEASE_DEBUG: "0"

    steps:
      - uses: actions/checkout@v4

      # rust-toolchain.toml이 버전을 고정하므로 rustup이 자동 설치한다
      - name: Install toolchain
        run: rustup show

      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: codex-rs
          # 캐시 키에 프로파일 설정을 반영하고 싶으면 prefix-key 사용

      - name: Build
        run: cargo build --release -p codex-app-server

      - name: Strip binary
        run: strip target/release/codex-app-server

      - uses: actions/upload-artifact@v4
        with:
          name: codex-app-server-macos-arm64
          path: codex-rs/target/release/codex-app-server
          retention-days: 14
```

> **크레이트 이름 확인 필수.** `-p codex-app-server`가 실제 패키지명과 다를 수 있다.
> 로컬에서 `cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name'`로 먼저 확인할 것.

### 3.6 CI 주의사항

| 항목 | 내용 |
|---|---|
| **스토리지** | 표준 macOS 러너는 SSD 14GB. Rust target 디렉터리가 커지면 부족할 수 있다. 필요 시 불필요한 사전 설치 툴 제거 스텝 추가 |
| **캐시** | `Swatinem/rust-cache@v2` 사용. 두 번째 빌드부터 의존성 컴파일이 사라진다. 단 캐시는 저장소당 10GB 한도 |
| **매트릭스 제거** | 원본 `rust-ci.yml`은 전 플랫폼을 빌드한다. 반드시 필요한 잡만 남길 것 |
| **트리거** | `push: branches` 대신 `workflow_dispatch` + 태그 트리거. 커밋마다 돌면 한도가 순식간에 소진 |
| **잡 타임아웃** | 기본 6시간. `timeout-minutes` 명시로 폭주 방지 |
| **아티팩트 다운로드 후** | macOS quarantine 속성 제거 필요: `xattr -d com.apple.quarantine ./codex-app-server && chmod +x ./codex-app-server` |
| **코드사이닝** | 서명 없는 바이너리는 Gatekeeper가 막는다. 로컬 임시 서명: `codesign -s - ./codex-app-server` |
| **실패 재실행** | 실패해도 소비된 분은 환불되지 않는다. 프라이빗이면 특히 주의 |

---

## 4. 의사결정 트리

```
app-server를 어디서 실행하는가?
│
├─ macOS에서 실행
│  │
│  ├─ 패치가 공개 가능한가?
│  │  ├─ 예 → 퍼블릭 포크 + GitHub Actions (무제한 무료) ★ 권장
│  │  └─ 아니오 → 프라이빗 + Team 플랜(3,000분), 월 4회까지
│  │
│  └─ 어느 쪽이든: 로컬은 dev 프로파일로 빠른 반복,
│     릴리스 빌드만 CI로 오프로딩하는 하이브리드
│
└─ 리눅스 컨테이너/서버에서 실행
   └─ Hetzner 등 ARM VPS + 셀프호스티드 러너
      (셀프호스티드 플랫폼 요금 미시행, 월 3만원대)
      → 이 경우 클라우드가 실제로 더 빠르다
```

---

## 5. 참고 링크

- codex PR #21574 — profiling 프로파일 추가, LTO 빌드 시간 측정
  https://github.com/openai/codex/pull/21574
- codex PR #27702 — ThinLTO + codegen-units=4 전환, CI 시간 측정
  https://github.com/openai/codex/pull/27702
- codex Issue #1411 — codegen-units=1 도입 논의
  https://github.com/openai/codex/issues/1411
- The Rust Performance Book — Build Configuration
  https://nnethercote.github.io/perf-book/build-configuration.html
- The Cargo Book — Profiles
  https://doc.rust-lang.org/cargo/reference/profiles.html
- GitHub Docs — Larger runners reference (머신 사양)
  https://docs.github.com/en/actions/reference/runners/larger-runners
- GitHub Changelog — 2026 Actions 요금 변경
  https://github.blog/changelog/2025-12-16-coming-soon-simpler-pricing-and-a-better-experience-for-github-actions/

---

## 6. 다음 단계

1. 로컬에서 `cargo build --timings` 1회 실행 → 병목 지점 확정
2. 환경변수 오버라이드 적용 후 재측정 → 개선폭 확인
3. 개선폭이 충분하면 로컬 유지, 부족하면 CI 구성으로 이동
4. CI 구성 시 패키지명 확인 → 워크플로 작성 → 1회 시험 실행으로 실제 소요 분 측정
5. 측정된 분으로 월 빌드 횟수 예산 재계산
