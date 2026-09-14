# RisuNest Sync 관리 앱

Windows/macOS GUI와 Linux CLI/TUI는 실행 중인 독립 데몬에 연결합니다. GUI/TUI를 종료해도 데몬은 계속 동작합니다. 관리 화면은 서버 DB를 직접 수정하지 않습니다.

## 실행

데몬, CLI, GUI, cloudflared는 같은 디렉터리에 배치합니다. GUI는 `risunest-sync-gui`, CLI는 `risunest-sync-manager`입니다. Windows 실행 파일에는 `.exe`가 붙습니다.

```text
risunest-sync-manager [--data-dir ABSOLUTE_PATH] [--server ABSOLUTE_EXECUTABLE]
risunest-sync-manager status
risunest-sync-manager autostart install
risunest-sync-manager autostart status
risunest-sync-manager start
risunest-sync-manager stop
risunest-sync-manager autostart remove
```

명령을 생략하면 개요, 기기, 연결, 실행 설정 메뉴가 열립니다. 방향키·Enter·1~9·Esc 또는 일반 번호 입력을 사용합니다. 기기 등록 링크는 발급 화면에서 한 번 표시하며, 지원하는 터미널에서 너비·높이가 충분할 때 QR도 표시합니다. 링크는 QR 여부와 관계없이 항상 제공합니다. 기기 해제는 접근 권한만 제거하고 라이브러리 데이터는 보존합니다.

GUI에는 고정 주소, 임시 주소(Cloudflare Tunnel), 레지스트리 주소·기본값 복원·현재 UUID, 기기 등록·해제, 파일 용량·남은 공간·실행 시간이 있습니다. 설정 편집 중 자동 갱신은 입력을 덮어쓰지 않습니다. 다른 관리 화면에서 상태를 변경한 경우 이전 설정을 자동 재전송하지 않고 다시 확인하도록 안내합니다.

| 플랫폼  | 데이터 기본 경로                                 | 서버 자동 실행                                       |
| ------- | ------------------------------------------------ | ---------------------------------------------------- |
| Windows | `%LOCALAPPDATA%/RisuNestSync`                    | 현재 사용자 Task Scheduler, 로그인 트리거, 일반 권한 |
| macOS   | `~/Library/Application Support/RisuNestSync`     | `~/Library/LaunchAgents`의 사용자 LaunchAgent        |
| Linux   | `${XDG_DATA_HOME:-~/.local/share}/risunest-sync` | `systemd --user`                                     |

Windows에서는 자동 실행을 등록한 뒤 시작합니다. 등록된 작업을 통해 실행하며 별도 직접 실행으로 우회하지 않습니다. 설치 위치가 바뀌면 소유한 기존 작업을 상태에 표시하고 다시 등록하거나 제거할 수 있지만, 이전 실행 경로의 작업을 시작하지는 않습니다. 자동 실행 해제는 현재 프로세스의 중지와 별개입니다. GUI 트레이 자동 실행도 서버 자동 실행과 독립적입니다. macOS는 LaunchAgent plist를 제거해 다음 로그인 실행을 해제하며, 현재 로그인 세션에 로드된 작업은 로그아웃까지 남습니다. GUI 앱 이동 후에는 실행 등록을 다시 확인하세요.

Linux 배포 압축을 푼 뒤 `sh install.sh`를 실행하면 `~/.local/lib/risunest-sync`와 `~/.local/bin`에 설치하고 자동 실행 선택과 TUI로 이어집니다. sudo·방화벽 변경·자동 linger 활성화는 하지 않습니다. systemd 사용자 세션이 없는 환경에서는 자동 실행 등록 오류를 표시합니다. 비-systemd 전용 init 시스템 어댑터는 제공하지 않습니다.

`prepare-update`는 자신의 데이터 경로에 인증된 데몬을 정상 종료합니다. `uninstall`은 정상 종료 후 해당 사용자 실행 등록과 GUI 자동 실행을 제거합니다. 두 명령 모두 서버 데이터를 삭제하지 않습니다. 연결할 수 없는 관리 파일은 같은 locator로 두 번 확인한 뒤 실행 중인 데몬이 없는 것으로 처리하고 다음 서버 시작이 덮어쓰도록 보존합니다. locator가 바뀌거나 인증할 수 없으면 다른 프로세스를 추측해서 종료하지 않습니다.

## 관리 API

데몬은 sync listener와 별도로 `127.0.0.1`의 임의 포트에 관리 API를 엽니다. `management-session`은 사용자 전용 credential 파일이며 Windows DPAPI 또는 Unix 0600으로 보호됩니다. 관리 클라이언트는 새 작업마다 파일을 읽고 프록시·리다이렉트 없이 연결합니다. 기기 sync token으로는 관리할 수 없으며 브라우저 Origin 요청도 거부합니다.

