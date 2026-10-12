#!/usr/bin/env bash
# down.sh [--force]: exits 0 when everything is released, 3 when something stays busy.
source /lab/in/common.sh
set +e

# A lazy unmount detaches a mount the kernel still references; once the
# container exits nothing can release it, and its loop device stays attached
# on the host until reboot. So a busy filesystem stops the teardown with the
# container still running, unless --force accepts that risk.
force=0
[[ ${1:-} == --force ]] && force=1
BUSY=3
busy=()

wait_until() {
  local seconds=$1
  shift
  local deadline=$((SECONDS + seconds))
  until "$@"; do
    ((SECONDS < deadline)) || return 1
    sleep 0.5
  done
}

under() { [[ $1 == "$2" || $1 == "$2"/* ]]; }

# Processes, other than this script and its ancestors, holding anything under $1.
users_of() {
  local target=$1 pid link
  local -A own=()
  pid=$$
  while ((pid > 1)); do
    own[$pid]=1
    pid=$(ps -o ppid= -p "$pid" 2>/dev/null) || break
    pid=${pid// /}
  done
  for link in /proc/[0-9]*; do
    pid=${link#/proc/}
    [[ -n ${own[$pid]:-} || $pid == 1 ]] && continue
    for link in /proc/"$pid"/{cwd,root,exe} /proc/"$pid"/fd/*; do
      if under "$(readlink "$link" 2>/dev/null)" "$target"; then
        echo "$pid"
        continue 2
      fi
    done
    if awk -v target="$target" '$6 == target || index($6, target "/") == 1 { found = 1 } END { exit !found }' \
      "/proc/$pid/maps" 2>/dev/null; then
      echo "$pid"
    fi
  done
}

no_users() { [[ -z $(users_of "$1") ]]; }

stop_users() {
  local pids
  pids=$(users_of "$1")
  [[ -n $pids ]] || return 0
  log "stopping processes using $1: $(echo $pids)"
  kill $pids 2>/dev/null
  wait_until 5 no_users "$1" || kill -9 $(users_of "$1") 2>/dev/null
}

release() {
  local target=$1 deadline=$((SECONDS + ${2:-30}))
  while is_mounted "$target"; do
    stop_users "$target"
    timeout -k 5 30 umount "$target" 2>/dev/null && continue
    ((SECONDS < deadline)) || break
    sleep 1
  done
  is_mounted "$target" || return 0
  if ((force)); then
    log "WARNING: --force: lazily unmounting busy $target; its loop device can stay attached on the host until reboot"
    umount -f -l "$target" && return 0
  fi
  busy+=("$target")
  return 1
}

stop_daemon() {
  pkill -x "$1" || return 0
  wait_until 10 bash -c "! pgrep -x '$1' >/dev/null" || pkill -9 -x "$1"
}

nfsd_clients() { ls /proc/fs/nfsd/clients 2>/dev/null; }
nfsd_threads() { cat /proc/fs/nfsd/threads 2>/dev/null || echo 0; }
nfsd_idle() { [[ -z $(nfsd_clients) && $(nfsd_threads) == 0 ]]; }

report_nfsd() {
  is_mounted /proc/fs/nfsd || return 0
  log "nfsd threads: $(nfsd_threads)"
  local client
  for client in $(nfsd_clients); do
    log "nfsd client $client: $(grep -E '^(address|name|minor version):' "/proc/fs/nfsd/clients/$client/info" 2>/dev/null | tr '\n' ' ')"
  done
  log "nfsd export cache:"
  grep -v '^#' /proc/net/rpc/nfsd.export/content >&2 2>/dev/null
  log "nfsd file cache:"
  head -3 /proc/fs/nfsd/filecache >&2 2>/dev/null
}

# Leaves the servers and $STATE in place so that `down` can be run again.
stop_if_busy() {
  ((${#busy[@]})) || return 0
  local target pid
  log "teardown stopped; still busy:"
  for target in "${busy[@]}"; do
    if [[ $target == /dev/* ]]; then
      log "  $target, mounted at: $(findmnt -rn -S "$target" -o TARGET 2>/dev/null | tr '\n' ' ')"
    else
      log "  $target ($(findmnt -rn -o FSTYPE,SOURCE --mountpoint "$target" 2>/dev/null | tr '\n' ' '))"
    fi
    for pid in $(users_of "$target"); do
      log "    used by pid $pid: $(ps -o comm=,args= -p "$pid" 2>/dev/null)"
    done
  done
  report_nfsd
  exit "$BUSY"
}

# The recorded device may have been detached and reused by the host since up.
backed_by() {
  local dev=$1 image=$2
  [[ -f $image && $(losetup -n -O BACK-FILE "$dev" 2>/dev/null) == "$image" ]] || return 1
  [[ $(losetup -n -O BACK-INO,BACK-MAJ:MIN "$dev" | awk '{print $1, $2}') == "$(stat -c '%i %Hd:%Ld' "$image")" ]]
}

[[ -d $STATE ]] && touch "$STATE/tearing-down"
rm -f "$STATE/up"

pkill -u "$TEST_UID" 2>/dev/null
wait_until 5 bash -c "! pgrep -u $TEST_UID >/dev/null" || pkill -9 -u "$TEST_UID"
release /work/strata

# Clients go first, while their servers can still answer the unmount.
for name in cifs-posix cifs-nocase cifs nfs3 nfs42; do release "$MNT/$name"; done
stop_if_busy

stop_daemon smbd

if is_mounted /proc/fs/nfsd; then
  # Unmounted v4.1+ clients destroy their client IDs; give that a moment to land.
  wait_until 15 bash -c '[[ -z $(ls /proc/fs/nfsd/clients) ]]'
  exportfs -au
  exportfs -f
  echo /srv/nfs > /proc/fs/nfsd/unlock_filesystem 2>/dev/null
  [[ $(nfsd_threads) == 0 ]] || echo 0 > /proc/fs/nfsd/threads || rpc.nfsd 0
  if ! wait_until 30 nfsd_idle; then
    log "nfsd did not stop: threads $(nfsd_threads), clients: $(echo $(nfsd_clients))"
    ((force)) || busy+=(/proc/fs/nfsd)
  fi
  stop_if_busy
fi
for daemon in rpc.mountd nfsdcld rpc.statd rpcbind; do stop_daemon "$daemon"; done
release /var/lib/nfs/rpc_pipefs
release /proc/fs/nfsd

for name in exfat-fuse ntfs-3g ntfs3 exfat vfat ext4-casefold ext4; do release "$MNT/$name"; done
for target in /srv/nfs/v42 /srv/nfs/v3 /srv/nfs /srv/smb; do release "$target"; done
stop_if_busy

loop_released() { [[ $(losetup -n -O BACK-FILE "$1" 2>/dev/null) != "$2" ]]; }
# A mounted block device refuses an exclusive open.
in_use() { ! python3 -I -c 'import os, sys; os.close(os.open(sys.argv[1], os.O_RDONLY | os.O_EXCL))' "$1" 2>/dev/null; }

if [[ -r $STATE/loops ]]; then
  pending=()
  while read -r dev majmin name; do
    [[ -b $dev ]] || continue
    if ! backed_by "$dev" "$IMAGES/$name.img"; then
      log "WARNING: $dev is no longer backed by $IMAGES/$name.img; leaving it alone"
      continue
    fi
    if in_use "$dev" && ((!force)); then
      log "$dev ($name) is still in use"
      busy+=("$dev")
      pending+=("$dev $majmin $name")
      continue
    fi
    # Blank the device before detaching so the detach uevent finds nothing.
    wipefs -a -q "$dev" >/dev/null 2>&1
    # losetup -d on a device still held only sets autoclear, so confirm it went.
    losetup -d "$dev"
    if ! wait_until 10 loop_released "$dev" "$IMAGES/$name.img"; then
      log "$dev ($name) is still attached: holders $(ls "/sys/block/${dev#/dev/}/holders" 2>/dev/null)"
      ((force)) || busy+=("$dev")
      pending+=("$dev $majmin $name")
    fi
  done < "$STATE/loops"
  ((${#pending[@]})) && printf '%s\n' "${pending[@]}" > "$STATE/loops"
fi
stop_if_busy
release /var/lib/extensions
stop_if_busy
rm -rf "$STATE"
log "teardown complete; loop devices still attached in the kernel:"
losetup -l -n -O NAME,AUTOCLEAR,BACK-FILE >&2
exit 0
