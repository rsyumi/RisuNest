<div align="center">
  <p><img src="public/wordmark.svg" width="500px" /></p>
  <h1>RisuNest</h1>
  <p><strong>Windows · macOS · Linux · Android · iOS 용 RisuAI 네이티브 포크 프로젝트.</strong></p>

  [![RisuAI 2026.8.250](https://img.shields.io/badge/RisuAI-2026.8.250-blue)](https://github.com/kwaroran/RisuAI)
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

RisuNest는 [RisuAI](https://github.com/kwaroran/RisuAI)를 포크해 개발된 **네이티브 앱** 프로젝트입니다. RisuAI의 기능은 최대한 그대로 유지하면서, 네이티브 전용 기술과 최적화를 도입하여 다양한 플랫폼에서 효율적으로 실행될 수 있도록 설계되었습니다.

> RisuNest는 RisuAI 개발자와 무관한 커뮤니티 프로젝트이며, RisuAI 측의 공식 승인이나 보증을 받지 않았습니다.

## 다운로드

> [!NOTE]
> RisuNest는 아직 첫 정식 릴리스를 공개하지 않았습니다. 아래 표는 릴리스 파이프라인이 만드는 파일 이름이며, 공개 후 [Releases](https://github.com/rsyumi/RisuNest/releases)에서 받을 수 있습니다.

| 플랫폼 | aarch64 (arm64) | x86_64 (x64) |
| --- | --- | --- |
| Windows | `RisuNest-<버전>-windows-aarch64-setup.exe` | `RisuNest-<버전>-windows-x86_64-setup.exe` |
| macOS | `RisuNest-<버전>-darwin-aarch64.dmg` | `RisuNest-<버전>-darwin-x86_64.dmg` |
| Linux | `RisuNest-<버전>-linux-aarch64.deb` / `.AppImage` | `RisuNest-<버전>-linux-x86_64.deb` / `.AppImage` |
| Android | `RisuNest-<버전>-android-aarch64.apk` | - |
| iOS | `RisuNest-<버전>-ios-aarch64-unsigned.ipa` | - |

- iOS는 서명되지 않은 IPA로 배포하므로 AltStore, Sideloadly 같은 도구로 직접 설치해야 합니다.
- 직접 빌드하려면 [소스에서 빌드](#소스에서-빌드)를 참고하세요.

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

RisuNest는 공식적으로 웹 빌드를 지원하지 않고 있으며, 많은 부분이 Rust로 구동되도록 설계되었습니다. 주요 특징은 다음과 같습니다.

- Rust SQLite 저장소를 사용하여 DB를 효율적으로 관리합니다.
- 네이티브 파일 시스템을 사용해 에셋과 미디어 파일을 빠르게 저장합니다.
- 정규식, Lua, 렌더링 최적화, 스트리밍, 로어북, 통신 등 다양한 부분에서 WebView와 Rust 모두 최적화가 이루어져 있습니다.
- RisuNest Sync 서버를 통해 여러 기기에서 사용할 수 있습니다.
- WebDAV, S3, Google Drive 등 외부 저장소와의 백업 및 동기화를 지원하여 서버가 없더라도 여러 기기에 걸쳐 사용하기 쉽습니다.
- iOS와 Android에서 네이티브 앱으로 실행할 수 있으며, 모바일 환경에서도 최적화된 성능을 제공합니다.

단 RisuNest는 RisuAI의 일부 기능을 제거하거나 변경했으며, 100% 호환성을 보장하지 않습니다. 호환성 문제가 있다면 이슈를 열어 주시기 바랍니다. 특히 Plugin API v2/2.1 플러그인은 지원하지 않으므로, Plugin API v3 버전을 사용해주세요.

## RisuNest Sync

RisuNest Sync는 여러 기기가 같은 라이브러리를 공유하도록 돕는 **개인 동기화 서버**입니다.

- Sync 서버는 독립적으로 실행되는 Rust 데몬입니다. Windows, macOS, Linux를 모두 지원합니다.
- Windows와 macOS에는 관리 GUI가, Linux에는 CLI/TUI가 제공됩니다.
- 임시 주소를 사용하는 경우, 주소가 바뀌어도 앱이 다시 찾을 수 있도록 엔드포인트 레지스트리를 함께 제공합니다.

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
