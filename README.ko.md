<div align="center">
  <p><img src="public/wordmark.svg" width="500px" /></p>
  <h1>RisuNest</h1>
  <p><strong>Windows · macOS · Linux · Android · iOS 용 RisuAI 네이티브 포크 프로젝트.</strong></p>

  [![RisuAI 2026.10.292](https://img.shields.io/badge/RisuAI-2026.10.292-blue)](https://github.com/kwaroran/RisuAI)
  [![Tauri 2](https://img.shields.io/badge/Tauri-2-%2324C8D8?logo=tauri&logoColor=white)](https://tauri.app/)
  [![Svelte 5](https://img.shields.io/badge/Svelte-5-FF3E00?logo=svelte&logoColor=white)](https://svelte.dev/)
  [![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
  [![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-orange)](LICENSE)
  <br>
  [![Stable release](https://github.com/rsyumi/RisuNest/actions/workflows/stable-release.yml/badge.svg)](https://github.com/rsyumi/RisuNest/actions/workflows/stable-release.yml)
  [![Latest release](https://img.shields.io/github/v/release/rsyumi/RisuNest?label=latest)](https://github.com/rsyumi/RisuNest/releases/latest)
  [![Downloads](https://img.shields.io/github/downloads/rsyumi/RisuNest/total)](https://github.com/rsyumi/RisuNest/releases)

  <p>
    한국어 ·
    <a href="README.md">English</a>
  </p>
</div>

## 소개

RisuNest는 [RisuAI](https://github.com/kwaroran/RisuAI)를 포크한 AI 채팅 앱이며, Windows, macOS, Linux, Android, iOS에서 사용할 수 있습니다. 여러 AI 제공자와 대화하고, 캐릭터와 채팅을 관리하며, 테마와 플러그인을 사용할 수 있습니다.

> RisuNest는 RisuAI 개발자와 무관한 커뮤니티 프로젝트이며, RisuAI 측의 공식 승인이나 보증을 받지 않았습니다.

## 다운로드

<!-- release-downloads:app:start -->
[RisuNest v1.4.0](https://github.com/rsyumi/RisuNest/releases/tag/app-v1.4.0)에서 운영체제와 CPU 아키텍처에 맞는 파일을 다운로드하세요.

| 플랫폼 | aarch64 (arm64) | x86_64 (x64) |
| --- | --- | --- |
| Windows | [EXE](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-windows-aarch64-setup.exe) | [EXE](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-windows-x86_64-setup.exe) |
| macOS | [DMG](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-darwin-aarch64.dmg) | [DMG](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-darwin-x86_64.dmg) |
| Linux | [DEB](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-aarch64.deb) / [AppImage](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-aarch64.AppImage) | [DEB](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-x86_64.deb) / [AppImage](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-x86_64.AppImage) |
| Android | [APK](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-android-aarch64.apk) | - |
| iOS | [IPA](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-ios-aarch64-unsigned.ipa) | - |
<!-- release-downloads:app:end -->

- iOS는 서명되지 않은 IPA로 배포하므로 AltStore, Sideloadly 같은 도구로 직접 설치해야 합니다.
- 직접 빌드하려면 [소스에서 빌드](#소스에서-빌드)를 참고하세요.

AltStore Classic, SideStore, Feather에 [iOS 소스](https://raw.githubusercontent.com/rsyumi/RisuNest/main/alt-source.json)를 추가하면 새 릴리스를 확인할 수 있습니다.

<a href="https://altdirect.app/?url=https://raw.githubusercontent.com/rsyumi/RisuNest/main/alt-source.json"><img src="https://altdirect.app/assets/png/AltSource_Blue.png" alt="Add AltSource via AltDirect" width="200"></a>

[Add to AppDB](https://appdb.to/repos/import?url=https://raw.githubusercontent.com/rsyumi/RisuNest/main/alt-source.json)

### 빌드 신뢰

- 배포 파일은 신뢰할 수 있는 인증서로 서명되지 않으므로 Windows와 macOS, Android에서 보안 경고가 표시될 수 있습니다.
- 모든 릴리스는 `.github/workflows/stable-release.yml`로 빌드되며, 빌드 로그는 [Actions](https://github.com/rsyumi/RisuNest/actions) 탭에서 볼 수 있습니다.
- 릴리스마다 서명된 `manifest.json`이 함께 올라가며, 앱의 업데이트 확인과 Sync 서버의 업데이트 확인은 이 서명을 검증한 뒤에만 파일을 받습니다.
- 출처를 알 수 없는 빌드는 절대 설치하거나 사용하지 마세요.

### macOS에서 열기

[macOS 공식 매뉴얼](https://support.apple.com/ko-kr/102445#openanyway)을 참고해 앱을 실행한 뒤 보안 경고가 뜨면 `설정` - `개인정보 보호 및 보안` - `그래도 열기`를 눌러 실행하거나, 하단 명령어를 통해 터미널에서 격리 속성을 제거해주세요.

```bash
xattr -dr com.apple.quarantine /Applications/RisuNest.app
```

## 주요 특징

- **로컬 라이브러리:** 캐릭터, 채팅, 미디어를 기기에 저장합니다.
- **채팅 설정:** 로어북, 정규식 스크립트, Lua 스크립트, Plugin API v3 플러그인을 사용할 수 있습니다.
- **기기 간 동기화:** RisuNest Sync를 통해 여러 기기에서 라이브러리를 공유합니다.
- **클라우드 저장소:** WebDAV, S3, Google Drive로 데이터를 백업하고 복원할 수 있습니다.
- **데스크톱과 모바일:** Windows, macOS, Linux, Android, iOS에서 사용할 수 있습니다.

단 RisuNest는 RisuAI의 일부 기능을 제거하거나 변경했으며, 100% 호환성을 보장하지 않습니다. 호환성 문제가 있다면 이슈를 열어 주시기 바랍니다. 특히 Plugin API v2/2.1 플러그인은 지원하지 않으므로, Plugin API v3 버전을 사용해주세요.

## RisuNest Sync

RisuNest Sync는 여러 기기가 같은 라이브러리를 공유하도록 돕는 **개인 동기화 서버**입니다.

- Sync 서버는 독립적으로 실행되는 Rust 데몬입니다. Windows, macOS, Linux를 모두 지원합니다.
- Windows와 macOS에는 관리 GUI가, Linux에는 CLI/TUI가 제공됩니다.
- 임시 주소를 사용하는 경우, 주소가 바뀌어도 앱이 다시 찾을 수 있도록 엔드포인트 레지스트리를 함께 제공합니다.

### 다운로드

<!-- release-downloads:sync:start -->
[RisuNest Sync v1.1.2](https://github.com/rsyumi/RisuNest/releases/tag/sync-v1.1.2)을 다운로드할 수 있습니다. Sync는 앱과 별도로 릴리스됩니다.

| 플랫폼 | aarch64 (arm64) | x86_64 (x64) |
| --- | --- | --- |
| Windows | [EXE](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-windows-aarch64-setup.exe) | [EXE](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-windows-x86_64-setup.exe) |
| macOS | [DMG](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-darwin-aarch64.dmg) | [DMG](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-darwin-x86_64.dmg) |
| Linux | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-linux-aarch64.tar.gz) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-linux-x86_64.tar.gz) |
| Windows (raw) | [ZIP](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-windows-aarch64.zip) | [ZIP](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-windows-x86_64.zip) |
| macOS (raw) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-darwin-aarch64.tar.gz) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-darwin-x86_64.tar.gz) |
| Linux (raw) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-linux-aarch64.tar.gz) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-linux-x86_64.tar.gz) |
<!-- release-downloads:sync:end -->

- `managed`는 권장 배포판이며, Sync 서버와 관리 GUI(Windows·macOS) 또는 `risunest-sync-manager` CLI/TUI(Linux), 업데이트 확인, `cloudflared`를 한 패키지로 담고 있습니다.
- Linux는 아카이브를 풀고 함께 들어 있는 `install.sh`를 실행하세요. 홈 디렉터리 아래에 설치하고 사용자 서비스를 등록하므로 systemd가 필요합니다.
- `raw`는 `risunest-sync-server` 데몬만 담은 아카이브로, 직접 관리하는 경우에 사용합니다. 자세한 내용은 [server/sync/README.md](server/sync/README.md)를 참고하세요.
- macOS의 Sync 앱도 서명되지 않았으므로 [macOS에서 열기](#macos에서-열기)를 참고하세요.

## RisuAI에서 옮겨오기

1. RisuAI에서 백업을 내보냅니다(`.bin`).
2. RisuNest를 처음 실행하면 나오는 시작 화면에서 **백업 파일에서 가져오기**를 선택합니다. 이미 사용 중이라면 설정의 **RisuNest → 백업·복구 → RisuSave 파일 → 가져오기**에서 엽니다.
3. RisuAI 계정 백업을 쓰고 있었다면 시작 화면에서 **동기화 서버에 연결 → RisuAI 계정 백업에서**를 선택합니다.

## 소스에서 빌드

Node.js 22.12 이상, pnpm, Rust와 Cargo가 필요합니다. Android 빌드에는 Android SDK/NDK(compile·target SDK 36, min SDK 24, NDK 28)가, iOS 빌드에는 macOS와 Xcode가 필요합니다.

```bash
pnpm install
pnpm tauri dev                  # 데스크톱 개발 실행
pnpm windows:build              # Windows
pnpm macos:build                # macOS
pnpm linux:build                # Linux
pnpm android:build:arm64        # Android 디버그 APK
pnpm android:build:release:arm64  # Android 릴리스 APK (서명 설정 필요)
pnpm test                       # Vitest
pnpm check                      # 타입 검사
```

iOS는 macOS에서 Xcode를 설치한 뒤 빌드합니다.

```bash
pnpm exec tauri ios build --target aarch64 --no-sign
```

## 라이선스

RisuNest는 [GPL-3.0](LICENSE)으로 배포됩니다.

이용 약관과 개인정보 처리방침은 [legal/TERMS.md](legal/TERMS.md)와 [legal/PRIVACY.md](legal/PRIVACY.md)에 있습니다.
