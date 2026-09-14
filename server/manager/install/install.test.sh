#!/usr/bin/env bash
set -euo pipefail

installer=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/install.sh
test_root=$(mktemp -d)
trap 'rm -rf "$test_root"' EXIT

make_manager() {
  local path=$1 role=$2
  cat > "$path" <<EOF
#!/bin/sh
role=$role
printf '%s:%s\n' "\$role" "\$*" >> "\$RISUNEST_TEST_LOG"
if flock -n "\$RISUNEST_TEST_LOCK" -c true; then
  printf '%s\n' 'manager command ran without the installer lock' >&2
  exit 97
fi
if test "\${1:-}" = --data-dir; then shift 2; fi
if test "\${1:-}" = --server; then shift 2; fi
if test "\${1:-}:\${2:-}:\${3:-}" = 'installer:sync-stage:lock-held'; then exit 0; fi
if test "\$1" = status; then
  test "\$role" = old && test "\${RISUNEST_TEST_WAS_RUNNING:-false}" = true
  exit
fi
if test "\$1:\$2:\$3" = 'installer:swap:lock-held'; then
  stage=\$4
  was_running=\$5
  install_dir=\$(dirname -- "\$0")
  exchange="\$stage.exchange-test"
  printf '%s\n%s\n' "\$stage" "\$was_running" > "\$RISUNEST_TEST_TRANSACTION"
  /bin/mv "\$install_dir" "\$exchange"
  /bin/mv "\$stage" "\$install_dir"
  /bin/mv "\$exchange" "\$stage"
  if test "\${RISUNEST_TEST_FAIL_SWAP:-false}" = true; then exit 73; fi
  exit 0
fi
if test "\$1:\$2:\$3" = 'installer:rollback-swap:lock-held'; then
  fallback=\$4
  if test -f "\$RISUNEST_TEST_TRANSACTION"; then
    stage=\$(sed -n '1p' "\$RISUNEST_TEST_TRANSACTION")
    was_running=\$(sed -n '2p' "\$RISUNEST_TEST_TRANSACTION")
    install_dir=\$(dirname -- "\$0")
    exchange="\$stage.exchange-test"
    /bin/mv "\$install_dir" "\$exchange"
    /bin/mv "\$stage" "\$install_dir"
    /bin/mv "\$exchange" "\$stage"
    rm -rf "\$stage"
    rm -f "\$RISUNEST_TEST_TRANSACTION"
  else
    was_running=\$fallback
    install_dir=\$(dirname -- "\$0")
  fi
  if test "\$was_running" = true; then
    "\$install_dir/risunest-sync-manager" start lock-held
  fi
  exit 0
fi
if test "\$1:\$2:\$3" = 'installer:commit-swap:lock-held'; then
  stage=\$(sed -n '1p' "\$RISUNEST_TEST_TRANSACTION")
  rm -rf "\$stage"
  rm -f "\$RISUNEST_TEST_TRANSACTION"
  exit 0
fi
if test "\$role:\$*" = 'new:autostart install lock-held' && test "\${RISUNEST_TEST_FAIL_AUTOSTART:-false}" = true; then
  exit 75
fi
if test "\$role:\$*" = 'new:autostart remove lock-held' && test "\${RISUNEST_TEST_FAIL_AUTOSTART_REMOVE:-false}" = true; then
  exit 78
fi
if test "\$role:\$*" = 'new:update schedule remove lock-held' && test "\${RISUNEST_TEST_FAIL_SCHEDULE_REMOVE:-false}" = true; then
  exit 77
fi
if test "\$role:\$*" = 'new:update policy notify lock-held' && test "\${RISUNEST_TEST_FAIL_POLICY:-false}" = true; then
  exit 74
fi
if test "\$role:\$*" = 'new:installer start-and-verify lock-held'; then
  printf '%s\n' 'new:system-start-accepted' >> "\$RISUNEST_TEST_LOG"
  if test "\${RISUNEST_TEST_FAIL_START:-false}" = true; then exit 76; fi
fi
exit 0
EOF
  chmod 700 "$path"
}

make_fixture() {
  local name=$1
  case_root="$test_root/$name"
  test_home="$case_root/home"
  source="$case_root/source"
  fake_bin="$case_root/bin"
  log="$case_root/commands.log"
  lock="$test_home/.local/share/risunest-sync/manager-update/instance.lock"
  mkdir -p "$test_home" "$source" "$fake_bin"
  for name in risunest-sync-server cloudflared CLOUDFLARED-LICENSE risunest-sync-bundle.json; do
    printf 'new-%s\n' "$name" > "$source/$name"
  done
  cp "$installer" "$source/install.sh"
  make_manager "$source/risunest-sync-manager" new
  cat > "$fake_bin/id" <<'EOF'
#!/bin/sh
case "$1" in -u) printf '%s\n' 1000 ;; -un) printf '%s\n' tester ;; *) exit 2 ;; esac
EOF
  cat > "$fake_bin/loginctl" <<'EOF'
