#!/usr/bin/env bash
# Build the re-clustered cross-OS volumes "the kernel way" (called by build_images.sh).
#
# Usage: scripts/xos/recluster_images.sh FLASHCP_BIN OUT_DIR WORK_DIR
#        sudo scripts/xos/recluster_images.sh fragfill MOUNTPOINT SRC_TREE CLUSTER_BYTES
#
# WORK_DIR must hold src/ (gen_tree.py output) and manifest.json, as build_images.sh leaves it.
# Needs passwordless sudo, sfdisk, losetup, mkfs.exfat, fsck.exfat, python3.
#
# Each volume is made on a REAL loop device: the Linux kernel mounts, fills and unmounts it, then
# `flashcp recluster` runs on the block device. The image holds exactly the bytes recluster wrote;
# it is not passed through mbr_wrap.py (which would recompute the boot checksums).
#   MBR volumes:        one type-0x07 partition at LBA 2048, /dev/loopNp1 is the volume
#   superfloppy volumes: no partition table, /dev/loopN is the volume
# Output per volume, like build_images.sh: OUT/NAME/{NAME.img, NAME.vhd, manifest.json}.
#
# The `fragfill` mode (also used by ci.yml) runs as root on an already mounted volume and leaves
# the tree on it with FAT chains instead of contiguous runs.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

# Fill MOUNTPOINT so that the tree's files end up fragmented: fill it completely with files of 8
# clusters (no free run is left anywhere), delete every other one, copy the tree (it can only use
# the 8-cluster holes, so a bigger file is chained through many of them), then delete the rest of
# the filler. The tree must fit in half of the volume.
fragfill() { # $1 mountpoint  $2 source tree  $3 cluster bytes
  local mp=$1 src=$2 cl=$3
  local fsz=$((cl * 8)) n=0 i
  mkdir "$mp/filler"
  while head -c "$fsz" /dev/zero >"$mp/filler/f$n" 2>/dev/null; do n=$((n + 1)); done
  rm -f "$mp/filler/f$n" # the one that did not fit
  echo "fragfill: $n filler files of $fsz bytes filled $mp (cluster $cl)"
  [ "$n" -ge 16 ] || { echo "fragfill: the volume took only $n filler files" >&2; exit 1; }
  sync
  for ((i = 1; i < n; i += 2)); do rm "$mp/filler/f$i"; done
  sync
  cp -r --preserve=timestamps "$src/." "$mp/"
  sync
  # Report only: how many pieces big.bin was chained into (needs FIBMAP or FIEMAP on exFAT).
  if command -v filefrag >/dev/null && [ -f "$mp/big.bin" ]; then
    filefrag "$mp/big.bin" || echo "filefrag could not map big.bin (reported, not fatal)"
  fi
  rm -r "$mp/filler"
  sync
}

if [ "${1:-}" = fragfill ]; then
  shift
  fragfill "$@"
  exit 0
fi

