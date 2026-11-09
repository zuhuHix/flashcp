#!/usr/bin/env python3
"""Mutate a mounted copy of the test tree, then verify the resulting state.

Usage: write_test.py MOUNTPOINT MANIFEST OUT_MANIFEST

1. verify the volume matches MANIFEST
2. delete 50 files (deterministic), rename one top-level dir that holds files,
   create xos-new/ with 300 new files (each fsynced)
3. write OUT_MANIFEST describing the new state (all dir mtimes and new-file mtimes null)
4. verify the volume against OUT_MANIFEST
Exit 0 on pass, 1 on failure.
"""
import hashlib
import json
import os
import random
import sys
import unicodedata

import gen_tree
from manifest import long_path, save
from verify import verify

SEED = 1234
DELETE_COUNT = 50
NEW_DIR = "xos-new"
NEW_COUNT = 300
UNI_STEMS = [unicodedata.normalize("NFC", s) for s in ["新しい", "Νέο", "🙂 new"]]


def fs_path(mount, rel):
    return os.path.join(long_path(mount), *rel.split("/"))


def main(argv):
    if len(argv) != 3:
        print(__doc__.strip())
        return 2
    mount, man_in, man_out = argv
    with open(man_in, encoding="utf-8") as f:
        base = json.load(f)
    if not verify(base, mount, "before-write"):
        print("write_test: baseline does not match MANIFEST, not mutating")
        return 1

    rng = random.Random(SEED)
    state = {k: dict(v) for k, v in base["entries"].items()}

    protected = {gen_tree.BIG_FILE, *gen_tree.TIMESTAMP_FILES}
    candidates = sorted(k for k, v in state.items() if v["type"] == "file" and k not in protected)
    for rel in sorted(rng.sample(candidates, DELETE_COUNT)):
        os.remove(fs_path(mount, rel))
        del state[rel]

    def holds_files(d):
        return any(k.startswith(d + "/") and v["type"] == "file" for k, v in state.items())

    def holds_timestamps(d):
        return any(t.startswith(d + "/") for t in gen_tree.TIMESTAMP_FILES)

    top_dirs = sorted(k for k, v in state.items()
                      if v["type"] == "dir" and "/" not in k and holds_files(k) and not holds_timestamps(k))
    old = rng.choice(top_dirs)
    new = old + "-renamed"
    if new in state:
        raise SystemExit(f"write_test: {ascii(new)} already exists")
    os.rename(fs_path(mount, old), fs_path(mount, new))
    for k in [k for k in state if k == old or k.startswith(old + "/")]:
        state[new + k[len(old):]] = state.pop(k)

    os.mkdir(fs_path(mount, NEW_DIR))
    state[NEW_DIR] = {"type": "dir", "size": 0, "mtime_ns": None}
    for i in range(NEW_COUNT):
        stem = UNI_STEMS[i % len(UNI_STEMS)] if i % 60 == 0 else "new"
        rel = f"{NEW_DIR}/{stem}-{i:03d}.bin"
        data = rng.randbytes(rng.randint(0, 64 * 1024))
        with open(fs_path(mount, rel), "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        state[rel] = {"type": "file", "size": len(data),
                      "sha256": hashlib.sha256(data).hexdigest(), "mtime_ns": None}

    for v in state.values():
        if v["type"] == "dir":
            v["mtime_ns"] = None
    if hasattr(os, "sync"):
        os.sync()
    save(state, man_out)

    print(f"write_test: deleted {DELETE_COUNT} files, renamed {ascii(old)} -> {ascii(new)}, "
          f"created {NEW_COUNT} files in {NEW_DIR}/")
    with open(man_out, encoding="utf-8") as f:
        expected = json.load(f)
    ok = verify(expected, mount, "after-write")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