`GET /status`, `POST /connection`, `POST /devices`, `POST /devices/{id}/revoke`, `POST /tunnel/{start,stop,restart}`, `POST /registry/repost`, `POST /shutdown`을 제공합니다. 조작은 현재 `revision`을 요구하며 오래된 요청은 409입니다. 기기 발급에는 64자리 hex `requestId`와 1~80자 `name`을 요구합니다. 같은 발급 요청을 다시 실행해 token을 재출력하거나 새 기기를 만들지 않습니다.

이 변경의 저장소 schema는 7입니다. 출시 전 정책에 따라 이전 RisuNest schema·connection-state를 자동 변환하지 않으며 `incompatible-store` 등으로 거부합니다. 기존 데이터를 지우거나 다시 초기화하는 설치 로직은 없습니다.

파일 용량은 데이터 폴더의 파일 길이 합계입니다. 별도 백업 폴더는 제외하고 DB/WAL·임시 파일·객체·기타를 구분합니다. 60초 간격으로 제한된 백그라운드 측정을 하며 실패하면 마지막 값과 측정 시각을 유지합니다. 물리 할당량이나 서버 할당 quota와는 다릅니다.

## 개발·검증

모든 Cargo 명령은 checkout과 worktree가 공유하는 `CARGO_TARGET_DIR`을 사용하세요. 이 저장소의 Windows 공유 경로는 `E:/Programming/Github/RisuNest/src-tauri/target`입니다.

```sh
cargo test --manifest-path server/sync/Cargo.toml --locked
cargo test --manifest-path server/manager/Cargo.toml --locked
cd server/manager/gui
pnpm install --frozen-lockfile
pnpm run check
pnpm run test
pnpm run build:agent
pnpm run tauri -- build --debug --no-bundle
```

GUI만 `pnpm run dev:agent`로 열면 네이티브 IPC가 없어 연결 안 됨으로 표시됩니다. 합성 화면 확인은 별도 `tests/preview/vite.config.ts` 진입점으로 실행하며 제품 번들에 포함하지 않습니다. CLI 실제 프로세스 검증은 `node server/manager/tests/daemon-process.mjs ABSOLUTE_SERVER ABSOLUTE_MANAGER`입니다. Windows 작업 등록 검증은 `tests/windows-startup.ps1`이며 합성 전용 작업을 등록·조회·제거합니다. Linux는 일반 사용자로 `python3 server/manager/tests/linux-process.py ABSOLUTE_SERVER ABSOLUTE_MANAGER`를 실행합니다. 고유 임시 데이터와 HOME에서 사용자 systemd, 번호/방향키 TUI, 로컬 sh 설치와 데이터 보존을 검증하며 테스트 서비스를 정리합니다.

## 배포 빌드

`install/package.mjs`는 현재 OS의 release 바이너리를 빌드하고 Windows NSIS, macOS app/DMG, Linux tar.gz를 준비합니다. 먼저 GUI 의존성을 설치하고 다음 환경 변수를 제공하세요.

- `CARGO_TARGET_DIR`: 공용 Cargo target
- `RISUNEST_DEFAULT_REGISTRY_URL`: 실제 기본 HTTPS 레지스트리 주소
- `RISUNEST_CLOUDFLARED`: 해당 OS/아키텍처의 검증된 Cloudflare 실행 파일
- `RISUNEST_CLOUDFLARED_SHA256`: 공식 배포본의 SHA-256
- `RISUNEST_CLOUDFLARED_LICENSE`: 해당 버전의 라이선스 파일

저장소 루트에서 `node server/manager/install/package.mjs`를 실행합니다. 기본 레지스트리를 비워 둔 개발 빌드는 직접 주소를 입력할 수 있지만 정식 패키징 명령은 기본 주소가 없으면 중단합니다. 예시 주소를 제품 기본값으로 사용하지 않습니다.

Windows NSIS는 currentUser 설치로 데몬·GUI·CLI·cloudflared를 배치하고 서버 작업을 등록·시작합니다. 설치용 서버는 `windows-background` feature로 콘솔 창 없이 실행하며 기본 서버 개발 빌드는 console CLI를 유지합니다. 제거 전 정상 종료와 자신의 실행 등록 제거가 실패하면 파일 삭제를 진행하지 않습니다. macOS는 사용자 Applications 디렉터리에 앱을 배치한 뒤 GUI에서 자동 실행을 설정합니다. 서명·공증은 배포 환경에서 수행하며 macOS 투명 창의 private API 사용은 Mac App Store 배포를 대상으로 하지 않습니다.

현재 검증과 아직 실행하지 않은 OS/설치 검증은 main checkout의 `docs/superpowers/plans/2026-09-13-sync-server-manager.md`에 기록합니다. 빌드 성공을 로그인·로그아웃·절전·배터리·macOS/Linux 실기기 검증으로 대체하지 않습니다.

공식 참조: [Tauri Windows 설치](https://v2.tauri.app/distribute/windows-installer/), [외부 실행 파일](https://v2.tauri.app/develop/sidecar/), [Apple LaunchAgent](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html).
