#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "Usage: install-apt-packages.sh package..." >&2
  exit 2
fi

# A slow mirror delivers without failing a request, so APT's own retries never start. Each index
# refresh and download is bounded, and a failed attempt drops the mirror it used from the runner's
# mirror list. Installation itself runs unbounded from the downloaded archives.
mirrors=${APT_MIRROR_LIST:-/etc/apt/apt-mirrors.txt}
limit=${APT_ATTEMPT_SECONDS:-240}
apt=(apt-get -o Acquire::Retries=2)

for attempt in 1 2 3; do
  status=0
  sudo timeout --kill-after=30 "$limit" "${apt[@]}" --error-on=any update || status=$?
  if [ "$status" -eq 0 ]; then
    sudo timeout --kill-after=30 "$limit" "${apt[@]}" install --download-only -y "$@" || status=$?
  fi
  if [ "$status" -eq 0 ]; then
    sudo "${apt[@]}" install -y "$@"
    exit 0
  fi
  if [ "$attempt" -eq 3 ] || { [ "$status" -ge 128 ] && [ "$status" -ne 137 ]; }; then
    exit "$status"
  fi
  mirror=$(grep -m 1 -v -E '^[[:space:]]*(#|$)' "$mirrors" 2>/dev/null || true)
  if [ -n "$mirror" ] && [ "$(grep -c -v -E '^[[:space:]]*(#|$)' "$mirrors")" -gt 1 ]; then
    sudo sed -i "0,/^[^#[:space:]]/{//d}" "$mirrors"
    echo "APT attempt $attempt failed (exit $status); retrying without ${mirror%%[[:space:]]*}." >&2
  else
    echo "APT attempt $attempt failed (exit $status); retrying." >&2
  fi
done
