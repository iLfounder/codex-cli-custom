# 0.153.3 U01–U21 포팅

대상은 upstream `rust-v0.153.3`의 정확한 commit `b1a547b1f73ce86205d9222ac19cff334b3b7a2e`다. 패치 순서·SHA-256·최종 tree 정본은 [series.toml](series.toml)이다. 기존 0.152 series를 덮어쓰지 않는다.

새로운 clean upstream checkout에서 series를 명시하여 적용한다.

```sh
git switch --detach b1a547b1f73ce86205d9222ac19cff334b3b7a2e
/path/to/codex-cli-custom/custom-patches/apply-series.sh "$PWD" rust-v0.153.3
```

적용 후 최종 tree는 `5328dc836cfeaa2658e938e02b0d6552e37ab4d0`다. 이 series는 workspace의 0.153.3 버전을 `Cargo.lock`에 포함하므로 0.152 빌드 설명의 placeholder 치환을 추가 실행하지 않는다. Rust toolchain 및 패키지 의존성은 적용된 소스의 `rust-toolchain.toml`과 `scripts/codex_package/`를 따른다.

현재 검증 범위는 21개 패치의 순차 재적용 및 tree 일치, Windows x64 release·패키지 소비·실제 packaged ConPTY의 원격 권위/재접속 mock이다. 별도 native Windows sandbox 6개 실패가 남아 있으므로 전체 workspace 통과를 의미하지 않는다. 대상 Mac의 현지 빌드, 실제 관리 런타임 교체 및 Windows TUI–Mac app-server 최종 검증은 아직 완료되지 않았다. 기존 CI/CD 설치 경로는 이 포팅 산출물을 자동으로 활성화하지 않는다.
