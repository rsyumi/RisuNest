#!/usr/bin/env bash
# Exercises the produced managed archive through the real installer, manager, daemon, and user systemd.
set -euo pipefail

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

test "$#" -eq 1 || fail 'usage: managed-install.integration.test.sh MANAGED_TAR_GZ'
test "$(uname -s)" = Linux || fail 'run this integration test on Linux'
test "$(id -u)" -ne 0 || fail 'run this integration test as the service user, not root'
for command in cmp flock grep loginctl python3 realpath sed stat systemctl tar; do
  command -v "$command" >/dev/null 2>&1 || fail "required command is unavailable: $command"
done

managed_archive=$(realpath "$1")
test -f "$managed_archive" || fail 'managed archive does not exist'
test -z "${XDG_DATA_HOME+x}" || fail 'XDG_DATA_HOME must be unset to exercise the installer default path'
test -z "${XDG_CONFIG_HOME+x}" || fail 'XDG_CONFIG_HOME must be unset to exercise the installer default path'

task_user=$(id -un)
task_home=${HOME:?HOME is required}
case "$task_home" in
  /*) ;;
  *) fail 'HOME must be absolute' ;;
esac
test "$task_home" != / || fail 'refusing to use the filesystem root as HOME'

install_parent="$task_home/.local/lib"
install_dir="$install_parent/risunest-sync"
bin_dir="$task_home/.local/bin"
wrapper="$bin_dir/risunest-sync-manager"
data_dir="$task_home/.local/share/risunest-sync"
unit_dir="$task_home/.config/systemd/user"
instance_name=$(python3 - "$data_dir" <<'PY'
import sys

value = 0xcbf29ce484222325
for byte in sys.argv[1].encode():
    value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
print(f"risunest-sync-{value:016x}")
PY
)
service="$instance_name.service"
update_service="$instance_name-update.service"
update_timer="$instance_name-update.timer"
service_path="$unit_dir/$service"
update_service_path="$unit_dir/$update_service"
update_timer_path="$unit_dir/$update_timer"

runner_temp=${RUNNER_TEMP:-/tmp}
case "$runner_temp" in
  /*) ;;
  *) fail 'RUNNER_TEMP must be absolute when set' ;;
esac
test_root=$(mktemp -d "$runner_temp/risunest-sync-managed-install.XXXXXX")
paths_owned=false
active_installer_pid=

cleanup() {
  result=$?
  trap - EXIT HUP INT TERM
  set +e
  cleanup_failed=false
  if test -n "$active_installer_pid" && kill -0 "$active_installer_pid" 2>/dev/null; then
    kill -TERM "$active_installer_pid" 2>/dev/null
    wait "$active_installer_pid"
  fi
  if test "$paths_owned" = true; then
    if test -x "$install_dir/risunest-sync-manager"; then
      "$install_dir/risunest-sync-manager" uninstall >/dev/null 2>&1
    fi
    systemctl --user disable --now "$update_timer" >/dev/null 2>&1
    systemctl --user stop "$update_service" >/dev/null 2>&1
    systemctl --user disable --now "$service" >/dev/null 2>&1
    rm -f -- "$service_path" "$update_service_path" "$update_timer_path" "$wrapper" || cleanup_failed=true
    systemctl --user daemon-reload >/dev/null 2>&1 || cleanup_failed=true
    systemctl --user reset-failed "$service" "$update_service" >/dev/null 2>&1
    rm -rf -- "$install_dir" "$data_dir" || cleanup_failed=true
    for unit in "$service" "$update_service" "$update_timer"; do
      if systemctl --user is-active --quiet "$unit"; then cleanup_failed=true; fi
    done
    for path in "$install_dir" "$wrapper" "$data_dir" "$service_path" "$update_service_path" "$update_timer_path"; do
      if test -e "$path" || test -L "$path"; then cleanup_failed=true; fi
    done
  fi
  rm -rf -- "$test_root" || cleanup_failed=true
  if test "$cleanup_failed" = true; then
    printf '%s\n' 'FAIL: synthetic installer state was not fully removed' >&2
    result=1
  fi
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

case "$test_root" in
  "$runner_temp"/risunest-sync-managed-install.*) ;;
  *) fail 'temporary directory is outside the expected runner directory' ;;
esac
extract_dir="$test_root/archive"
mkdir "$extract_dir"

for path in "$install_dir" "$wrapper" "$data_dir" "$service_path" "$update_service_path" "$update_timer_path"; do
  if test -e "$path" || test -L "$path"; then
    fail "refusing to overwrite pre-existing path: $path"
  fi
done
shopt -s nullglob
preexisting_stages=(
  "$install_parent"/.risunest-sync-update-installer-stage.*
  "$install_parent"/.risunest-sync-update-installer-backup-*
  "$bin_dir"/.risunest-sync-manager-stage.*
  "$bin_dir"/.risunest-sync-manager-backup.*
)
test "${#preexisting_stages[@]}" -eq 0 || fail "refusing to disturb pre-existing installer state: ${preexisting_stages[0]}"
for unit in "$service" "$update_service" "$update_timer"; do
  load_state=$(systemctl --user show "$unit" --property=LoadState --value 2>/dev/null || true)
  if test -n "$load_state" && test "$load_state" != not-found; then
    fail "refusing to replace a loaded user unit: $unit ($load_state)"
  fi
  if systemctl --user cat "$unit" >/dev/null 2>&1; then
    fail "refusing to replace an existing user unit: $unit"
  fi
done
paths_owned=true

test "$(loginctl show-user "$task_user" --property=Linger --value 2>/dev/null || true)" = yes || \
  fail 'the build-lab runner must enable linger before this test'
python3 - <<'PY'
import socket

with socket.socket() as probe:
    probe.bind(("127.0.0.1", 4319))
PY

tar -tzf "$managed_archive" > "$test_root/archive-entries.txt"
mapfile -t archive_entries < <(LC_ALL=C sort "$test_root/archive-entries.txt")
expected_entries=(
  CLOUDFLARED-LICENSE
  cloudflared
  install.sh
  risunest-sync-bundle.json
  risunest-sync-manager
  risunest-sync-server
)
test "${archive_entries[*]}" = "${expected_entries[*]}" || fail 'managed archive must contain only the six top-level installed files'
tar -xzf "$managed_archive" -C "$extract_dir"
read -r source_version source_protocol_id source_store_format_id < <(python3 - "$extract_dir" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
marker = json.loads((root / "risunest-sync-bundle.json").read_text())
assert marker["schema"] == "risunest-sync-bundle/v1"
assert marker["product"] == "sync"
assert marker["variant"] == "managed"
assert sorted(marker["files"]) == sorted(
    entry.name for entry in root.iterdir() if entry.name != "risunest-sync-bundle.json"
)
for entry in root.iterdir():
    assert entry.is_file() and not entry.is_symlink(), entry
vendor = marker["vendor"]
assert len(vendor) == 1 and vendor[0]["name"] == "cloudflared"
assert vendor[0]["path"] == "cloudflared"
assert hashlib.sha256((root / "cloudflared").read_bytes()).hexdigest() == vendor[0]["sha256"]
print(marker["version"], marker["protocolId"], marker["storeFormatId"])
PY
)

assert_clean_transaction() {
  test ! -e "$data_dir/manager-update/transaction.json" || fail 'installer transaction remained after success'
  local leftovers=(
    "$install_parent"/.risunest-sync-update-installer-stage.*
    "$install_parent"/.risunest-sync-update-installer-backup-*
    "$bin_dir"/.risunest-sync-manager-stage.*
    "$bin_dir"/.risunest-sync-manager-backup.*
  )
  test "${#leftovers[@]}" -eq 0 || fail "installer staging remained after success: ${leftovers[0]}"
}

assert_installed_files() {
  for name in "${expected_entries[@]}"; do
    test -f "$install_dir/$name" || fail "installed file is missing: $name"
    cmp "$extract_dir/$name" "$install_dir/$name" || fail "installed file differs from the produced archive: $name"
  done
  test "$(stat -c '%a' "$install_dir/risunest-sync-server")" = 700
  test "$(stat -c '%a' "$install_dir/risunest-sync-manager")" = 700
  test "$(stat -c '%a' "$install_dir/cloudflared")" = 700
  test "$(stat -c '%a' "$install_dir/CLOUDFLARED-LICENSE")" = 600
  test "$(stat -c '%a' "$install_dir/risunest-sync-bundle.json")" = 600
  test -x "$wrapper" || fail 'manager launcher was not installed'
  grep -qx '# RisuNest sync manager launcher' < <(sed -n '2p' "$wrapper") || fail 'manager launcher ownership marker is missing'
}

assert_registration_state() {
  "$wrapper" autostart status | python3 -c 'import json,sys; assert json.load(sys.stdin) == {"registered": True, "enabled": True, "actionMatches": True}'
  "$wrapper" update status | python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["settings"]["policy"] == "notify"; assert value["schedule"] == {"registered": True, "enabled": True, "actionMatches": True}'
  test "$(systemctl --user is-enabled "$service")" = enabled
  test "$(systemctl --user is-enabled "$update_timer")" = enabled
  test "$(systemctl --user is-active "$update_timer")" = active
  test "$(systemctl --user show "$service" --property=FragmentPath --value)" = "$service_path"
  grep -Fq "ExecStart=\"$install_dir/risunest-sync-server\" serve --data-dir \"$data_dir\"" "$service_path"
  grep -Fq "ExecStart=\"$install_dir/risunest-sync-manager\" --data-dir \"$data_dir\" --server \"$install_dir/risunest-sync-server\" update scheduled" "$update_service_path"
  test "$(loginctl show-user "$task_user" --property=Linger --value)" = yes
}

assert_service_active() {
  test "$(systemctl --user is-active "$service")" = active
}

assert_running_health() {
  local expected_version=$1
  "$wrapper" status | python3 -c '
import json
import sys

expected_version, protocol_id, store_format_id = sys.argv[1:]
value = json.load(sys.stdin)
assert value["version"] == expected_version
assert value["protocolId"] == protocol_id
assert value["storeFormatId"] == store_format_id
assert value["devices"] == []
if value["connectionState"]["mode"] == "managed":
    assert value["tunnel"]["phase"] == "connected" or (
        value["tunnel"]["phase"] == "failed"
        and value["tunnel"]["error"] == "tunnel-readiness-timeout"
    )
publication = value["publication"]
assert publication["phase"] in ("published", "disabled", "stopped") or (
    publication["phase"] == "failed"
    and publication["error"] in ("directory-unreachable", "directory-publication-failed")
)
' "$expected_version" "$source_protocol_id" "$source_store_format_id"
}

assert_manager_state() {
  assert_running_health "$1"
  assert_registration_state
  assert_service_active
}

sh "$extract_dir/install.sh" --non-interactive --policy=notify
assert_installed_files
assert_manager_state "$source_version"
assert_clean_transaction
test -f "$data_dir/metadata.sqlite" || fail 'initial install did not initialize the daemon store'
initial_install_inode=$(stat -c '%i' "$install_dir")
initial_store_inode=$(stat -c '%i' "$data_dir/metadata.sqlite")
initial_invocation=$(systemctl --user show "$service" --property=InvocationID --value)
test -n "$initial_invocation" || fail 'initial systemd invocation ID is unavailable'

# Reinstall the identical produced archive while the managed daemon is running.
sh "$extract_dir/install.sh" --non-interactive --policy=notify
assert_installed_files
assert_manager_state "$source_version"
assert_clean_transaction
test "$(stat -c '%i' "$install_dir")" != "$initial_install_inode" || fail 'same-version reinstall did not swap the installed bundle directory'
test "$(stat -c '%i' "$data_dir/metadata.sqlite")" = "$initial_store_inode" || fail 'same-version reinstall replaced the daemon data store'
reinstalled_invocation=$(systemctl --user show "$service" --property=InvocationID --value)
test -n "$reinstalled_invocation" || fail 'reinstalled systemd invocation ID is unavailable'
test "$reinstalled_invocation" != "$initial_invocation" || fail 'same-version reinstall did not restart the managed daemon'

stable_install_inode=$(stat -c '%i' "$install_dir")
stable_store_inode=$(stat -c '%i' "$data_dir/metadata.sqlite")
stable_license_hash=$(python3 - "$install_dir/CLOUDFLARED-LICENSE" <<'PY'
import hashlib
from pathlib import Path
import sys

print(hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)
failure_dir="$test_root/post-swap-failure"
cp -a "$extract_dir" "$failure_dir"
printf '%s\n' 'same-version-post-swap-failure-flavor' >> "$failure_dir/CLOUDFLARED-LICENSE"
failure_license_hash=$(python3 - "$failure_dir/CLOUDFLARED-LICENSE" <<'PY'
import hashlib
from pathlib import Path
import sys

print(hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)
test "$failure_license_hash" != "$stable_license_hash"

# Make the first post-swap schedule rewrite fail through the real manager and systemd path.
chmod 400 "$update_service_path"
if sh "$failure_dir/install.sh" --non-interactive --policy=notify; then
  fail 'post-swap configuration failure unexpectedly succeeded'
fi
chmod 600 "$update_service_path"
test "$(stat -c '%i' "$install_dir")" = "$stable_install_inode" || fail 'same-version rollback did not restore the original install directory'
test "$(stat -c '%i' "$data_dir/metadata.sqlite")" = "$stable_store_inode" || fail 'same-version rollback replaced the daemon data store'
rolled_back_license_hash=$(python3 - "$install_dir/CLOUDFLARED-LICENSE" <<'PY'
import hashlib
from pathlib import Path
import sys

print(hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)
test "$rolled_back_license_hash" = "$stable_license_hash" || fail 'same-version rollback did not restore the prior bundle bytes'
test "$rolled_back_license_hash" != "$failure_license_hash" || fail 'same-version rollback accepted the staged flavor as the prior bundle'
assert_installed_files
assert_manager_state "$source_version"
assert_clean_transaction

# A repeated rollback command with no transaction must be harmless at the real CLI boundary.
exec 9>"$data_dir/manager-update/instance.lock"
flock -n 9 || fail 'could not reacquire the installer lock for rollback idempotence'
"$install_dir/risunest-sync-manager" installer rollback-swap lock-held false 9>&-
flock -u 9
exec 9>&-
assert_manager_state "$source_version"
assert_clean_transaction

health_failure_dir="$test_root/post-start-health-failure"
cp -a "$extract_dir" "$health_failure_dir"
# The real daemon remains executable and locally healthy, but cannot report this distinct target version.
# That keeps OS start acceptance separate from the manager's authenticated target identity gate.
health_target_version="$source_version-health-mismatch"
python3 - "$health_failure_dir/risunest-sync-bundle.json" "$health_target_version" <<'PY'
import json
from pathlib import Path
import sys

path = Path(sys.argv[1])
value = json.loads(path.read_text())
value["version"] = sys.argv[2]
path.write_text(json.dumps(value, indent=2) + "\n")
PY
printf '%s\n' 'post-start-health-failure-flavor' >> "$health_failure_dir/CLOUDFLARED-LICENSE"
health_source_inode=$(stat -c '%i' "$install_dir")
health_store_inode=$(stat -c '%i' "$data_dir/metadata.sqlite")
health_source_invocation=$(systemctl --user show "$service" --property=InvocationID --value)
test -n "$health_source_invocation" || fail 'source systemd invocation ID is unavailable before the health gate test'

health_log="$test_root/post-start-health-failure.log"
sh "$health_failure_dir/install.sh" --non-interactive --policy=notify >"$health_log" 2>&1 &
health_installer_pid=$!
active_installer_pid=$health_installer_pid
health_start_observed=false
health_poll=0
while test "$health_poll" -lt 300 && kill -0 "$health_installer_pid" 2>/dev/null; do
  health_poll=$((health_poll + 1))
  transaction="$data_dir/manager-update/transaction.json"
  if test -f "$transaction" \
    && python3 - "$transaction" "$health_target_version" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text())
assert value["phase"] == "restarting"
assert value["targetVersion"] == sys.argv[2]
assert value["wasRunning"] is True
PY
  then
    target_invocation=$(systemctl --user show "$service" --property=InvocationID --value 2>/dev/null || true)
    if test -n "$target_invocation" \
      && test "$target_invocation" != "$health_source_invocation" \
      && test "$(systemctl --user is-active "$service" 2>/dev/null || true)" = active \
      && cmp "$health_failure_dir/CLOUDFLARED-LICENSE" "$install_dir/CLOUDFLARED-LICENSE" \
      && target_status=$("$install_dir/risunest-sync-manager" status 2>/dev/null) \
      && printf '%s' "$target_status" | python3 -c '
import json
import sys

source_version, target_version, protocol_id, store_format_id = sys.argv[1:]
value = json.load(sys.stdin)
assert source_version != target_version
assert value["version"] == source_version
assert value["protocolId"] == protocol_id
assert value["storeFormatId"] == store_format_id
' "$source_version" "$health_target_version" "$source_protocol_id" "$source_store_format_id"
    then
      health_start_observed=true
      break
    fi
  fi
  sleep 0.1
done
if wait "$health_installer_pid"; then
  fail 'post-start unhealthy target unexpectedly committed'
else
  health_installer_result=$?
fi
active_installer_pid=
test "$health_installer_result" -ne 0
test "$health_start_observed" = true || fail 'health fixture did not prove an accepted target systemd start before rejection'
grep -Fq 'updated-server-health-failed' "$health_log" || fail 'health fixture failed for a reason other than target health rejection'
test "$(stat -c '%i' "$install_dir")" = "$health_source_inode" || fail 'unhealthy target rollback did not restore the source install directory'
test "$(stat -c '%i' "$data_dir/metadata.sqlite")" = "$health_store_inode" || fail 'unhealthy target rollback replaced the daemon data store'
assert_installed_files
assert_manager_state "$source_version"
assert_clean_transaction
health_source_after_rollback=$(systemctl --user show "$service" --property=InvocationID --value)
test -n "$health_source_after_rollback" || fail 'source invocation is unavailable after unhealthy target rollback'
test "$health_source_after_rollback" != "$target_invocation" || fail 'unhealthy target process was not terminated before source restart'

stopped_source_inode=$(stat -c '%i' "$install_dir")
stopped_store_inode=$(stat -c '%i' "$data_dir/metadata.sqlite")
"$wrapper" stop
if systemctl --user is-active --quiet "$service"; then fail 'manager stop left the source service active'; fi
test ! -e "$data_dir/management-session" || fail 'manager stop left a live management locator'

stopped_health_log="$test_root/post-start-health-failure-stopped-source.log"
sh "$health_failure_dir/install.sh" --non-interactive --policy=notify >"$stopped_health_log" 2>&1 &
stopped_health_installer_pid=$!
active_installer_pid=$stopped_health_installer_pid
stopped_health_start_observed=false
stopped_health_poll=0
while test "$stopped_health_poll" -lt 300 && kill -0 "$stopped_health_installer_pid" 2>/dev/null; do
  stopped_health_poll=$((stopped_health_poll + 1))
  transaction="$data_dir/manager-update/transaction.json"
  if test -f "$transaction" \
    && python3 - "$transaction" "$health_target_version" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text())
assert value["phase"] == "restarting"
assert value["targetVersion"] == sys.argv[2]
assert value["wasRunning"] is False
PY
  then
    stopped_target_invocation=$(systemctl --user show "$service" --property=InvocationID --value 2>/dev/null || true)
    if test -n "$stopped_target_invocation" \
      && test "$(systemctl --user is-active "$service" 2>/dev/null || true)" = active \
      && cmp "$health_failure_dir/CLOUDFLARED-LICENSE" "$install_dir/CLOUDFLARED-LICENSE" \
      && target_status=$("$install_dir/risunest-sync-manager" status 2>/dev/null) \
      && printf '%s' "$target_status" | python3 -c '
import json
import sys

source_version, target_version, protocol_id, store_format_id = sys.argv[1:]
value = json.load(sys.stdin)
assert source_version != target_version
assert value["version"] == source_version
assert value["protocolId"] == protocol_id
assert value["storeFormatId"] == store_format_id
' "$source_version" "$health_target_version" "$source_protocol_id" "$source_store_format_id"
    then
      stopped_health_start_observed=true
      break
    fi
  fi
  sleep 0.1
done
if wait "$stopped_health_installer_pid"; then
  fail 'stopped-source unhealthy target unexpectedly committed'
else
  stopped_health_installer_result=$?
fi
active_installer_pid=
test "$stopped_health_installer_result" -ne 0
test "$stopped_health_start_observed" = true || fail 'stopped-source fixture did not prove an accepted target systemd start before rejection'
grep -Fq 'updated-server-health-failed' "$stopped_health_log" || fail 'stopped-source fixture failed for a reason other than target health rejection'
test "$(stat -c '%i' "$install_dir")" = "$stopped_source_inode" || fail 'stopped-source rollback did not restore the source install directory'
test "$(stat -c '%i' "$data_dir/metadata.sqlite")" = "$stopped_store_inode" || fail 'stopped-source rollback replaced the daemon data store'
assert_installed_files
assert_registration_state
assert_clean_transaction
if systemctl --user is-active --quiet "$service"; then fail 'stopped-source rollback left the target or source service active'; fi
test ! -e "$data_dir/management-session" || fail 'stopped-source rollback left a live management locator'

stopped_success_source_inode=$(stat -c '%i' "$install_dir")
sh "$extract_dir/install.sh" --non-interactive --policy=notify
test "$(stat -c '%i' "$install_dir")" != "$stopped_success_source_inode" || fail 'explicit stopped-source reinstall did not swap the installed bundle directory'
assert_installed_files
assert_manager_state "$source_version"
assert_clean_transaction

printf '%s\n' 'PASS: produced Linux managed archive initial install and same-version reinstall'
printf '%s\n' 'PASS: real manager CLI, user service, update timer, linger, data preservation, and transaction cleanup'
printf '%s\n' 'PASS: injected post-swap failure restores prior same-version bytes and repeated rollback is idempotent'
printf '%s\n' 'PASS: accepted target start is health-gated before commit and unhealthy target rollback restores healthy source bytes'
printf '%s\n' 'PASS: unhealthy target rollback terminates the target and preserves stopped source intent'
printf '%s\n' 'PASS: successful explicit reinstall of a stopped source finishes running and enabled'
printf '%s\n' 'NOTE: this logged-in runner test does not claim reboot-before-first-login validation'
