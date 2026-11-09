#!/usr/bin/env bash
# Attach a flashcp raw disk image on macOS, fsck_exfat -n it, mount, verify against the manifest,
# run a write test, unmount, fsck_exfat -n again, detach.
#
#   scripts/xos/mac_check.sh IMG MANIFEST LABEL LOGDIR [superfloppy]
#
# Exit 0 only if every step passes.
set -uo pipefail
IMG=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
MANIFEST=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
LABEL=$3
LOGDIR=$4
SUPER=${5:-}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$LOGDIR"
LOGDIR=$(cd "$LOGDIR" && pwd)
SUMMARY=${GITHUB_STEP_SUMMARY:-/dev/null}
report() { echo "$*"; echo "$*" >>"$SUMMARY"; }
ok=1

attach_out=$(hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount "$IMG" 2>&1)
echo "$attach_out"
disk=$(echo "$attach_out" | awk '/^\/dev\/disk[0-9]+[[:space:]]/{print $1; exit}')
if [ -z "$disk" ]; then report "- \`$LABEL\`: **hdiutil attach failed**"; exit 1; fi
diskutil list "$disk"
if [ -n "$SUPER" ]; then dev=$disk; else dev=${disk}s1; fi
rdev=${dev/\/dev\/disk//dev/rdisk}
cleanup() { diskutil unmount force "$dev" >/dev/null 2>&1; hdiutil detach "$disk" -force >/dev/null 2>&1; }
trap cleanup EXIT

run_fsck() { # $1 = tag
  local log="$LOGDIR/fsck_exfat-$LABEL-$1.txt"
  sudo fsck_exfat -n "$rdev" >"$log" 2>&1
  local code=$?
  echo "----- fsck_exfat -n $rdev ($1), exit $code -----"; cat "$log"
  if [ $code -ne 0 ]; then
    report "- \`$LABEL\` fsck_exfat ($1): **FAIL** (exit $code)"
    { echo '```'; tail -n 25 "$log"; echo '```'; } | tee -a "$SUMMARY"
    ok=0
  else
    report "- \`$LABEL\` fsck_exfat ($1): OK"
  fi
}

if [ ! -e "$dev" ]; then
  report "- \`$LABEL\`: **no device $dev** after attach (macOS did not see the partition)"
  exit 1
fi
diskutil info "$dev" | sed -n '1,40p' || true

run_fsck fresh

mp=$(mktemp -d /tmp/xos-mnt.XXXXXX)
if ! diskutil mount -mountPoint "$mp" "$dev"; then
  if ! sudo mount -t exfat "$dev" "$mp"; then
    report "- \`$LABEL\`: **mount failed**"
    exit 1
  fi
fi
report "- \`$LABEL\`: mounted at $mp ($(diskutil info "$dev" | awk -F': *' '/File System Personality/{print $2}'))"

if python3 "$HERE/verify.py" "$MANIFEST" "$mp" --label "macos:$LABEL" 2>&1 | tee "$LOGDIR/verify-$LABEL.txt"; then
  report "- \`$LABEL\` verify: OK"
else
  report "- \`$LABEL\` verify: **FAIL** (see verify-$LABEL.txt)"; ok=0
fi

if python3 "$HERE/write_test.py" "$mp" "$MANIFEST" "$LOGDIR/after-$LABEL.json" 2>&1 | tee "$LOGDIR/writetest-$LABEL.txt"; then
  report "- \`$LABEL\` write test: OK"
else
  report "- \`$LABEL\` write test: **FAIL**"; ok=0
fi
sync
diskutil unmount "$dev" || sudo umount "$mp" || { report "- \`$LABEL\`: **unmount failed**"; ok=0; }

run_fsck after-write

[ $ok = 1 ]
