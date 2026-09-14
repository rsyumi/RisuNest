#!/bin/sh
set -eu

if test "$#" -ne 3; then
  printf '%s\n' "usage: $0 <app.tar.gz> <app.tar.gz.sig> <inventory-sync_darwin_ARCH.json>" >&2
  exit 2
fi
: "${RISUNEST_UPDATE_PUBLIC_KEY:?RISUNEST_UPDATE_PUBLIC_KEY is required}"
: "${RISUNEST_TEST_RUST_TARGET:?RISUNEST_TEST_RUST_TARGET is required}"
: "${CARGO_TARGET_DIR:?CARGO_TARGET_DIR is required}"
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
test "${GITHUB_ACTIONS:-}" = true || {
  printf '%s\n' "macOS whole-app harness requires an ephemeral GitHub Actions user" >&2
  exit 2
}

absolute_file() {
  directory=$(dirname -- "$1")
  name=$(basename -- "$1")
  printf '%s/%s\n' "$(CDPATH= cd -- "$directory" && pwd -P)" "$name"
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo=$(CDPATH= cd -- "$script_dir/../../.." && pwd -P)
export RISUNEST_TEST_MAC_APP_ARCHIVE=$(absolute_file "$1")
export RISUNEST_TEST_MAC_APP_ARCHIVE_SIGNATURE=$(absolute_file "$2")
export RISUNEST_TEST_MAC_BUILD_INVENTORY=$(absolute_file "$3")
export RISUNEST_TEST_UPDATE_PUBLIC_KEY=$RISUNEST_UPDATE_PUBLIC_KEY

cd "$repo"
cargo test \
  --manifest-path server/manager/Cargo.toml \
  --locked \
  --release \
  --target "$RISUNEST_TEST_RUST_TARGET" \
  --test macos_update \
  produced_whole_app_helper_commits_and_rolls_back \
  -- \
  --ignored \
  --exact \
  --nocapture \
  --test-threads=1
