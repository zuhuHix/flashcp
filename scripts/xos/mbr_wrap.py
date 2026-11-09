#!/usr/bin/env python3
"""Wrap an exFAT superfloppy volume image in a single-partition MBR.

Usage: mbr_wrap.py VOLUME.img OUT.img [--start-lba 2048]

Output layout:
  LBA 0            MBR: one primary partition, type 0x07, LBA-only CHS filler
  [1, start-lba)   zeros (sparse)
  start-lba ...    the exFAT volume, byte for byte, except that the boot
                   regions are patched:
                     * PartitionOffset (offset 64, u64) = start-lba in both
                       the main (sectors 0-11) and backup (sectors 12-23)
                       boot regions
                     * each region's boot checksum (sector 11 / 23) is
                       recomputed per Microsoft exFAT spec 3.4

The input's existing boot checksums are verified before anything is patched;
the script fails loudly if they are wrong. The output is read back and
re-verified before the script reports success.

Stdlib only. Images are copied sparsely (all-zero 1 MiB chunks are skipped and
the file is truncated to size, so holes stay holes).
"""
import argparse
import hashlib
import os
import struct
import sys

SECTOR = 512
CHUNK = 1 << 20
BOOT_SECTORS = 12          # sectors per boot region (sectors 0-11)
BOOT_BYTES = BOOT_SECTORS * SECTOR
PARTITION_OFFSET_FIELD = 64
CHECKSUM_SKIP = (106, 107, 112)  # VolumeFlags (2 bytes) and PercentInUse
U32_MAX = 0xFFFFFFFF
REGION_LABELS = ("main", "backup")


class WrapError(Exception):
    pass


def boot_checksum(region):
    """Boot checksum over the first 11 sectors of a boot region (exFAT spec 3.4)."""
    cs = 0
    for i in range(11 * SECTOR):
        if i in CHECKSUM_SKIP:
            continue
        cs = ((0x80000000 if cs & 1 else 0) + (cs >> 1) + region[i]) & U32_MAX
    return cs


def check_region(region, label, expect_offset=None):
    """Validate one 12-sector boot region. Raises WrapError on any problem."""
    if region[3:11] != b"EXFAT   ":
        raise WrapError(f"{label} boot region: no 'EXFAT   ' signature at offset 3")
    if region[510:512] != b"\x55\xaa":
        raise WrapError(f"{label} boot region: bad boot signature at offset 510")
    shift = region[108]
    if shift != 9:
        raise WrapError(
            f"{label} boot region: BytesPerSectorShift is {shift}, need 9 (512-byte sectors)"
        )
    if expect_offset is not None:
        got = struct.unpack_from("<Q", region, PARTITION_OFFSET_FIELD)[0]
        if got != expect_offset:
            raise WrapError(
                f"{label} boot region: PartitionOffset is {got}, expected {expect_offset}"
            )
    stored = struct.unpack("<128I", region[11 * SECTOR:12 * SECTOR])
    want = boot_checksum(region)
    if any(v != want for v in stored):
        raise WrapError(
            f"{label} boot region: checksum mismatch (stored 0x{stored[0]:08x}, "
            f"computed 0x{want:08x}); refusing to patch a damaged volume"
        )


def build_mbr(name, start_lba, sectors):
    mbr = bytearray(SECTOR)
    # Deterministic disk signature derived from the output file name.
    mbr[440:444] = hashlib.sha256(os.path.basename(name).encode()).digest()[:4]
    entry = struct.pack(
        "<B3sB3sII",
        0x00,              # status: not bootable
        b"\xfe\xff\xff",   # start CHS (LBA-only filler)
        0x07,              # type: NTFS/exFAT/HPFS
        b"\xfe\xff\xff",   # end CHS (LBA-only filler)
        start_lba,
        sectors,
    )
    mbr[446:462] = entry
    mbr[510:512] = b"\x55\xaa"
    return bytes(mbr)