#!/bin/sh
if test "$1" = show-user; then printf '%s\n' yes; exit 0; fi
exit 0
EOF
  cat > "$fake_bin/systemctl" <<'EOF'
#!/bin/sh
exit 0
EOF
  chmod 700 "$fake_bin/id" "$fake_bin/loginctl" "$fake_bin/systemctl"
}

run_installer() {
  env \
    HOME="$test_home" \
    PATH="$fake_bin:/usr/bin:/bin" \
    RISUNEST_TEST_LOG="$log" \
    RISUNEST_TEST_LOCK="$lock" \
    RISUNEST_TEST_WAS_RUNNING="${RISUNEST_TEST_WAS_RUNNING:-false}" \
    RISUNEST_TEST_FAIL_AUTOSTART="${RISUNEST_TEST_FAIL_AUTOSTART:-false}" \
    RISUNEST_TEST_FAIL_AUTOSTART_REMOVE="${RISUNEST_TEST_FAIL_AUTOSTART_REMOVE:-false}" \
    RISUNEST_TEST_FAIL_SCHEDULE_REMOVE="${RISUNEST_TEST_FAIL_SCHEDULE_REMOVE:-false}" \
    RISUNEST_TEST_FAIL_SWAP="${RISUNEST_TEST_FAIL_SWAP:-false}" \
    RISUNEST_TEST_FAIL_START="${RISUNEST_TEST_FAIL_START:-false}" \
    RISUNEST_TEST_FAIL_POLICY="${RISUNEST_TEST_FAIL_POLICY:-false}" \
    RISUNEST_TEST_TRANSACTION="$case_root/transaction" \
    /bin/sh "$source/install.sh" --non-interactive --policy=notify
}

make_fixture success
run_installer >/dev/null
test "$(cat "$test_home/.local/lib/risunest-sync/risunest-sync-server")" = new-risunest-sync-server
grep -q '^new:--data-dir .* installer sync-stage lock-held ' "$log"
grep -qx 'new:update policy notify lock-held' "$log"
grep -qx 'new:autostart install lock-held' "$log"
grep -qx 'new:installer start-and-verify lock-held' "$log"

make_fixture swap-failure
mkdir -p "$test_home/.local/lib/risunest-sync" "$test_home/.local/bin"
printf '%s\n' old-server > "$test_home/.local/lib/risunest-sync/risunest-sync-server"
make_manager "$test_home/.local/lib/risunest-sync/risunest-sync-manager" old
cat > "$test_home/.local/bin/risunest-sync-manager" <<'EOF'
#!/bin/sh
# RisuNest sync manager launcher
exit 0
EOF
chmod 700 "$test_home/.local/bin/risunest-sync-manager"
export RISUNEST_TEST_WAS_RUNNING=true
export RISUNEST_TEST_FAIL_SWAP=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'swap failure unexpectedly succeeded' >&2
  exit 1
fi
unset RISUNEST_TEST_FAIL_SWAP
test "$(cat "$test_home/.local/lib/risunest-sync/risunest-sync-server")" = old-server
grep -qx 'old:prepare-update lock-held' "$log"
grep -q '^old:installer swap lock-held ' "$log"
grep -qx 'new:installer rollback-swap lock-held true' "$log"
grep -qx 'old:start lock-held' "$log"
if find "$test_home/.local/lib" -maxdepth 1 -name '.risunest-sync-update-installer-stage.*' | grep -q .; then
  printf '%s\n' 'installer transaction files were not cleaned' >&2
  exit 1
fi

make_fixture staging-failure
mkdir -p "$test_home/.local/lib/risunest-sync"
printf '%s\n' old-server > "$test_home/.local/lib/risunest-sync/risunest-sync-server"
make_manager "$test_home/.local/lib/risunest-sync/risunest-sync-manager" old
cat > "$fake_bin/install" <<'EOF'
#!/bin/sh
case "$*" in *'/cloudflared '*) exit 74 ;; esac
exec /usr/bin/install "$@"
EOF
chmod 700 "$fake_bin/install"
export RISUNEST_TEST_WAS_RUNNING=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'staging failure unexpectedly succeeded' >&2
  exit 1
fi
test "$(cat "$test_home/.local/lib/risunest-sync/risunest-sync-server")" = old-server
test ! -e "$log"

make_fixture finalize-failure
mkdir -p "$test_home/.local/lib/risunest-sync"
printf '%s\n' old-server > "$test_home/.local/lib/risunest-sync/risunest-sync-server"
make_manager "$test_home/.local/lib/risunest-sync/risunest-sync-manager" old
export RISUNEST_TEST_WAS_RUNNING=true
export RISUNEST_TEST_FAIL_AUTOSTART=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'finalize failure unexpectedly succeeded' >&2
  exit 1
