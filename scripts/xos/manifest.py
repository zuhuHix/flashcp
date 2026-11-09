#!/usr/bin/env python3
"""Walk a tree and write manifest.json (stdlib only).

Usage: manifest.py SRC OUT.json
"""
import hashlib
import json
import os
import sys
import unicodedata

CHUNK = 1 << 20
JUNK_NAMES = {"System Volume Information", ".fseventsd", ".Spotlight-V100", ".Trashes", ".TemporaryItems"}


def is_junk(name):
    return name in JUNK_NAMES or name.startswith("._")


def long_path(p):
    """On Windows return the \\\\?\\ form so long paths work. No-op elsewhere."""
    if os.name != "nt":
        return p
    p = os.path.abspath(p)
    if p.startswith("\\\\?\\"):
        return p
    if p.startswith("\\\\"):
        return "\\\\?\\UNC\\" + p[2:]
    return "\\\\?\\" + p


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(CHUNK):
            h.update(chunk)
    return h.hexdigest()


def scan(root, hash_files=True):
    """Walk root. Returns (entries, notes).

    entries: {NFC relpath ("/" separated): {"type", "size", "mtime_ns"[, "sha256"]}}
    notes:   {"non_nfc": on-disk names that are not NFC, "dupes": names colliding after NFC}
    Junk entries at the top level are skipped.
    """
    base = long_path(root)
    entries, non_nfc, dupes = {}, [], []
    todo = [()]
    while todo:
        parts = todo.pop()
        here = os.path.join(base, *parts)
        with os.scandir(here) as it:
            children = sorted(it, key=lambda e: e.name)
        for e in children:
            if not parts and is_junk(e.name):
                continue
            raw = "/".join(parts + (e.name,))
            rel = unicodedata.normalize("NFC", raw)
            if rel != raw:
                non_nfc.append(raw)
            if rel in entries:
                dupes.append(raw)
                continue
            st = e.stat(follow_symlinks=False)
            if e.is_dir(follow_symlinks=False):
                entries[rel] = {"type": "dir", "size": 0, "mtime_ns": st.st_mtime_ns}
                todo.append(parts + (e.name,))
            elif e.is_file(follow_symlinks=False):
                entry = {"type": "file", "size": st.st_size, "mtime_ns": st.st_mtime_ns}
                if hash_files:
                    entry["sha256"] = sha256_file(os.path.join(here, e.name))
                entries[rel] = entry
            else:
                entries[rel] = {"type": "other", "size": 0, "mtime_ns": st.st_mtime_ns}
    return entries, {"non_nfc": non_nfc, "dupes": dupes}


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def save(entries, path):
    doc = {"entries": {k: entries[k] for k in sorted(entries)}}
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        json.dump(doc, f, ensure_ascii=False, sort_keys=True, indent=1)
        f.write("\n")


def main(argv):
    if len(argv) != 3:
        print(__doc__.strip())
        return 2
    entries, notes = scan(argv[1])
    if notes["dupes"]:
        print(f"error: names collide after NFC normalization: {[ascii(d) for d in notes['dupes']]}")
        return 1
    bad = [k for k, v in entries.items() if v["type"] not in ("file", "dir")]
    if bad:
        print(f"error: unsupported entries (symlinks/devices?): {[ascii(b) for b in bad[:10]]}")
        return 1
    if notes["non_nfc"]:
        print(f"warning: {len(notes['non_nfc'])} source names are not NFC; manifest keys are NFC")
    save(entries, argv[2])
    nf = sum(v["type"] == "file" for v in entries.values())
    print(f"manifest: {nf} files, {len(entries) - nf} dirs -> {argv[2]}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
