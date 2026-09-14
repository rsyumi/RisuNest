#!/bin/sh
# Run from an extracted, verified Linux release archive. No downloads or root daemon.
set -eu
umask 077

non_interactive=false
policy=automatic
while test "$#" -gt 0; do
  case "$1" in
    --non-interactive) non_interactive=true ;;
    --policy=automatic|--policy=notify|--policy=off) policy=${1#--policy=} ;;
    *) printf '알 수 없는 옵션: %s\n' "$1" >&2; exit 2 ;;
  esac
  shift
done
case "$(uname -s)" in Linux) ;; *) printf '%s\n' 'Linux에서 실행하세요.' >&2; exit 1;; esac
case "${HOME:?HOME is required}" in /*) ;; *) exit 1;; esac
if test "$(id -u)" -eq 0; then
  printf '%s\n' 'root로 전체 설치 스크립트를 실행하지 마세요. 서버를 실행할 일반 사용자로 다시 실행하세요.' >&2
  exit 2
fi
command -v systemctl >/dev/null 2>&1 && command -v loginctl >/dev/null 2>&1 || {
  printf '%s\n' 'systemd 사용자 서비스와 loginctl이 필요합니다. root 서버로 자동 전환하지 않습니다.' >&2
  exit 3
}

target_user=$(id -un)
linger=$(loginctl show-user "$target_user" -p Linger --value 2>/dev/null || true)
if test "$linger" != yes; then
  printf '로그인 전에도 Sync를 시작하도록 %s 계정에 linger를 설정합니다.\n' "$target_user"
  if test "$non_interactive" = true; then
    linger_command='loginctl enable-linger --no-ask-password'
  else
    linger_command='loginctl enable-linger'
  fi
  if ! $linger_command "$target_user"; then
    printf '%s\n' 'linger를 설정하지 못했습니다. 시스템 정책에 따라 최초 한 번 관리자 승인이 필요할 수 있습니다.' >&2
    exit 4
  fi
fi
test "$(loginctl show-user "$target_user" -p Linger --value 2>/dev/null || true)" = yes || {
  printf '%s\n' 'linger 활성화를 확인하지 못했습니다.' >&2
  exit 4
}

source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
install_parent="$HOME/.local/lib"
install_dir="$install_parent/risunest-sync"
bin_dir="$HOME/.local/bin"
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}/risunest-sync
wrapper="$bin_dir/risunest-sync-manager"
for input in risunest-sync-server risunest-sync-manager cloudflared CLOUDFLARED-LICENSE risunest-sync-bundle.json install.sh; do
  test -f "$source_dir/$input" || { printf '누락된 파일: %s\n' "$input" >&2; exit 1; }
done
if { test -e "$wrapper" || test -L "$wrapper"; } && ! head -n 2 "$wrapper" | tail -n 1 | grep -qx '# RisuNest sync manager launcher'; then
  printf '%s\n' '기존 ~/.local/bin/risunest-sync-manager를 확인하세요. 자동으로 덮어쓰지 않습니다.' >&2
  exit 1
fi
if test "$non_interactive" = false && test -t 0; then
  printf '%s\n' '업데이트 정책을 선택하세요: 1) 안전할 때 자동 적용 (기본) 2) 알림만 3) 예약 확인 끄기'
  answer=1
  read -r answer || true
  case "$answer" in 2) policy=notify ;; 3) policy=off ;; *) policy=automatic ;; esac
fi

mkdir -p "$install_parent" "$bin_dir" "$data_dir/manager-update"
if command -v flock >/dev/null 2>&1; then
  exec 9>"$data_dir/manager-update/instance.lock"
  flock -n 9 || { printf '%s\n' '다른 설치 또는 업데이트가 진행 중입니다.' >&2; exit 5; }
else
  printf '%s\n' '설치 충돌을 막기 위한 flock 명령이 필요합니다.' >&2
  exit 5
fi

stage_dir="$install_parent/.risunest-sync-update-installer-stage.$$"
wrapper_stage="$bin_dir/.risunest-sync-manager-stage.$$"
wrapper_backup="$bin_dir/.risunest-sync-manager-backup.$$"
rollback_needed=false
managed_swap=false
fresh_swap=false
wrapper_moved=false
wrapper_swapped=false
restore_run_intent=false
configuration_attempted=false
server_start_attempted=false

finish_install() {
  result=$?
  trap - EXIT HUP INT TERM
  preserve_stage=false
  bundle_rolled_back=true
  if test "$rollback_needed" = true; then
    if test "$managed_swap" = true; then
      if "$install_dir/risunest-sync-manager" installer rollback-swap lock-held "$restore_run_intent" 9>&- >/dev/null 2>&1; then
        restore_run_intent=false
      else
        preserve_stage=true
        bundle_rolled_back=false
        result=1
      fi
    elif test "$fresh_swap" = true; then
      fresh_cleanup_ok=true
      if test "$server_start_attempted" = true && ! "$install_dir/risunest-sync-manager" stop 9>&- >/dev/null 2>&1; then
        fresh_cleanup_ok=false
        result=1
      fi
      if test "$configuration_attempted" = true; then
        if ! "$install_dir/risunest-sync-manager" update schedule remove lock-held 9>&- >/dev/null 2>&1; then
          fresh_cleanup_ok=false
          result=1
        fi
        if ! "$install_dir/risunest-sync-manager" autostart remove lock-held 9>&- >/dev/null 2>&1; then
          fresh_cleanup_ok=false
          result=1
        fi
      fi
      if test "$fresh_cleanup_ok" = true; then
        rm -rf "$install_dir" || result=1
      else
        bundle_rolled_back=false
      fi
    fi
    if test "$bundle_rolled_back" = true; then
      if test "$wrapper_moved" = true && test -e "$wrapper_backup"; then
        mv -f "$wrapper_backup" "$wrapper" || result=1
      elif test "$wrapper_swapped" = true; then
        rm -f "$wrapper" || result=1
      fi
    fi
  fi
  if test "$preserve_stage" = false; then rm -rf "$stage_dir" 2>/dev/null || true; fi
  rm -f "$wrapper_stage" 2>/dev/null || true
  if test "$restore_run_intent" = true && test -x "$install_dir/risunest-sync-manager"; then
    "$install_dir/risunest-sync-manager" start lock-held 9>&- >/dev/null 2>&1 || result=1
  fi
  flock -u 9 2>/dev/null || true
  exec 9>&-
  exit "$result"
}
trap finish_install EXIT
trap 'exit 130' HUP INT TERM

mkdir "$stage_dir"
for binary in risunest-sync-server risunest-sync-manager cloudflared; do
  install -m 700 "$source_dir/$binary" "$stage_dir/$binary"
done
install -m 600 "$source_dir/CLOUDFLARED-LICENSE" "$stage_dir/CLOUDFLARED-LICENSE"
install -m 600 "$source_dir/risunest-sync-bundle.json" "$stage_dir/risunest-sync-bundle.json"
install -m 700 "$source_dir/install.sh" "$stage_dir/install.sh"
"$stage_dir/risunest-sync-manager" --data-dir "$data_dir" --server "$stage_dir/risunest-sync-server" installer sync-stage lock-held "$stage_dir" 9>&-
cat > "$wrapper_stage" <<'LAUNCHER'
#!/bin/sh
# RisuNest sync manager launcher
exec "$HOME/.local/lib/risunest-sync/risunest-sync-manager" "$@"
LAUNCHER
chmod 700 "$wrapper_stage"

rollback_needed=true
if test -x "$install_dir/risunest-sync-manager"; then
  if "$install_dir/risunest-sync-manager" status >/dev/null 2>&1; then
    restore_run_intent=true
  fi
  "$install_dir/risunest-sync-manager" prepare-update lock-held 9>&-
  managed_swap=true
  "$install_dir/risunest-sync-manager" installer swap lock-held "$stage_dir" "$restore_run_intent" 9>&-
elif test -e "$install_dir"; then
  printf '%s\n' '기존 설치 디렉터리에 실행 가능한 Sync 관리자가 없습니다. 해당 디렉터리를 확인하세요.' >&2
  exit 1
else
  mv "$stage_dir" "$install_dir"
  fresh_swap=true
fi
if test -e "$wrapper" || test -L "$wrapper"; then
  cp -pP "$wrapper" "$wrapper_backup"
  wrapper_moved=true
fi
mv "$wrapper_stage" "$wrapper"
wrapper_swapped=true

configuration_attempted=true
"$install_dir/risunest-sync-manager" update policy "$policy" lock-held 9>&-
"$install_dir/risunest-sync-manager" autostart install lock-held 9>&-
server_start_attempted=true
"$install_dir/risunest-sync-manager" installer start-and-verify lock-held 9>&-
if test "$managed_swap" = true; then
  "$install_dir/risunest-sync-manager" installer commit-swap lock-held 9>&-
fi
restore_run_intent=false
rollback_needed=false
rm -f "$wrapper_backup"
flock -u 9
exec 9>&-
trap - EXIT HUP INT TERM

printf '%s\n' '설치했습니다. 로그인하지 않은 동안의 실행은 시스템의 사용자 서비스 정책을 따릅니다.'
case ":$PATH:" in *":$bin_dir:"*) ;; *) printf 'PATH에 %s를 추가하면 risunest-sync-manager 명령을 사용할 수 있습니다.\n' "$bin_dir";; esac
if test "$non_interactive" = false && test -t 0 && test -t 1; then exec "$install_dir/risunest-sync-manager"; fi
