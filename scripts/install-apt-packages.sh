#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "Usage: install-apt-packages.sh package..." >&2
  exit 2
fi

# A slow mirror delivers without failing a request, so APT's own retries never start. Each attempt's
# index refresh and download share one time limit, and a failed attempt drops the mirror it used
# from the runner's mirror list. Installation itself runs unbounded from the downloaded archives.
# Three attempts at the default limit end within 14 minutes, inside the callers' 20-minute steps.
mirrors=${APT_MIRROR_LIST:-/etc/apt/apt-mirrors.txt}
limit=${APT_ATTEMPT_SECONDS:-240}
fetch='apt-get -o Acquire::Retries=2 --error-on=any update && apt-get -o Acquire::Retries=2 install --download-only -y "$@"'

for attempt in 1 2 3; do
  status=0
  sudo timeout --kill-after=30 "$limit" bash -c "$fetch" fetch "$@" || status=$?
  if [ "$status" -eq 0 ]; then
    sudo apt-get -o Acquire::Retries=2 install -y "$@"
    exit 0
  fi
  if [ "$attempt" -eq 3 ] || { [ "$status" -ge 128 ] && [ "$status" -ne 137 ]; }; then
    exit "$status"
  fi
  mirror=$(grep -m 1 -v -E '^[[:space:]]*(#|$)' "$mirrors" 2>/dev/null || true)
  # An indented line names a mirror too.
  mirror=${mirror#"${mirror%%[![:space:]]*}"}
  if [ -n "$mirror" ] && [ "$(grep -c -v -E '^[[:space:]]*(#|$)' "$mirrors")" -gt 1 ]; then
    sudo sed -i "0,/^[[:space:]]*[^#[:space:]]/{//d}" "$mirrors"
    echo "APT attempt $attempt failed (exit $status); retrying without ${mirror%%[[:space:]]*}." >&2
  else
    echo "APT attempt $attempt failed (exit $status); retrying." >&2
  fi
done
