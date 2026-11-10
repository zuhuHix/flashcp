#!/bin/bash
# Root-only checks of the drive-mode system integration, for a disposable CI VM (gui.yml).
#   test-drive-mode-root.sh sandbox <flashcp-udev-check>        the udev helper's decisions + sandbox
#   test-drive-mode-root.sh package <flashcp-gui.deb> <flashcp>  install, system off/on, remove, purge
# Never run this on a workstation: it installs packages and changes udev rules.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "needs root (CI only)"; exit 1; }
[ "${CI:-}" = true ] || { echo "refusing outside CI"; exit 1; }
fail() { echo "::error::$*"; exit 1; }

sandbox() {
  local helper; helper=$(realpath "$1")
  local w; w=$(mktemp -d); chmod 755 "$w"
  truncate -s 64M "$w/img"; mkfs.exfat -L SANDBOX "$w/img" >/dev/null
  local loop; loop=$(losetup --find --show "$w/img")
  local made_bin=""; [ -e /usr/bin/flashcp ] || { touch /usr/bin/flashcp; made_bin=1; }
  trap 'umount /usr/bin/flashcp 2>/dev/null; [ -n "'"$made_bin"'" ] && rm -f /usr/bin/flashcp; losetup -d '"$loop"'; rm -rf '"$w" RETURN
  local uid=4242
  echo "kernel $(uname -r), LSMs: $(cat /sys/kernel/security/lsm 2>/dev/null || echo unknown), $(systemctl --version | head -1)"
  # The helper runs like udev runs it: in a unit with systemd-udevd.service's seccomp filter and
  # sandbox (which deny Landlock, mount, seccomp), a fake seat0 and the user's runtime dir.
  local udevd=(-p "SystemCallFilter=@system-service @module @raw-io bpf" -p "SystemCallFilter=~@clock"
               -p SystemCallErrorNumber=EPERM -p LockPersonality=yes -p MemoryDenyWriteExecute=yes
               -p RestrictRealtime=yes -p PrivateMounts=yes -p IPAddressDeny=any
               -p "RestrictAddressFamilies=AF_UNIX AF_NETLINK AF_INET AF_INET6"
               -p "TemporaryFileSystem=/run/systemd/seats /run/user")
  # Prove the stand-in really is udevd's context: Landlock is denied there.
  systemd-run --quiet --wait --pipe "${udevd[@]}" /usr/bin/python3 -I -c \
    'import ctypes,sys; l=ctypes.CDLL(None,use_errno=True); r=l.syscall(444,None,0,1); sys.exit(0 if r<0 and ctypes.get_errno()==1 else 1)' \
    || fail "the udevd-like unit does not deny Landlock"
  # A fake /usr/bin/flashcp (bind-mounted over the real path) that exits 0 only if the sandbox holds.
  cat > "$w/ok" <<EOF
#!/bin/sh
[ "\$(id -u)" -ge 61184 ] && [ "\$(id -u)" -le 65519 ] || exit 10   # a DynamicUser uid
[ -z "\$ID_FS_UUID\$PKEXEC_UID\$FLASHCP_UDEV_CHECK_DEBUG" ] || exit 11
for fd in 3 4 5 6 7 8 9; do if { true >&\$fd; } 2>/dev/null; then exit 12; fi; done
[ "\$(head -c 11 <&0 | tail -c 8)" = "EXFAT   " ] || exit 13
if cat /etc/hostname >&2 2>&2; then exit 14; fi        # /etc is an empty tmpfs
if [ -e /home ] && ls /home/* >&2 2>&2; then exit 20; fi
if touch /usr/bin/x 2>/dev/null || touch /tmp/../x 2>/dev/null; then exit 21; fi
read -r _ < /proc/self/mountinfo || exit 17            # the check's busy test: mountinfo and /sys
ls /sys/class/block >&2 || exit 18
[ "\$(ulimit -f)" = 0 ] || exit 15
[ "\$1 \$2 \$3 \$4 \$5 \$6" = "mount --check --json --device-fd 0 $loop" ] || exit 16
exit 0
EOF
  printf '#!/bin/sh\necho %s\nexit 0\n' "'{\"event\":\"check\",\"ok\":true,\"busy_unknown\":\"EACCES\"}'" > "$w/unknown"
  # Network: a listener on the host's loopback must be unreachable from the check.
  cat > "$w/net" <<'EOF'
#!/usr/bin/python3 -I
import socket, sys
try:
    socket.create_connection(("127.0.0.1", 8765), 1)
except OSError:
    sys.exit(0)
sys.exit(19)
EOF
  printf '#!/bin/sh\necho %s\nexit 0\n' "'{\"event\":\"check\",\"ok\":true,\"pending_journal\":true}'" > "$w/pending"
  printf '#!/bin/sh\nexit 3\n' > "$w/refuse"
  printf '#!/bin/sh\nexit 4\n' > "$w/busy"
  printf '#!/bin/sh\nexec sleep 30\n' > "$w/hang"
  chmod 755 "$w"/ok "$w"/pending "$w"/net "$w"/unknown "$w"/refuse "$w"/busy "$w"/hang
  # run <fake> <setup shell commands> -> prints the helper's stdout
  run() {
    mount --bind "$w/$1" /usr/bin/flashcp
    systemd-run --quiet --wait --pipe --collect "${udevd[@]}" -E ID_FS_UUID=1A2B-3C4D -E FLASHCP_UDEV_CHECK_DEBUG=1 \
      /bin/sh -ec "
        printf 'ACTIVE=c9\nACTIVE_UID=$uid\n' > /run/systemd/seats/seat0
        mkdir -p /run/user/$uid/flashcp/kernel; touch /run/user/$uid/flashcp/drive-mode-on
        python3 -I -c 'import socket; socket.socket(socket.AF_UNIX).bind(\"/run/user/$uid/flashcp/drived.sock\")'
        chown -R $uid /run/user/$uid
        $2
        exec '$helper' '$loop'" </dev/null || true
    umount /usr/bin/flashcp
  }
  expect() { # name fake setup want
    local t0 got; t0=$(date +%s%N)
    got=$(run "$2" "$3" | tr '\n' ' ')
    local ms=$(( ($(date +%s%N) - t0) / 1000000 ))
    echo "udev-check [$1]: '${got}' in ${ms} ms"
    [ "$got" = "$4" ] || fail "udev-check $1: got '$got', want '$4'"
    [ "$ms" -lt 2500 ] || fail "udev-check $1 took $ms ms"
  }
  expect served ok "" "FLASHCP_DRIVE_MODE=1 FLASHCP_DRIVE_UID=$uid UDISKS_IGNORE=1 "
  expect "busy unknown" unknown "" ""
  python3 -c 'import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("127.0.0.1", 8765)); s.listen(); time.sleep(60)' &
  local listener=$!; sleep 0.5
  python3 -c 'import socket; socket.create_connection(("127.0.0.1", 8765), 1)' || fail "listener not up"
  expect "no network" net "" "FLASHCP_DRIVE_MODE=1 FLASHCP_DRIVE_UID=$uid UDISKS_IGNORE=1 "
  kill "$listener"
  expect refused refuse "" ""
  expect busy busy "" "FLASHCP_KEEP=1 "
  expect hang hang "" ""
  expect "daemon off" ok "rm /run/user/$uid/flashcp/drive-mode-on" ""
  expect "drive mode off, pending journal: recovery first" pending "rm /run/user/$uid/flashcp/drive-mode-on" \
    "FLASHCP_DRIVE_MODE=1 FLASHCP_DRIVE_UID=$uid UDISKS_IGNORE=1 FLASHCP_RECOVER=1 "
  expect "no daemon, pending journal: left to udisks" pending \
    "rm /run/user/$uid/flashcp/drive-mode-on /run/user/$uid/flashcp/drived.sock" ""
  expect "marker is a symlink" ok "rm /run/user/$uid/flashcp/drive-mode-on; ln -s /etc/passwd /run/user/$uid/flashcp/drive-mode-on" ""
  expect "marker owned by root" ok "chown 0 /run/user/$uid/flashcp/drive-mode-on" ""
  expect "fallback flag" ok "touch /run/user/$uid/flashcp/kernel/1A2B-3C4D; chown $uid /run/user/$uid/flashcp/kernel/1A2B-3C4D" "UDISKS_AUTO=0 "
  expect "fallback flag owned by root" ok "touch /run/user/$uid/flashcp/kernel/1A2B-3C4D" "FLASHCP_DRIVE_MODE=1 FLASHCP_DRIVE_UID=$uid UDISKS_IGNORE=1 "
  expect "no active user" ok "printf 'ACTIVE=c1\n' > /run/systemd/seats/seat0" ""
  # The hung check was ended by its unit's RuntimeMaxSec, not left behind.
  sleep 1.5; ! pgrep -f "sleep 30" || fail "the hung check is still running"
  # Not a device path / not a block device: nothing, without running anything.
  [ -z "$("$helper" /etc/passwd)" ] && [ -z "$("$helper" "$w/img")" ] || fail "non-device input"
}

package() {
  local deb=$1 cli=$2
  install -m 0755 "$cli" /usr/bin/flashcp
  # Everything the GUI package depends on except the CLI package (installed by hand above).
  local deps; deps=$(dpkg-deb -f "$deb" Depends | tr ',' '\n' | sed 's/^ *//; s/ *$//; s/ |.*//; s/ (.*//' | grep -vx flashcp)
  # shellcheck disable=SC2086
  apt-get install -y --no-install-recommends $deps >/dev/null
  dpkg -i --ignore-depends=flashcp "$deb"
  for f in /usr/lib/udev/rules.d/61-flashcp-drive-mode.rules /usr/libexec/flashcp-udev-check \
           /usr/libexec/flashcp-mountpoint /usr/lib/systemd/user/flashcp-drived.service \
           /usr/lib/systemd/user-preset/80-flashcp-drived.preset; do
    [ -e "$f" ] || fail "not installed: $f"
  done
  udevadm verify /usr/lib/udev/rules.d/61-flashcp-drive-mode.rules
  [ "$(systemctl --global is-enabled flashcp-drived.service)" = enabled ] || fail "user service not enabled by the preset"
  systemd-analyze --user verify /usr/lib/systemd/user/flashcp-drived.service
  # The rule is a no-op for a non-USB device (a loop device with exFAT).
  local w; w=$(mktemp -d); truncate -s 64M "$w/img"; mkfs.exfat "$w/img" >/dev/null
  local loop; loop=$(losetup --find --show "$w/img"); udevadm settle
  udevadm test "/sys/class/block/${loop#/dev/}" 2>&1 | tee "$w/test.log" | grep -q 61-flashcp-drive-mode.rules \
    || fail "udevadm test did not read the rule"
  ! grep -q flashcp-udev-check "$w/test.log" || fail "the rule ran the check for a non-USB device"
  udevadm info --query=property "$loop" | grep -q FLASHCP_ && fail "a non-USB device was tagged"
  losetup -d "$loop"; rm -rf "$w"
  # The system-wide switch (the rollback command) and back.
  flashcp drive-mode off --system --json | tee /dev/stderr | grep -q '"system_enabled":false'
  [ -e /etc/flashcp/drive-mode-disabled ] || fail "off --system did not create the flag"
  flashcp drive-mode on --system --json | grep -q '"system_enabled":true'
  [ ! -e /etc/flashcp/drive-mode-disabled ] || fail "on --system did not remove the flag"
  # Uninstall restores normal automount: rule gone, user service disabled, nothing left over.
  dpkg -r flashcp-gui
  [ ! -e /usr/lib/udev/rules.d/61-flashcp-drive-mode.rules ] || fail "rule left after remove"
  [ "$(systemctl --global is-enabled flashcp-drived.service 2>/dev/null || true)" != enabled ] || fail "service still enabled"
  dpkg -P flashcp-gui
  echo "package: install, system off/on, remove and purge OK"
}

"$@"
