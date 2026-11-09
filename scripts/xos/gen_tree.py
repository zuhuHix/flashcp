#!/usr/bin/env python3
"""Generate a deterministic, deliberately awkward source tree for exFAT tests.

Usage: gen_tree.py DIR    (DIR must be missing or empty)
"""
import os
import random
import sys
import unicodedata
from datetime import datetime, timezone

SEED = 20240517
BIG_FILE = "big.bin"
BIG_SIZE = 50 * 1024 * 1024
SMALL_FILES = 2000
ILLEGAL = set('"*/:<>?\\|')


def utc_ns(y, mo, d, h, mi, s, frac_ns):
    return int(datetime(y, mo, d, h, mi, s, tzinfo=timezone.utc).timestamp()) * 10**9 + frac_ns


FIXED_MTIME_NS = utc_ns(2024, 5, 17, 10, 20, 30, 450_000_000)
# Sub-second parts are multiples of 10 ms so exFAT's truncation does not change them.
TIMESTAMP_FILES = {
    "timestamps/1980-03-01.txt": utc_ns(1980, 3, 1, 0, 0, 0, 370_000_000),
    "timestamps/2000-02-29.txt": utc_ns(2000, 2, 29, 12, 34, 56, 780_000_000),
    "timestamps/2100-12-31.txt": utc_ns(2100, 12, 31, 23, 59, 58, 990_000_000),
}

BASE_DIRS = [
    "日本語", "Ελληνικά", "naïve résumé", "🎉 party", "docs", "media", "src", "data", "logs", "cache",
    "reports", "assets", "archive", "scripts", "config", "fonts", "images", "notes", "backup", "tmp-files",
]
EMPTY_DIRS = ["empty-a", "empty-b/empty-c", "docs/empty-sub", "archive/empty-deep/er"]
TOP_FILES = ["😀🎉 party.txt", "naïve résumé.txt", "日本語.txt", "Ελληνικά.txt"]
ACCENT_NAMES = ["cafe.txt", "café.txt", "cafè.txt", "cafê.txt"]  # NFC, differ only by accent
WORDS = ["report", "photo", "note", "log", "draft", "scan", "tmp", "backup"]
UNI_STEMS = ["😀", "日本", "Ελλ", "naïve", "résumé", "ßtraße"]
EXTS = [".txt", ".dat", ".bin", ".md", ""]
DEEP = "/".join(f"d{i}" for i in range(1, 9))
LONG_NAME = "xos-long-name-" + "ü日" * 10  # BMP only, no emoji
LONG_NAME += "a" * (200 - len(LONG_NAME) - 4) + ".txt"


def check_name(part):
    if (not part or unicodedata.normalize("NFC", part) != part or set(part) & ILLEGAL
            or any(ord(c) < 32 for c in part) or part.endswith((".", " "))):
        raise ValueError(f"bad name: {ascii(part)}")
    return part


class Tree:
    def __init__(self, root):
        self.root, self.files, self.dirs = root, [], set()

    def path(self, rel):
        return os.path.join(self.root, *rel.split("/"))

    def mkdir(self, rel):
        parts = [check_name(p) for p in rel.split("/")]
        for i in range(1, len(parts) + 1):
            sub = "/".join(parts[:i])
            if sub not in self.dirs:
                os.makedirs(self.path(sub), exist_ok=True)
                self.dirs.add(sub)

    def write(self, rel, data):
        head, _, leaf = rel.rpartition("/")
        if head:
            self.mkdir(head)
        check_name(leaf)
        with open(self.path(rel), "wb") as f:
            f.write(data)
        self.files.append(rel)


def main(argv):
    if len(argv) != 2:
        print(__doc__.strip())
        return 2
    root = os.path.abspath(argv[1])
    if os.path.isdir(root) and os.listdir(root):
        print(f"refusing to write into non-empty {root}")
        return 2
    os.makedirs(root, exist_ok=True)
    rng = random.Random(SEED)
    t = Tree(root)

    for d in BASE_DIRS + EMPTY_DIRS:
        t.mkdir(d)
    t.write(f"{DEEP}/leaf.txt", rng.randbytes(100))
    t.write(LONG_NAME, rng.randbytes(1000))
    for name in TOP_FILES:
        t.write(name, rng.randbytes(rng.randint(1, 4096)))
    for i, name in enumerate(ACCENT_NAMES):
        t.write(f"accents/{name}", rng.randbytes(200 + 50 * i))
    for name in TIMESTAMP_FILES:
        t.write(name, rng.randbytes(64))
    for d in BASE_DIRS:
        for n in ("empty-0.txt", "empty-1.txt"):
            t.write(f"{d}/{n}", b"")
    for i in range(SMALL_FILES):
        d = BASE_DIRS[i % len(BASE_DIRS)]
        stem = UNI_STEMS[i % len(UNI_STEMS)] if i % 53 == 0 else WORDS[i % len(WORDS)]
        t.write(f"{d}/{stem}-{i:04d}{EXTS[i % len(EXTS)]}", rng.randbytes(rng.randint(0, 8192)))

    left = BIG_SIZE  # streamed in 1 MiB pieces to keep memory flat
    with open(t.path(BIG_FILE), "wb") as f:
        while left:
            n = min(left, 1 << 20)
            f.write(rng.randbytes(n))
            left -= n
    t.files.append(BIG_FILE)

    # Timestamps last: files first, then directories deepest-first so child writes cannot bump them.
    for rel in t.files:
        ns = TIMESTAMP_FILES.get(rel, FIXED_MTIME_NS)
        os.utime(t.path(rel), ns=(ns, ns))
    for rel in sorted(t.dirs, key=lambda r: (-r.count("/"), r)):
        os.utime(t.path(rel), ns=(FIXED_MTIME_NS, FIXED_MTIME_NS))

    total = sum(os.path.getsize(t.path(f)) for f in t.files)
    print(f"gen_tree: {root}: {len(t.files)} files, {len(t.dirs)} dirs, {total} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
