# RisuNest Sync 관리 앱

RisuNest Sync 관리 앱은 실행 중인 독립 데몬을 관리합니다. Windows와 macOS는 GUI를 제공하며 Linux는 CLI/TUI를 제공합니다. 관리 앱을 종료해도 데몬은 계속 동작합니다.

일반적인 설치, 기기 등록, 연결, 백업과 복원 방법은 [`../sync/README.md`](../sync/README.md)를 확인하세요.

## 실행

데몬, 관리 앱, `cloudflared`는 같은 배포 디렉터리에 있어야 합니다. Windows 실행 파일에는 `.exe`가 붙습니다. Windows 배포에는 예약 업데이트가 콘솔 창 없이 실행하는 `risunest-sync-manager-background.exe`도 같은 디렉터리에 포함됩니다.

Windows에서 업데이트 후 GUI를 자동으로 다시 열려면 같은 사용자와 로그인 세션의 Windows Explorer 데스크톱이 실행 중이어야 합니다. 업데이트 완료 후 GUI 재실행만 실패한 경우 관리 앱을 직접 다시 실행하세요.

```text
risunest-sync-manager
risunest-sync-manager status
risunest-sync-manager start
risunest-sync-manager stop
risunest-sync-manager autostart install
risunest-sync-manager autostart remove
risunest-sync-manager update status
risunest-sync-manager update policy automatic|notify|off
risunest-sync-manager update check
```

명령을 생략하면 TUI가 열립니다. 기기 등록 링크는 한 번만 표시되며, 지원하는 터미널에서는 같은 값을 QR로 표시합니다. 기기 해제는 접근 권한만 제거하고 라이브러리 데이터는 보존합니다.

Linux 관리형 압축 파일은 일반 사용자로 `sh install.sh`를 실행해 설치합니다. 비대화형 설치는 `sh install.sh --non-interactive --policy=automatic`을 사용합니다. 설치 프로그램은 방화벽을 변경하거나 root 데몬으로 전환하지 않습니다.

## 개발과 검증

저장소의 공용 `CARGO_TARGET_DIR`을 사용하세요.

```sh
cargo test --manifest-path server/sync/Cargo.toml --locked
cargo test --manifest-path server/manager/Cargo.toml --locked
pnpm --dir server/manager/gui install --frozen-lockfile
pnpm --dir server/manager/gui run check
pnpm --dir server/manager/gui run test
```

Windows GUI 재실행 수명 검사는 일반 `cargo test`와 릴리스 CI에서 제외되는 수동 전용 검사입니다. PowerShell 7과 Rust 도구 체인이 설치되어 있고 현재 사용자로 Windows Explorer 데스크톱에 로그인한 환경에서 실행하세요. 공용 `CARGO_TARGET_DIR` 설정을 그대로 사용합니다.

```powershell
pwsh -File ./server/manager/tests/windows-gui-relaunch.ps1
```

이 스크립트는 합성 프로브를 빌드하고 정확히 한 개의 재실행 테스트를 실행합니다. 호출 프로세스의 PATH가 비어 있어도 프로브가 실행되며, 호출 프로세스의 job을 종료한 뒤에도 응답하는지 확인합니다. 프로브와 로그는 `server/manager/.test-output/gui-relaunch-*`에 저장됩니다. `-Release`와 `-Target <Rust target triple>`을 지정할 수 있으며, 지정한 타깃의 실행 파일을 실행할 수 있는 Windows 환경이 필요합니다. 데스크톱 접근이나 실행이 실패하면 검사는 실패로 끝납니다.

로컬 배포 빌드는 현재 운영체제에서 실행합니다:

```sh
pnpm sync:windows:build
pnpm sync:linux:build
pnpm sync:macos:build
```
