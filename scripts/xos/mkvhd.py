#!/usr/bin/env python3
"""Make a fixed-size VHD from a raw disk image.

Usage:
  mkvhd.py RAW.img OUT.vhd        copy RAW to OUT.vhd (sparse), append footer
  mkvhd.py RAW.img --in-place     append the footer to RAW itself

Raw data is padded with zeros to a multiple of 512 bytes if needed. The
512-byte footer follows the Microsoft "Virtual Hard Disk Image Format
Specification" (fixed disk, type 2, no footer copy at the start).

Stdlib only.
"""
import argparse
import os
import struct
import sys
import time
import uuid

SECTOR = 512
CHUNK = 1 << 20
COOKIE = b"conectix"
UNIX_TO_VHD_EPOCH = 946684800          # 2000-01-01 00:00:00 UTC, Unix seconds
MAX_FIXED_SIZE = 2040 * (1 << 30)      # largest VHD disk size
MAX_CHS_SECTORS = 65535 * 16 * 255


class VhdError(Exception):
    pass


def chs_geometry(total_sectors):
    """Disk geometry algorithm from the VHD spec (Appendix: CHS calculation)."""
    if total_sectors > MAX_CHS_SECTORS:
        total_sectors = MAX_CHS_SECTORS
    if total_sectors >= 65535 * 16 * 63:
        spt = 255
        heads = 16
        cth = total_sectors // spt
    else:
        spt = 17
        cth = total_sectors // spt
        heads = (cth + 1023) // 1024
        if heads < 4:
            heads = 4
        if cth >= heads * 1024 or heads > 16:
            spt = 31
            heads = 16
            cth = total_sectors // spt
        if cth >= heads * 1024:
            spt = 63
            heads = 16
            cth = total_sectors // spt
    cylinders = cth // heads
    return cylinders, heads, spt


def make_footer(size, timestamp=None):
    if timestamp is None:
        timestamp = int(time.time()) - UNIX_TO_VHD_EPOCH
    if not 0 <= timestamp <= 0xFFFFFFFF:
        raise VhdError("timestamp out of range for the VHD footer")
    cyl, heads, spt = chs_geometry(size // SECTOR)

    f = bytearray(SECTOR)
    f[0:8] = COOKIE
    struct.pack_into(">I", f, 8, 0x00000002)            # features: reserved bit set
    struct.pack_into(">I", f, 12, 0x00010000)           # file format version 1.0
    struct.pack_into(">Q", f, 16, 0xFFFFFFFFFFFFFFFF)   # data offset (fixed: none)
    struct.pack_into(">I", f, 24, timestamp)
    f[28:32] = b"fcp "                                  # creator application
    struct.pack_into(">I", f, 32, 0x00010000)           # creator version
    f[36:40] = b"Wi2k"                                  # creator host OS
    struct.pack_into(">Q", f, 40, size)                 # original size
    struct.pack_into(">Q", f, 48, size)                 # current size
    struct.pack_into(">HBB", f, 56, cyl, heads, spt)    # disk geometry
    struct.pack_into(">I", f, 60, 2)                    # disk type: fixed
    # checksum at 64..67 stays zero while summing
    f[68:84] = uuid.uuid4().bytes
    # saved state (84) and reserved (85..511) stay zero
    checksum = (~sum(f)) & 0xFFFFFFFF
    struct.pack_into(">I", f, 64, checksum)
    return bytes(f)


def has_footer(path, size):
    if size < SECTOR:
        return False
    with open(path, "rb") as f:
        f.seek(size - SECTOR)
        return f.read(8) == COOKIE


def copy_sparse(src, dst, length):
    """Copy length bytes from the start of src to the start of dst, skipping zero chunks."""
    done = 0
    while done < length:
        buf = src.read(min(CHUNK, length - done))
        if not buf:
            raise VhdError("unexpected end of raw file")
        if buf.count(0) != len(buf):
            dst.seek(done)
            dst.write(buf)
        done += len(buf)


def mkvhd(raw_path, out_path, in_place):
    size = os.path.getsize(raw_path)
    if size == 0:
        raise VhdError("raw image is empty")
    if size > MAX_FIXED_SIZE:
        raise VhdError("raw image is larger than the 2040 GiB VHD limit")
    if has_footer(raw_path, size):
        raise VhdError(f"{raw_path} already ends with a VHD footer")
    padded = (size + SECTOR - 1) // SECTOR * SECTOR
    footer = make_footer(padded)

    if in_place:
        with open(raw_path, "r+b") as f:
            f.truncate(padded)
            f.seek(padded)
            f.write(footer)
        target = raw_path
    else:
        if os.path.abspath(raw_path) == os.path.abspath(out_path):
            raise VhdError("OUT must differ from RAW (use --in-place to modify RAW)")
        with open(raw_path, "rb") as src, open(out_path, "wb") as dst:
            copy_sparse(src, dst, size)
            dst.truncate(padded)
            dst.seek(padded)
            dst.write(footer)
        target = out_path

    if os.path.getsize(target) != padded + SECTOR:
        raise VhdError("output size is wrong after writing the footer")
    return target, padded


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("raw", help="raw disk image")
    ap.add_argument("out", nargs="?", help="output .vhd (omit with --in-place)")
    ap.add_argument("--in-place", action="store_true",
                    help="append the footer to RAW instead of writing OUT")
    args = ap.parse_args(argv)
    if args.in_place and args.out is not None:
        ap.error("OUT must not be given with --in-place")
    if not args.in_place and args.out is None:
        ap.error("OUT is required unless --in-place is given")
    try:
        target, padded = mkvhd(args.raw, args.out, args.in_place)
    except (VhdError, OSError) as e:
        print(f"mkvhd: error: {e}", file=sys.stderr)
        return 1
    print(f"mkvhd: wrote {target}: fixed VHD, virtual size {padded} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
