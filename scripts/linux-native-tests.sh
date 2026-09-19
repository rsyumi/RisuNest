#!/usr/bin/env bash
set -euo pipefail
: "${CARGO_TARGET_DIR:?Use the existing shared Cargo target directory}"
: "${DBUS_SESSION_BUS_ADDRESS:?Run this script inside dbus-run-session}"
profile=$(mktemp -d /tmp/risunest-linux-tests.XXXXXX)
trap 'rm -rf -- "$profile"' EXIT
export XDG_DATA_HOME="$profile/data"
export XDG_CONFIG_HOME="$profile/config"
export XDG_CACHE_HOME="$profile/cache"
mkdir -m 700 -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$profile/control"
printf '%s' 'risunest-synthetic-tests-only' | gnome-keyring-daemon --unlock --components=secrets --control-directory="$profile/control"
cargo test --manifest-path src-tauri/Cargo.toml --locked "$@"
