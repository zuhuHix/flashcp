#!/usr/bin/env bash
# Build every cross-OS test volume from one tricky source tree.
#
# Usage: scripts/xos/build_images.sh FLASHCP_BIN OUT_DIR
#
# For each volume VOL it writes OUT_DIR/VOL/{VOL.img, VOL.vhd, manifest.json}:
#   VOL.img  raw disk: MBR + one type-0x07 partition at LBA 2048 holding the flashcp volume
#   VOL.vhd  the same disk as a fixed VHD (raw + 512-byte footer)
# and OUT_DIR/superfloppy/ with the bare flashcp volume (no partition table) as .img and .vhd.
# With passwordless sudo it also writes control-mkfs (mkfs.exfat + Linux cp) and the recl-* volumes
# (kernel-made, then `flashcp recluster`; see recluster_images.sh).
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
  "prefix-512 512M --fat prefix --cluster 512"
  "prefix-1k 512M --fat prefix --cluster 1024"
  "prefix-2k 512M --fat prefix --cluster 2048"
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

# Control: the same tree written by mkfs.exfat + the Linux kernel driver (not by flashcp), so a
# behaviour seen on another OS can be attributed to flashcp or to exFAT on that OS in general.
if sudo -n true 2>/dev/null; then
  echo "::group::volume control-mkfs (mkfs.exfat + Linux cp)"
  vol="$WORK/control-mkfs.vol"
  truncate -s 512M "$vol"
  mkfs.exfat -L XOSTEST "$vol"
  mp=$(mktemp -d)
  sudo mount -o loop "$vol" "$mp"
  sudo cp -r --preserve=timestamps "$WORK/src/." "$mp/"
  sudo umount "$mp"
  check_linux "$vol" || echo "control volume does not match the manifest on Linux (reported, not fatal)"
  mkdir -p "$OUT/control-mkfs"
  python3 "$HERE/mbr_wrap.py" "$vol" "$OUT/control-mkfs/control-mkfs.img"
  python3 "$HERE/mkvhd.py" "$OUT/control-mkfs/control-mkfs.img" "$OUT/control-mkfs/control-mkfs.vhd"
  cp "$WORK/manifest.json" "$OUT/control-mkfs/"
  rm -f "$vol"
  echo "::endgroup::"
fi

# Re-clustered volumes: made on loop devices (kernel mkfs.exfat + mount + fill), then
# `flashcp recluster` on the block device. Needs sudo for losetup/mount, like control-mkfs.
if sudo -n true 2>/dev/null; then
  bash "$HERE/recluster_images.sh" "$FLASHCP" "$OUT" "$WORK"
else
  echo "no passwordless sudo: skipping the recl-* volumes (loop devices and mounts need root)"
fi

rm -rf "$WORK"
ls -lsR "$OUT"
