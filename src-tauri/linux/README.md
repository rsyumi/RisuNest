# Linux desktop packages

The initial build baseline is Ubuntu 24.04 LTS, x86_64. Both DEB and AppImage
use the Svelte/WebKitGTK UI and the native Rust store, save transport and regex
paths. The DEB requires glibc 2.39 or newer and Ubuntu's OpenSSL 3 package.
AppImage does not guarantee compatibility with older glibc versions.

## Build on Linux

Use Node.js 24, pnpm 10.34.1 and Rust 1.97.1 (the CI toolchain), then install:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libwebkit2gtk-4.1-dev \
  libssl-dev libdbus-1-dev libayatana-appindicator3-dev librsvg2-dev patchelf \
  desktop-file-utils shared-mime-info libfuse2t64 xdg-utils dbus-x11 gnome-keyring
pnpm install --frozen-lockfile
export CARGO_TARGET_DIR=/absolute/path/to/shared/src-tauri/target
pnpm linux:build
```

All worktrees must use the same Cargo target directory. Do not install Linux
dependencies into a Windows `node_modules` directory or junction. Use separate
host dependencies when building from WSL.

Outputs are under `$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/bundle/`.
The Linux override adds a desktop launcher, icons and MIME definitions for
`.risum`, `.risup`, `.charx`, `.risunest` and `.risudat`. It also registers
`risunestlocal:` URLs. Generic `.bin` files are not associated. AppImage contains
these desktop resources but does not install them into the host desktop.

```sh
sudo apt-get install ./RisuNest_1.0.0_amd64.deb
chmod +x ./RisuNest_1.0.0_amd64.AppImage
./RisuNest_1.0.0_amd64.AppImage
```

AppImage needs FUSE 2 for normal execution. On hosts without FUSE, its
`--appimage-extract-and-run` option is available. No WebKit sandbox override is
required by the build configuration. Updates are manual package replacements;
no update service or release publication is configured.

Native credential storage needs an unlocked Secret Service provider, such as
GNOME Keyring, in the user's desktop session. Headless sessions need their own
provider setup; the test script starts one only for its synthetic profile.

## Verification

Agents must use `pnpm linux:build:agent` so Realm endpoints remain blocked.
The `Linux package check` workflow builds these verification packages and
uploads artifacts without publishing a release.

```sh
pnpm check
dbus-run-session -- bash scripts/linux-native-tests.sh
pnpm linux:build:agent
```

For an installed **agent** DEB, install `xvfb` and `webkit2gtk-driver`, then run
the smoke test as a regular user in a private D-Bus session. The runner creates
a synthetic XDG profile and verifies the native identifier and data path before
inspecting the UI. Never point it at real app data or a normal unblocked build.

```sh
dbus-run-session -- xvfb-run -a python3 scripts/linux-package-smoke.py \
  --binary /usr/bin/RisuNest --output /tmp/risunest-package-results --phase first
# Reinstall the same agent DEB, then repeat with --phase reinstall.
# Use the agent AppImage path and --phase appimage to check format switching.
```

The test verifies initial setup, empty character creation, exact native catalog
and key/value persistence, cold/warm URL delivery, installed desktop URL
delivery, and canonical delivery of a synthetic file URL containing Unicode,
spaces, `#` and `%`. The file fixture deliberately has invalid card content;
this checks association delivery, not character import correctness.

WSL/Xvfb checks do not replace clean VM installation, GNOME/KDE Wayland/X11,
physical GPU, Korean IME, audio, accessibility, or other distribution validation.
