# Linux desktop packages

RisuNest publishes DEB and AppImage packages for x86_64 and aarch64. The initial build baseline is Ubuntu 24.04 LTS. Both formats use the Svelte/WebKitGTK UI and native Rust storage paths.

DEB packages require glibc 2.39 or newer and Ubuntu's OpenSSL 3 package. AppImage does not guarantee compatibility with older glibc versions.

## Build

Use the CI toolchain, Node.js 24, pnpm 11, and Rust 1.97.1. Install the platform dependencies:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libwebkit2gtk-4.1-dev \
  libssl-dev libdbus-1-dev libayatana-appindicator3-dev librsvg2-dev patchelf \
  desktop-file-utils shared-mime-info libfuse2t64 xdg-utils dbus-x11 gnome-keyring
pnpm install --frozen-lockfile
export CARGO_TARGET_DIR=/absolute/path/to/RisuNest/src-tauri/target
pnpm linux:build
```

All worktrees share one Cargo target. WSL must use Linux-installed dependencies instead of a Windows node_modules directory or junction.

Packages are written below `$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/bundle/`.

## Desktop integration

The DEB installs desktop launchers, icons, MIME definitions for RisuNest file formats, and the `risunestlocal:` URL handler. Generic `.bin` files are not associated.

AppImage contains the same desktop resources but does not install them. Register its URL handler from **Settings > RisuNest > Platform > AppImage links** and register again after moving the file. Development and agent builds cannot register it.

AppImage normally requires FUSE 2. Use `--appimage-extract-and-run` on hosts without FUSE. Native credential storage requires an unlocked Secret Service provider such as GNOME Keyring.

## Verification

Use the agent build for isolated package tests:

```sh
pnpm check
dbus-run-session -- bash scripts/linux-native-tests.sh
pnpm linux:build:agent
```

For package smoke testing, use an agent package and the runner's synthetic XDG profile.

WSL/Xvfb results do not replace clean-VM installation, GNOME and KDE, Wayland and X11, physical GPU, Korean IME, audio, accessibility, or distribution validation.
