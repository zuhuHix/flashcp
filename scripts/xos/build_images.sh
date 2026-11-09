#!/usr/bin/env bash
# Build every cross-OS test volume from one tricky source tree.
#
# Usage: scripts/xos/build_images.sh FLASHCP_BIN OUT_DIR
#
# For each volume VOL it writes OUT_DIR/VOL/{VOL.img, VOL.vhd, manifest.json}:
#   VOL.img  raw disk: MBR + one type-0x07 partition at LBA 2048 holding the flashcp volume
#   VOL.vhd  the same disk as a fixed VHD (raw + 512-byte footer)
# and OUT_DIR/superfloppy/ with the bare flashcp volume (no partition table) as .img and .vhd.
# Each volume is also checked here first: fsck.exfat -n, then (with sudo) a loop mount + verify.py,
# which validates the verifier itself against the Linux kernel driver.
set -euo pipefail

FLASHCP=$(realpath "$1")
OUT=$(realpath -m "$2")
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
WORK="$OUT/.work"
mkdir -p "$WORK"

python3 "$HERE/gen_tree.py" "$WORK/src"
python3 "$HERE/manifest.py" "$WORK/src" "$WORK/manifest.json"

# name  size  extra flashcp args
VOLUMES=(
  "prefix-auto 512M --fat prefix"
  "full-auto 512M --fat full"
  "prefix-4k 512M --fat prefix --cluster 4096"
  "prefix-128k 512M --fat prefix --cluster 131072"
  "prefix-1g 1G --fat prefix"
)

check_linux() { # $1 = raw exFAT volume image (no MBR)
  fsck.exfat -n "$1"
  if sudo -n true 2>/dev/null; then
    local mp; mp=$(mktemp -d)
    sudo mount -o loop,ro "$1" "$mp"
    sudo python3 "$HERE/verify.py" "$WORK/manifest.json" "$mp" --label "linux:$(basename "$1")" || { sudo umount "$mp"; return 1; }
    sudo umount "$mp"
    rmdir "$mp"
  else
    echo "no passwordless sudo: skipping Linux mount + verify"
  fi
}

for spec in "${VOLUMES[@]}"; do
  read -r name size args <<<"$spec"
  echo "::group::volume $name ($size, $args)"
  vol="$WORK/$name.vol"
  rm -f "$vol"
  truncate -s "$size" "$vol"
  # shellcheck disable=SC2086
  "$FLASHCP" build "$WORK/src" "$vol" $args --label XOSTEST --yes-erase
  check_linux "$vol"
  mkdir -p "$OUT/$name"
  python3 "$HERE/mbr_wrap.py" "$vol" "$OUT/$name/$name.img"
  sfdisk -d "$OUT/$name/$name.img"
  # The partition inside the MBR image must still be a clean exFAT volume (PartitionOffset patched).
  dd if="$OUT/$name/$name.img" of="$WORK/part.chk" bs=1M skip=1 conv=sparse status=none
  fsck.exfat -n "$WORK/part.chk"
  rm -f "$WORK/part.chk"
  python3 "$HERE/mkvhd.py" "$OUT/$name/$name.img" "$OUT/$name/$name.vhd"
  cp "$WORK/manifest.json" "$OUT/$name/"
  if [ "$name" = prefix-auto ]; then
    mkdir -p "$OUT/superfloppy"
    cp --sparse=always "$vol" "$OUT/superfloppy/superfloppy.img"
    python3 "$HERE/mkvhd.py" "$vol" "$OUT/superfloppy/superfloppy.vhd"
    cp "$WORK/manifest.json" "$OUT/superfloppy/"
  fi
  rm -f "$vol"
  echo "::endgroup::"
done

rm -rf "$WORK"
ls -lsR "$OUT"
