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
  local w; w=$(mktemp -d)
  truncate -s 64M "$w/img"; mkfs.exfat -L SANDBOX "$w/img" >/dev/null
  local loop; loop=$(losetup --find --show "$w/img")
  trap 'losetup -d '"$loop"'; rm -rf '"$w" RETURN
  local uid=4242
  # A fake /usr/bin/flashcp that exits 0 only if the sandbox holds (bind-mounted in a private
  # mount namespace, together with a fake seat0 and the user's runtime dir).
  cat > "$w/ok" <<EOF
#!/bin/sh
[ "\$(id -u)" = 65534 ] && [ "\$(id -g)" = 65534 ] && [ -z "\$(id -G | tr -d ' 65534')" ] || exit 10
[ -z "\$HOME\$ID_FS_UUID\$PKEXEC_UID" ] || exit 11
for fd in 4 5 6 7 8 9; do if { true >&\$fd; } 2>/dev/null; then exit 12; fi; done
[ "\$(head -c 11 <&3 | tail -c 8)" = "EXFAT   " ] || exit 13
if cat /etc/hostname >/dev/null 2>&1; then exit 14; fi
read -r _ < /proc/self/mountinfo || exit 17   # the check's busy test: own mountinfo and /sys
ls /sys/class/block >/dev/null || exit 18
[ "\$(ulimit -f)" = 0 ] || exit 15
[ "\$1 \$2 \$3 \$4 \$5 \$6" = "mount --check --json --device-fd 3 $loop" ] || exit 16
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
  printf '#!/bin/sh\nexit 3\n' > "$w/refuse"
  printf '#!/bin/sh\nexit 4\n' > "$w/busy"
  printf '#!/bin/sh\nexec sleep 30\n' > "$w/hang"
  chmod 755 "$w"/ok "$w"/net "$w"/unknown "$w"/refuse "$w"/busy "$w"/hang
  # run <fake> <setup shell commands> -> prints the helper's stdout
  run() {
    local ll=""; [ -n "${NO_LL:-}" ] && ll="FLASHCP_UDEV_CHECK_NO_LANDLOCK=1"
    unshare -m sh -ec "
      mount -t tmpfs none /run/systemd 2>/dev/null || { mkdir -p /run/systemd && mount -t tmpfs none /run/systemd; }
      mkdir -p /run/systemd/seats; printf 'ACTIVE=c9\nACTIVE_UID=$uid\n' > /run/systemd/seats/seat0
      mkdir -p /run/user; mount -t tmpfs none /run/user
      mkdir -p /run/user/$uid/flashcp/kernel; touch /run/user/$uid/flashcp/drive-mode-on
      chown -R $uid /run/user/$uid
      touch /usr/bin/flashcp 2>/dev/null || true
      mount --bind '$w/$1' /usr/bin/flashcp
      $2
      exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-   # like udev: only 0-2 open, so the device lands on fd 3
      ID_FS_UUID=1A2B-3C4D HOME=/root $ll '$helper' '$loop'"
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
  NO_LL=1 expect "no Landlock: fail closed" ok "" ""
  expect refused refuse "" ""
  expect busy busy "" "FLASHCP_KEEP=1 "
  expect hang hang "" ""
  expect "daemon off" ok "rm /run/user/$uid/flashcp/drive-mode-on" ""
  expect "marker is a symlink" ok "rm /run/user/$uid/flashcp/drive-mode-on; ln -s /etc/passwd /run/user/$uid/flashcp/drive-mode-on" ""
  expect "marker owned by root" ok "chown 0 /run/user/$uid/flashcp/drive-mode-on" ""
  expect "fallback flag" ok "touch /run/user/$uid/flashcp/kernel/1A2B-3C4D; chown $uid /run/user/$uid/flashcp/kernel/1A2B-3C4D" "UDISKS_AUTO=0 "
  expect "fallback flag owned by root" ok "touch /run/user/$uid/flashcp/kernel/1A2B-3C4D" "FLASHCP_DRIVE_MODE=1 FLASHCP_DRIVE_UID=$uid UDISKS_IGNORE=1 "
  expect "no active user" ok "printf 'ACTIVE=c1\n' > /run/systemd/seats/seat0" ""
  # The hung child was SIGKILLed, not left behind.
  sleep 0.5; ! pgrep -u 65534 -f "sleep 30" || fail "the hung check child is still running"
  # Not a device path / not a block device: nothing, without running anything.
  [ -z "$("$helper" /etc/passwd)" ] && [ -z "$("$helper" "$w/img")" ] || fail "non-device input"
}

package() {
  local deb=$1 cli=$2
  install -m 0755 "$cli" /usr/bin/flashcp
  apt-get install -y --no-install-recommends udisks2 fuse3 >/dev/null
  dpkg -i --ignore-depends=flashcp "$deb" || apt-get install -f -y
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
