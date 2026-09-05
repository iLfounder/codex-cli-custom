# 0.153.3 U01–U21 포팅

대상은 upstream `rust-v0.153.3`의 정확한 commit `b1a547b1f73ce86205d9222ac19cff334b3b7a2e`다. 패치 순서·SHA-256·최종 tree 정본은 [series.toml](series.toml)이다. 기존 0.152 series를 덮어쓰지 않는다.

새로운 clean upstream checkout에서 series를 명시하여 적용한다.

```sh
git switch --detach b1a547b1f73ce86205d9222ac19cff334b3b7a2e
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD" rust-v0.153.3
```

적용 후 최종 tree는 `eccd8fb041e83ceb6090144c6561adc4d35863b8`다. 이 series는 workspace의 0.153.3 버전을 `Cargo.lock`에 포함하므로 0.152 빌드 설명의 placeholder 치환을 추가 실행하지 않는다. Rust toolchain 및 패키지 의존성은 적용된 소스의 `rust-toolchain.toml`과 `scripts/codex_package/`를 따른다.

현재 series는 source commit `cf23416262f30b1dcc84fc617313916a266c5581`의 tree를 재현하며, 21개 패치의 순차 재적용 및 기존 적용 스크립트의 최종 tree 일치를 확인했다. 이전 source `ae33927bf27a3739d139f92b987a067f70e67537`에서는 Windows x64·Mac arm64 release 빌드와 패키지 소비, packaged ConPTY 원격 권위/재접속 mock, 실제 SSH 연결을 통한 goal·approval 검증이 통과했다. 새 source `cf23416262f30b1dcc84fc617313916a266c5581`에서는 수정한 Windows 개발 CLI(ci-test profile)로 실제 원격 account 전환·footer·legacy adapter 표시·resize 검증이 통과했다. 이전 release의 검증 결과를 새 source release의 완료 근거로 간주하지 않는다.

새 source의 release 빌드는 진행 중이며, 최종 동일 source의 Mac 산출물 검증과 실제 관리 런타임 교체는 아직 완료되지 않았다. 별도 native Windows sandbox 6개 실패도 남아 있으므로 전체 workspace 통과를 의미하지 않는다. 기존 CI/CD 설치 경로는 이 포팅 산출물을 자동으로 활성화하지 않는다.