fi
unset RISUNEST_TEST_FAIL_AUTOSTART
test "$(cat "$test_home/.local/lib/risunest-sync/risunest-sync-server")" = old-server
grep -qx 'new:update policy notify lock-held' "$log"
grep -qx 'new:autostart install lock-held' "$log"
grep -qx 'new:installer rollback-swap lock-held true' "$log"
grep -qx 'old:start lock-held' "$log"

make_fixture upgrade-success
mkdir -p "$test_home/.local/lib/risunest-sync"
printf '%s\n' old-server > "$test_home/.local/lib/risunest-sync/risunest-sync-server"
make_manager "$test_home/.local/lib/risunest-sync/risunest-sync-manager" old
export RISUNEST_TEST_WAS_RUNNING=true
run_installer >/dev/null
test "$(cat "$test_home/.local/lib/risunest-sync/risunest-sync-server")" = new-risunest-sync-server
grep -q '^old:installer swap lock-held .* true$' "$log"
grep -qx 'new:installer start-and-verify lock-held' "$log"
grep -qx 'new:installer commit-swap lock-held' "$log"
test ! -e "$case_root/transaction"

make_fixture upgrade-health-failure
mkdir -p "$test_home/.local/lib/risunest-sync"
printf '%s\n' old-server > "$test_home/.local/lib/risunest-sync/risunest-sync-server"
make_manager "$test_home/.local/lib/risunest-sync/risunest-sync-manager" old
export RISUNEST_TEST_WAS_RUNNING=true
export RISUNEST_TEST_FAIL_START=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'upgrade health failure unexpectedly succeeded' >&2
  exit 1
fi
unset RISUNEST_TEST_FAIL_START
test "$(cat "$test_home/.local/lib/risunest-sync/risunest-sync-server")" = old-server
grep -qx 'new:installer start-and-verify lock-held' "$log"
grep -qx 'new:system-start-accepted' "$log"
grep -qx 'new:installer rollback-swap lock-held true' "$log"
grep -qx 'old:start lock-held' "$log"
if grep -qx 'new:installer commit-swap lock-held' "$log"; then
  printf '%s\n' 'unhealthy upgrade committed its transaction' >&2
  exit 1
fi

make_fixture fresh-start-failure
export RISUNEST_TEST_WAS_RUNNING=false
export RISUNEST_TEST_FAIL_START=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'fresh start failure unexpectedly succeeded' >&2
  exit 1
fi
unset RISUNEST_TEST_FAIL_START
grep -qx 'new:autostart install lock-held' "$log"
grep -qx 'new:installer start-and-verify lock-held' "$log"
grep -qx 'new:system-start-accepted' "$log"
grep -qx 'new:stop' "$log"
grep -qx 'new:update schedule remove lock-held' "$log"
grep -qx 'new:autostart remove lock-held' "$log"
test ! -e "$test_home/.local/lib/risunest-sync"
test ! -e "$test_home/.local/bin/risunest-sync-manager"

make_fixture fresh-policy-failure
export RISUNEST_TEST_WAS_RUNNING=false
export RISUNEST_TEST_FAIL_POLICY=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'fresh policy failure unexpectedly succeeded' >&2
  exit 1
fi
unset RISUNEST_TEST_FAIL_POLICY
grep -qx 'new:update policy notify lock-held' "$log"
grep -qx 'new:update schedule remove lock-held' "$log"
grep -qx 'new:autostart remove lock-held' "$log"
test ! -e "$test_home/.local/lib/risunest-sync"
test ! -e "$test_home/.local/bin/risunest-sync-manager"

make_fixture fresh-cleanup-failure
export RISUNEST_TEST_WAS_RUNNING=false
export RISUNEST_TEST_FAIL_START=true
export RISUNEST_TEST_FAIL_SCHEDULE_REMOVE=true
export RISUNEST_TEST_FAIL_AUTOSTART_REMOVE=true
if run_installer >/dev/null 2>&1; then
  printf '%s\n' 'fresh cleanup failure unexpectedly succeeded' >&2
  exit 1
fi
unset RISUNEST_TEST_FAIL_START RISUNEST_TEST_FAIL_SCHEDULE_REMOVE RISUNEST_TEST_FAIL_AUTOSTART_REMOVE
grep -qx 'new:update schedule remove lock-held' "$log"
grep -qx 'new:autostart remove lock-held' "$log"
test -x "$test_home/.local/lib/risunest-sync/risunest-sync-manager"
test -x "$test_home/.local/bin/risunest-sync-manager"

printf '%s\n' 'Linux installer transaction tests passed.'
