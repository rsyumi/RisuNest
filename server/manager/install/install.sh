#!/bin/sh
# Run from an extracted, verified Linux release archive. No downloads or sudo.
set -eu
umask 077
case "$(uname -s)" in Linux) ;; *) printf '%s\n' 'Linux에서 실행하세요.' >&2; exit 1;; esac
case "${HOME:?HOME is required}" in /*) ;; *) exit 1;; esac
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
install_dir="$HOME/.local/lib/risunest-sync"
bin_dir="$HOME/.local/bin"
for binary in risunest-sync-server risunest-sync-manager cloudflared; do
  test -f "$source_dir/$binary" || { printf '누락된 파일: %s\n' "$binary" >&2; exit 1; }
done
if test -x "$install_dir/risunest-sync-manager"; then
  "$install_dir/risunest-sync-manager" prepare-update
fi
mkdir -p "$install_dir" "$bin_dir"
for binary in risunest-sync-server risunest-sync-manager cloudflared; do
  install -m 700 "$source_dir/$binary" "$install_dir/$binary"
done
install -m 600 "$source_dir/CLOUDFLARED-LICENSE" "$install_dir/CLOUDFLARED-LICENSE"
# A wrapper preserves the sibling daemon path when ~/.local/bin is a symlink farm.
wrapper="$bin_dir/risunest-sync-manager"
if test -e "$wrapper" && ! head -n 2 "$wrapper" | tail -n 1 | grep -qx '# RisuNest sync manager launcher'; then
  printf '%s\n' '기존 ~/.local/bin/risunest-sync-manager를 확인하세요. 자동으로 덮어쓰지 않습니다.' >&2
  exit 1
fi
cat > "$wrapper" <<'LAUNCHER'
#!/bin/sh
# RisuNest sync manager launcher
exec "$HOME/.local/lib/risunest-sync/risunest-sync-manager" "$@"
LAUNCHER
chmod 700 "$wrapper"
printf '%s\n' '설치했습니다. 로그인하지 않은 동안의 실행은 시스템의 사용자 서비스 정책을 따릅니다.'
printf '%s\n' '로그인 시 서버를 자동으로 실행할까요? [Y/n]'
answer=n
if test -t 0; then read -r answer; fi
case "$answer" in ''|y|Y)
  "$install_dir/risunest-sync-manager" autostart install
  ;;
esac
"$install_dir/risunest-sync-manager" start
case ":$PATH:" in *":$bin_dir:"*) ;; *) printf 'PATH에 %s를 추가하면 risunest-sync-manager 명령을 사용할 수 있습니다.\n' "$bin_dir";; esac
if test -t 0 && test -t 1; then exec "$install_dir/risunest-sync-manager"; fi
