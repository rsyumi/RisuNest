<div align="center">
  <p><img src="public/wordmark.svg" width="500px" /></p>
  <h1>RisuNest</h1>
  <p><strong>A native RisuAI fork for Windows, macOS, Linux, Android, and iOS.</strong></p>

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
    <a href="README.ko.md">한국어</a> ·
    English
  </p>
</div>

## Introduction

RisuNest is a cross-platform native application forked from [RisuAI](https://github.com/kwaroran/RisuAI). It is designed to preserve upstream functionality while introducing native runtime components and low-level optimizations for high performance across diverse platforms.

> RisuNest is an independent community project unaffiliated with the upstream RisuAI developer and is not officially approved or endorsed by RisuAI.

## Downloads

> [!NOTE]
> RisuNest has not yet published its first stable release. The table below lists the binary filenames produced by the release pipeline. Once published, they will be available on [Releases](https://github.com/rsyumi/RisuNest/releases).

| Platform | aarch64 (arm64) | x86_64 (x64) |
| --- | --- | --- |
| Windows | `RisuNest-<version>-windows-aarch64-setup.exe` | `RisuNest-<version>-windows-x86_64-setup.exe` |
| macOS | `RisuNest-<version>-darwin-aarch64.dmg` | `RisuNest-<version>-darwin-x86_64.dmg` |
| Linux | `RisuNest-<version>-linux-aarch64.deb` / `.AppImage` | `RisuNest-<version>-linux-x86_64.deb` / `.AppImage` |
| Android | `RisuNest-<version>-android-aarch64.apk` | - |
| iOS | `RisuNest-<version>-ios-aarch64-unsigned.ipa` | - |

- iOS builds are distributed as unsigned IPAs and must be sideloaded using tools such as AltStore or Sideloadly.
- To build from source yourself, see [Building from Source](#building-from-source).

### Build Integrity & Security

- Release binaries are not signed with a trusted certificate, so Windows, macOS, and Android may display security warnings.
- Every release is built via `.github/workflows/stable-release.yml`, and public build logs are available on the [Actions](https://github.com/rsyumi/RisuNest/actions) tab.
- Each release includes a cryptographically signed `manifest.json`. The app and Sync server updaters only download files after verifying this signature.
- Never install or run builds from untrusted sources.

### Opening on macOS

Refer to Apple's [official guide on opening unnotarized apps](https://support.apple.com/en-us/102445#openanyway). If macOS blocks the app on first launch, go to **System Settings > Privacy & Security** and click **Open Anyway**, or remove the quarantine flag via Terminal:

```bash
xattr -dr com.apple.quarantine /Applications/RisuNest.app
```

## Key Features

RisuNest intentionally focuses on native platforms rather than web builds, delegating critical operations to a high-performance Rust core. Key features include:

- **Robust Local Storage:** High-performance database operations powered by an embedded SQLite layer in Rust.
- **Fast Media I/O:** Rapid asset and media caching leveraging direct native filesystem access.
- **Full-Stack Optimization:** Targeted optimizations across both the WebView UI and Rust backend for regex processing, Lua scripting, rendering, stream decoding, lorebooks, and networking.
- **Multi-Device Sync:** Seamless synchronization across devices powered by the dedicated RisuNest Sync server.
- **Cloud Storage Integration:** Backup and restore support for external cloud storage (WebDAV, S3, Google Drive) without needing a self-hosted sync server.
- **Native Mobile Support:** First-class native apps on iOS and Android with optimized memory footprint and responsiveness.

However, RisuNest modifies or deprecates certain upstream RisuAI features and does not guarantee 100% backward compatibility. If you encounter a compatibility issue, please open an issue. In particular, legacy Plugin API v2/2.1 plugins are not supported; please use plugins updated for Plugin API v3.

## RisuNest Sync

RisuNest Sync is a **personal synchronization server** that allows multiple devices to share the same library seamlessly.

- The Sync server runs as an independent Rust daemon available for Windows, macOS, and Linux.
- Windows and macOS include a management GUI, while Linux provides a CLI/TUI.
- When using dynamic or ephemeral addresses, a built-in endpoint registry ensures clients can automatically rediscover the server when its IP or domain changes.

## Migrating from RisuAI

1. Export a backup (`.bin`) from RisuAI.
2. On the welcome screen shown when you first launch RisuNest, select **Import from a backup file**. If you are already using the app, open **Settings → RisuNest → Backup & restore → RisuSave file → Import**.
3. If you used RisuAI account backup, select **Connect to a sync server → From a RisuAI account backup** on the welcome screen.

## Building from Source

Node.js 22.12 or later, pnpm, Rust, and Cargo are required. Android builds require the Android SDK/NDK (compile and target SDK 36, min SDK 24, NDK 28), while iOS builds require macOS and Xcode.

```bash
pnpm install
pnpm tauri dev                    # Start desktop app in development mode
pnpm windows:build                # Windows
pnpm macos:build                  # macOS
pnpm linux:build                  # Linux
pnpm android:build:arm64          # Android debug APK
pnpm android:build:release:arm64  # Android release APK (signing required)
pnpm test                         # Vitest
pnpm check                        # Type checking
```

To build for iOS, ensure Xcode is installed on macOS and run:

```bash
pnpm exec tauri ios build --target aarch64 --no-sign
```

## License

RisuNest is distributed under the [GPL-3.0](LICENSE) license.

The Terms of Service and Privacy Notice are available in [legal/TERMS.md](legal/TERMS.md) and [legal/PRIVACY.md](legal/PRIVACY.md).
