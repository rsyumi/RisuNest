<div align="center">
  <p><img src="public/wordmark.svg" width="500px" /></p>
  <h1>RisuNest</h1>
  <p><strong>A native RisuAI fork for Windows, macOS, Linux, Android, and iOS.</strong></p>

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
    <a href="README.ko.md">한국어</a> ·
    English
  </p>
</div>

## Introduction

RisuNest is an AI chat application forked from [RisuAI](https://github.com/kwaroran/RisuAI), available for Windows, macOS, Linux, Android, and iOS. Chat with different AI providers, manage characters and chats, and use themes and plugins.

> RisuNest is an independent community project unaffiliated with the upstream RisuAI developer and is not officially approved or endorsed by RisuAI.

## Downloads

<!-- release-downloads:app:start -->
Download [RisuNest v1.4.0](https://github.com/rsyumi/RisuNest/releases/tag/app-v1.4.0) for your operating system and CPU architecture.

| Platform | aarch64 (arm64) | x86_64 (x64) |
| --- | --- | --- |
| Windows | [EXE](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-windows-aarch64-setup.exe) | [EXE](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-windows-x86_64-setup.exe) |
| macOS | [DMG](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-darwin-aarch64.dmg) | [DMG](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-darwin-x86_64.dmg) |
| Linux | [DEB](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-aarch64.deb) / [AppImage](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-aarch64.AppImage) | [DEB](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-x86_64.deb) / [AppImage](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-linux-x86_64.AppImage) |
| Android | [APK](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-android-aarch64.apk) | - |
| iOS | [IPA](https://github.com/rsyumi/RisuNest/releases/download/app-v1.4.0/RisuNest-1.4.0-ios-aarch64-unsigned.ipa) | - |
<!-- release-downloads:app:end -->

- iOS builds are distributed as unsigned IPAs and must be sideloaded using tools such as AltStore or Sideloadly.
- To build from source yourself, see [Building from Source](#building-from-source).

Add the [iOS source](https://raw.githubusercontent.com/rsyumi/RisuNest/main/alt-source.json) to AltStore Classic, SideStore, or Feather to receive release updates:

<a href="https://altdirect.app/?url=https://raw.githubusercontent.com/rsyumi/RisuNest/main/alt-source.json"><img src="https://altdirect.app/assets/png/AltSource_Blue.png" alt="Add AltSource via AltDirect" width="200"></a>

[Add to AppDB](https://appdb.to/repos/import?url=https://raw.githubusercontent.com/rsyumi/RisuNest/main/alt-source.json)

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

- **Local Library:** Store characters, chats, and media on your device.
- **Chat Customization:** Use lorebooks, regex scripts, Lua scripts, and Plugin API v3 plugins.
- **Multi-Device Sync:** Share your library across devices through RisuNest Sync.
- **Cloud Storage:** Back up and restore your data with WebDAV, S3, or Google Drive.
- **Desktop and Mobile:** Use the app on Windows, macOS, Linux, Android, and iOS.

However, RisuNest modifies or deprecates certain upstream RisuAI features and does not guarantee 100% backward compatibility. If you encounter a compatibility issue, please open an issue. In particular, legacy Plugin API v2/2.1 plugins are not supported; please use plugins updated for Plugin API v3.

## RisuNest Sync

RisuNest Sync is a **personal synchronization server** that allows multiple devices to share the same library seamlessly.

- The Sync server runs as an independent Rust daemon available for Windows, macOS, and Linux.
- Windows and macOS include a management GUI, while Linux provides a CLI/TUI.
- When using dynamic or ephemeral addresses, a built-in endpoint registry ensures clients can automatically rediscover the server when its IP or domain changes.

### Downloads

<!-- release-downloads:sync:start -->
Download [RisuNest Sync v1.1.2](https://github.com/rsyumi/RisuNest/releases/tag/sync-v1.1.2). Sync is released independently from the app.

| Platform | aarch64 (arm64) | x86_64 (x64) |
| --- | --- | --- |
| Windows | [EXE](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-windows-aarch64-setup.exe) | [EXE](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-windows-x86_64-setup.exe) |
| macOS | [DMG](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-darwin-aarch64.dmg) | [DMG](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-darwin-x86_64.dmg) |
| Linux | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-linux-aarch64.tar.gz) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-managed-linux-x86_64.tar.gz) |
| Windows (raw) | [ZIP](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-windows-aarch64.zip) | [ZIP](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-windows-x86_64.zip) |
| macOS (raw) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-darwin-aarch64.tar.gz) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-darwin-x86_64.tar.gz) |
| Linux (raw) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-linux-aarch64.tar.gz) | [TAR.GZ](https://github.com/rsyumi/RisuNest/releases/download/sync-v1.1.2/RisuNest-Sync-1.1.2-raw-linux-x86_64.tar.gz) |
<!-- release-downloads:sync:end -->

- `managed` packages are the recommended distribution. They bundle the Sync server, the management GUI (Windows and macOS) or the `risunest-sync-manager` CLI/TUI (Linux), the updater, and `cloudflared`.
- On Linux, extract the archive and run the included `install.sh`. It installs Sync under your home directory and registers a user service, so systemd is required.
- `raw` archives contain only the `risunest-sync-server` daemon for manual administration. See [server/sync/README.md](server/sync/README.md).
- The macOS Sync app is unsigned as well. See [Opening on macOS](#opening-on-macos).

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
