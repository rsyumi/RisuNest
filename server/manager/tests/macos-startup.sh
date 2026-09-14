#!/bin/sh
set -eu

server=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
manager=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
test_root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/risunest-manager-launchd.XXXXXX")
data_root="$test_root/data"
test_home="$test_root/home"
mkdir -p "$test_home/Library/LaunchAgents"
domain="gui/$(id -u)"
daemon_label=""
update_label=""

manager_run() {
  env HOME="$test_home" "$manager" --data-dir "$data_root" --server "$server" "$@"
}

cleanup() {
  manager_run update policy off >/dev/null 2>&1 || true
  if test -n "$update_label"; then launchctl bootout "$domain/$update_label" >/dev/null 2>&1 || true; fi
  if test -n "$daemon_label"; then launchctl bootout "$domain/$daemon_label" >/dev/null 2>&1 || true; fi
  manager_run prepare-update >/dev/null 2>&1 || true
  rm -rf "$test_root"
}
trap cleanup EXIT INT TERM

manager_run autostart install
daemon_plist=$(find "$test_home/Library/LaunchAgents" -maxdepth 1 -name 'io.github.rsyumi.risunest-sync-*.plist' ! -name '*-update.plist' -print)
test "$(printf '%s\n' "$daemon_plist" | sed '/^$/d' | wc -l | tr -d ' ')" = 1
daemon_label=$(/usr/libexec/PlistBuddy -c 'Print :Label' "$daemon_plist")
launchctl print "$domain/$daemon_label" >/dev/null

update_plist=$(find "$test_home/Library/LaunchAgents" -maxdepth 1 -name 'io.github.rsyumi.risunest-sync-*-update.plist' -print)
test -z "$update_plist"
update_status=$(manager_run update status)
STATUS="$update_status" node -e 'const s=JSON.parse(process.env.STATUS); if(s.settings.policy!=="automatic" || s.schedule.registered || s.schedule.enabled) process.exit(1)'

manager_run update schedule reconcile
update_plist=$(find "$test_home/Library/LaunchAgents" -maxdepth 1 -name 'io.github.rsyumi.risunest-sync-*-update.plist' -print)
test "$(printf '%s\n' "$update_plist" | sed '/^$/d' | wc -l | tr -d ' ')" = 1
update_label=$(/usr/libexec/PlistBuddy -c 'Print :Label' "$update_plist")
launchctl print "$domain/$update_label" >/dev/null
update_status=$(manager_run update status)
STATUS="$update_status" node -e 'const s=JSON.parse(process.env.STATUS); if(s.settings.policy!=="automatic" || !s.schedule.registered || !s.schedule.enabled || !s.schedule.actionMatches) process.exit(1)'

status=$(manager_run autostart status)
STATUS="$status" node -e 'const s=JSON.parse(process.env.STATUS); if(!s.registered || !s.enabled || !s.actionMatches) process.exit(1)'
manager_run start
ready=false
attempt=0
while test "$attempt" -lt 50; do
  if manager_run status >/dev/null 2>&1; then ready=true; break; fi
  attempt=$((attempt + 1))
  sleep 0.1
done
test "$ready" = true

manager_run autostart remove
status=$(manager_run autostart status)
STATUS="$status" node -e 'const s=JSON.parse(process.env.STATUS); if(s.registered || s.enabled) process.exit(1)'
launchctl print "$domain/$update_label" >/dev/null
manager_run status >/dev/null

manager_run update policy off >/dev/null
test ! -e "$update_plist"
if launchctl print "$domain/$update_label" >/dev/null 2>&1; then exit 1; fi
update_status=$(manager_run update status)
STATUS="$update_status" node -e 'const s=JSON.parse(process.env.STATUS); if(s.settings.policy!=="off" || s.schedule.registered || s.schedule.enabled) process.exit(1)'
status=$(manager_run autostart status)
STATUS="$status" node -e 'const s=JSON.parse(process.env.STATUS); if(s.registered || s.enabled) process.exit(1)'
manager_run status >/dev/null
manager_run stop
printf '%s\n' 'PASS: macOS independent LaunchAgent registration, scheduling, cleanup, live-removal preservation and graceful stop'