[ $# -eq 3 ] || { echo "usage: $0 FLASHCP_BIN OUT_DIR WORK_DIR" >&2; exit 2; }
FLASHCP=$(realpath "$1")
OUT=$(realpath -m "$2")
WORK=$(realpath "$3")
[ -d "$WORK/src" ] && [ -f "$WORK/manifest.json" ] || { echo "$WORK needs src/ and manifest.json" >&2; exit 2; }
for tool in sfdisk losetup mkfs.exfat fsck.exfat python3 sudo; do
  command -v "$tool" >/dev/null || { echo "::error::$tool is required for the recl-* volumes"; exit 1; }
done

# name  size  cluster  layout (part|whole)  fill (cp|frag|build)  --fat (default|prefix|full)  expect
# fill: cp / frag = mkfs.exfat, then the kernel copies the tree (plainly, or fragmented); build =
# `flashcp build` writes the volume (no mkfs, no mount).
# expect: moves = "switches":2 and move_bytes > 0, sw2 = "switches":2, frag = the plan counts
# fragmented files, crash = stop after the main boot sector of the final switch (see
# FLASHCP_TEST_RECLUSTER_STOP), control = no recluster at all.
#
# The two volumes without a partition table: Windows only shows such a disk image when its
# sector 0 cannot be taken for an MBR. mkfs.exfat leaves zeros where an MBR has its partition
# table (an empty table: Windows shows no volume), flashcp fills the boot code area with 0xF4.
# So recl-superfloppy starts from a `flashcp build` volume, and control-mkfs-superfloppy (never
# re-clustered, report-only) shows what Windows does with mkfs.exfat's own.
VOLUMES=(
  "recl-32k 1G 32K part cp default moves"
  "recl-128k 2G 128K part cp default moves"
  "recl-frag-32k 1G 32K part frag full frag"
  "recl-frag-128k 2G 128K part frag default frag"
  "recl-superfloppy 2G 128K whole build default sw2"
  "recl-crash-window 1G 32K part cp default crash"
  "control-mkfs-superfloppy 512M 32K whole cp default control"
)

MP=""    # current mount point, if any
LOOP=""  # current loop device, if any
DEV=""   # the volume's block device: ${LOOP}p1 or $LOOP

cleanup() {
  set +e
  if [ -n "$MP" ]; then
    if sudo mountpoint -q "$MP"; then sudo umount "$MP" || sudo umount -l "$MP"; fi
    rmdir "$MP"
    MP=""
  fi
  if [ -n "$LOOP" ]; then
    sudo losetup -d "$LOOP"
    LOOP=""
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

die() { echo "::error::$*"; exit 1; }

expect_eq() { # what actual wanted
  [ "$2" = "$3" ] || die "$1: expected '$3', got '$2'"
}

jget() { # FILE EVENT KEY: KEY of the first JSON line in FILE with "event":EVENT (EVENT "-" = any line)
  python3 -I - "$@" <<'PY'
import json, sys
path, event, key = sys.argv[1:4]
for line in open(path, errors="replace"):
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        d = json.loads(line)
    except ValueError:
        continue
    if (event == "-" or d.get("event") == event) and key in d:
        v = d[key]
        print(str(v).lower() if isinstance(v, bool) else v)
        break
else:
    sys.exit(1)
PY
}

attach() { # $1 disk image, $2 part|whole: sets LOOP and DEV
  local i
  if [ "$2" = part ]; then
    LOOP=$(sudo losetup -fP --show "$1")
    DEV=${LOOP}p1
    for i in $(seq 1 50); do
      [ -b "$DEV" ] && break
      sudo udevadm settle 2>/dev/null || true
      sleep 0.2
    done
    [ -b "$DEV" ] || die "$DEV did not appear (partition scan of $LOOP)"
  else
    LOOP=$(sudo losetup -f --show "$1")
    DEV=$LOOP
  fi
  echo "attached $1 as $LOOP (volume $DEV)"
}

detach() {
  sync
  sudo losetup -d "$LOOP"
  LOOP=""
  DEV=""
}

mount_dev() { # $1 device, $2 optional mount options
  MP=$(mktemp -d)
  sudo mount ${2:+-o "$2"} "$1" "$MP"
}

umount_dev() {
  sudo umount "$MP"
  rmdir "$MP"
  MP=""
}

verify_dev() { # $1 device  $2 label: kernel mount (ro) + verify.py against the manifest
  mount_dev "$1" ro
  sudo python3 "$HERE/verify.py" "$WORK/manifest.json" "$MP" --label "$2"
  umount_dev
}

# Run a command that is expected to exit with a given code; prints nothing else, sets RC.
RC=0
run_rc() { RC=0; "$@" || RC=$?; }

build_one() { # name size cluster layout fill fat expect
  local name=$1 size=$2 cluster=$3 layout=$4 fill=$5 fat=$6 want=$7
  local disk="$WORK/$name.disk" log="$WORK/$name.recluster.log" fatargs=() h1 h2 cl
  [ "$fat" = default ] || fatargs=(--fat "$fat")
  echo "::group::volume $name ($size, mkfs -c $cluster, $layout, fill $fill, fat $fat, expect $want)"

  rm -f "$disk"
  truncate -s "$size" "$disk"
  if [ "$layout" = part ]; then
    printf 'label: dos\nlabel-id: 0x20240517\nstart=2048, type=7\n' | sfdisk --quiet "$disk"
  fi
  attach "$disk" "$layout"
  if [ "$fill" = build ]; then
    sudo "$FLASHCP" build "$WORK/src" "$DEV" --cluster "$(cluster_bytes "$cluster")" --label XOSTEST --yes-erase
  else
    sudo mkfs.exfat -c "$cluster" -L XOSTEST "$DEV"
    mount_dev "$DEV"
    if [ "$fill" = frag ]; then
      # filler files are 8 clusters long
      sudo bash "$HERE/recluster_images.sh" fragfill "$MP" "$WORK/src" "$(cluster_bytes "$cluster")"
    else
      sudo cp -r --preserve=timestamps "$WORK/src/." "$MP/"
    fi
    umount_dev
  fi

  # The volume must be right BEFORE the recluster, so a fill problem is never blamed on it.
  sudo fsck.exfat -n "$DEV"
  verify_dev "$DEV" "linux-before:$name"

  if [ "$want" != control ]; then
    run_rc sudo "$FLASHCP" recluster --check "$DEV"
    expect_eq "$name: recluster --check before" "$RC" 0
  fi

  if [ "$want" = control ]; then
    echo "$name: a control, not re-clustered"
  elif [ "$want" = crash ]; then
    # The state a crash at the worst moment leaves: main boot region = new volume, backup = old.
    run_rc sudo env FLASHCP_TEST_RECLUSTER_STOP=reclustered "$FLASHCP" recluster "$DEV" "${fatargs[@]}" --json >"$log" 2>&1
    cat "$log"
    expect_eq "$name: exit code of the stopped recluster" "$RC" 1
    grep -q 'FLASHCP_TEST_RECLUSTER_STOP' "$log" || die "$name: the stop hook's message is missing"
    run_rc sudo "$FLASHCP" recluster --check --json "$DEV" >"$log.check" 2>&1
    cat "$log.check"
    echo "recluster --check after the stop: exit $RC"
    expect_eq "$name: finish flag" "$(jget "$log.check" check finish)" true
    sudo "$FLASHCP" info --json "$DEV" || true
    if sudo fsck.exfat -n "$DEV"; then
      echo "fsck.exfat -n on the crash-window volume: clean"
    else
      echo "fsck.exfat -n on the crash-window volume: reports problems (reported, not fatal)"
    fi
    verify_dev "$DEV" "linux-after:$name"
  else
    sudo "$FLASHCP" recluster "$DEV" "${fatargs[@]}" --json | tee "$log"
    expect_eq "$name: done.changed" "$(jget "$log" done changed)" true
    expect_eq "$name: done.cluster" "$(jget "$log" done cluster)" 4096
    case "$want" in
      moves)
        expect_eq "$name: plan.switches" "$(jget "$log" plan switches)" 2
        [ "$(jget "$log" plan move_bytes)" -gt 0 ] || die "$name: move_bytes is 0, no file data was moved"
        ;;
      sw2) expect_eq "$name: plan.switches" "$(jget "$log" plan switches)" 2 ;;
      frag)
        [ "$(jget "$log" plan fragmented)" -gt 0 ] || die "$name: no file is fragmented, the fill did not work"
        ;;
    esac
    sudo "$FLASHCP" info --json "$DEV" >"$log.info"
    cat "$log.info"
    cl=$(jget "$log.info" - cluster)
    expect_eq "$name: info cluster" "$cl" 4096

    # Nothing left to gain: --check says no (exit 3), a second run changes nothing.
    run_rc sudo "$FLASHCP" recluster --check "$DEV"
    expect_eq "$name: recluster --check after" "$RC" 3
    h1=$(sudo head -c 64M "$DEV" | sha256sum)
    sudo "$FLASHCP" recluster "$DEV" --json | tee "$log.again"
    expect_eq "$name: second run changed" "$(jget "$log.again" done changed)" false
    expect_eq "$name: second run written" "$(jget "$log.again" done written)" 0
    h2=$(sudo head -c 64M "$DEV" | sha256sum)
    expect_eq "$name: first 64 MiB after the second run" "$h2" "$h1"

    sudo fsck.exfat -n "$DEV"
    verify_dev "$DEV" "linux-after:$name"
  fi
  detach

  # Zero runs (the deleted filler) become holes again, so the artifact stays small.
  fallocate --dig-holes "$disk" || true
  mkdir -p "$OUT/$name"
  mv "$disk" "$OUT/$name/$name.img"
  python3 "$HERE/mkvhd.py" "$OUT/$name/$name.img" "$OUT/$name/$name.vhd"
  cp "$WORK/manifest.json" "$OUT/$name/"
  echo "::endgroup::"
}

cluster_bytes() { # 32K -> 32768
  case "$1" in
    *K) echo $((${1%K} * 1024)) ;;
    *M) echo $((${1%M} * 1024 * 1024)) ;;
    *) echo "$1" ;;
  esac
}

for spec in "${VOLUMES[@]}"; do
  read -r name size cluster layout fill fat want <<<"$spec"
  build_one "$name" "$size" "$cluster" "$layout" "$fill" "$fat" "$want"
done