def copy_sparse(src, dst, length, dst_base):
    """Copy `length` bytes from src to dst at dst_base, skipping all-zero chunks."""
    done = 0
    while done < length:
        buf = src.read(min(CHUNK, length - done))
        if not buf:
            raise WrapError("unexpected end of volume file")
        if buf.count(0) != len(buf):
            dst.seek(dst_base + done)
            dst.write(buf)
        done += len(buf)


def wrap(vol_path, out_path, start_lba):
    if start_lba < 1:
        raise WrapError("--start-lba must be >= 1 (LBA 0 holds the MBR)")
    if os.path.abspath(vol_path) == os.path.abspath(out_path):
        raise WrapError("OUT must differ from VOLUME")

    size = os.path.getsize(vol_path)
    if size % SECTOR:
        raise WrapError(f"volume size {size} is not a multiple of 512")
    if size < 2 * BOOT_BYTES:
        raise WrapError("volume is too small to hold two exFAT boot regions")
    sectors = size // SECTOR
    if sectors > U32_MAX or start_lba + sectors > U32_MAX:
        raise WrapError("partition does not fit in a 32-bit MBR LBA field")

    with open(vol_path, "rb") as vf:
        head = vf.read(2 * BOOT_BYTES)
    if len(head) != 2 * BOOT_BYTES:
        raise WrapError("could not read the boot regions")

    vol_length = struct.unpack_from("<Q", head, 72)[0]
    if vol_length > sectors:
        raise WrapError(f"exFAT VolumeLength ({vol_length} sectors) exceeds the file")

    # Verify both regions as they are on disk before touching anything.
    for r in range(2):
        check_region(head[r * BOOT_BYTES:(r + 1) * BOOT_BYTES], REGION_LABELS[r])

    # Build the patched boot regions.
    patched = bytearray(head)
    for r in range(2):
        base = r * BOOT_BYTES
        struct.pack_into("<Q", patched, base + PARTITION_OFFSET_FIELD, start_lba)
        cs = boot_checksum(bytes(patched[base:base + BOOT_BYTES]))
        patched[base + 11 * SECTOR:base + 12 * SECTOR] = struct.pack("<I", cs) * (SECTOR // 4)

    base = start_lba * SECTOR
    total = base + size
    mbr = build_mbr(out_path, start_lba, sectors)

    with open(vol_path, "rb") as src, open(out_path, "wb") as dst:
        dst.write(mbr)
        copy_sparse(src, dst, size, base)
        dst.seek(base)
        dst.write(bytes(patched))
        dst.truncate(total)

    # Read back and verify the output.
    with open(out_path, "rb") as f:
        out_mbr = f.read(SECTOR)
        if out_mbr != mbr:
            raise WrapError("MBR read-back mismatch")
        f.seek(base)
        out_head = f.read(2 * BOOT_BYTES)
    if len(out_head) != 2 * BOOT_BYTES:
        raise WrapError("output is shorter than expected")
    for r in range(2):
        check_region(out_head[r * BOOT_BYTES:(r + 1) * BOOT_BYTES],
                     REGION_LABELS[r] + " (output)", expect_offset=start_lba)

    return total, sectors


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("volume", help="exFAT superfloppy image (no partition table)")
    ap.add_argument("out", help="MBR-wrapped output image")
    ap.add_argument("--start-lba", type=int, default=2048,
                    help="partition start LBA (default 2048 = 1 MiB)")
    args = ap.parse_args(argv)
    try:
        total, sectors = wrap(args.volume, args.out, args.start_lba)
    except (WrapError, OSError) as e:
        print(f"mbr_wrap: error: {e}", file=sys.stderr)
        return 1
    print(f"mbr_wrap: wrote {args.out}: {total} bytes; partition type 0x07 "
          f"at LBA {args.start_lba}, {sectors} sectors; boot regions verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
