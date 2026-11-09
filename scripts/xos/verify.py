#!/usr/bin/env python3
"""Check a mounted exFAT volume against manifest.json (stdlib only).

Usage: verify.py MANIFEST MOUNTPOINT [--label NAME] [--no-mtime-dirs]
Exit 0 on pass, 1 on failure.
"""
import argparse
import sys

from manifest import load, scan

TICK_NS = 10_000_000       # exFAT mtime resolution: 10 ms, truncated
TZ_STEP_NS = 900 * 10**9   # UTC offsets are whole multiples of 15 minutes
WARN_NS = 2 * 10**9        # drift up to 2 s is a warning, not a failure
MAX_LINES = 50


def mtime_verdict(actual, want):
    """None if OK, else (level, message) where level is 'warn' or 'fail'."""
    diff = actual - want // TICK_NS * TICK_NS
    if abs(diff) <= TICK_NS:
        return None
    k = round(diff / TZ_STEP_NS)
    if k and abs(diff - k * TZ_STEP_NS) <= WARN_NS:
        return "fail", f"TIMEZONE: off by {diff / 1e9:+.3f} s ({k * 15:+d} min UTC offset applied or ignored)"
    if abs(diff) <= WARN_NS:
        return "warn", f"mtime off by {diff / 1e6:+.3f} ms (driver dropped the 10 ms field?)"
    return "fail", f"MTIME: off by {diff / 1e9:+.3f} s"


def verify(manifest, mountpoint, label="volume", no_mtime_dirs=False):
    want = manifest["entries"]
    try:
        found, notes = scan(mountpoint)
    except OSError as e:
        print(f"VERIFY {label}: FAIL cannot read {ascii(mountpoint)}: {e}")
        return False

    problems, warnings = [], []
    for rel in sorted(set(want) | set(found)):
        w, f, name = want.get(rel), found.get(rel), ascii(rel)
        if f is None:
            problems.append(f"MISSING {name} ({w['type']})")
        elif w is None:
            problems.append(f"EXTRA {name} ({f['type']})")
        elif w["type"] != f["type"]:
            problems.append(f"TYPE {name}: expected {w['type']}, found {f['type']}")
        else:
            if w["type"] == "file" and w["size"] != f["size"]:
                problems.append(f"SIZE {name}: expected {w['size']}, found {f['size']}")
            elif w["type"] == "file" and w["sha256"] != f["sha256"]:
                problems.append(f"SHA256 {name}: content differs")
            skip = w.get("mtime_ns") is None or (no_mtime_dirs and w["type"] == "dir")
            verdict = None if skip else mtime_verdict(f["mtime_ns"], w["mtime_ns"])
            if verdict:
                level, msg = verdict
                (problems if level == "fail" else warnings).append(f"{name}: {msg}")

    for raw in notes["non_nfc"]:
        warnings.append(f"NFC-WARN on-disk name is not NFC (NFD?): {ascii(raw)}")
    for raw in notes["dupes"]:
        problems.append(f"DUPLICATE name collides after NFC normalization: {ascii(raw)}")

    nf = sum(v["type"] == "file" for v in found.values())
    nd = sum(v["type"] == "dir" for v in found.values())
    for line in problems[:MAX_LINES]:
        print("  " + line)
    if len(problems) > MAX_LINES:
        print(f"  ... {len(problems) - MAX_LINES} more problem(s) not shown")
    for line in warnings[:MAX_LINES]:
        print("  WARN " + line)
    if len(warnings) > MAX_LINES:
        print(f"  ... {len(warnings) - MAX_LINES} more warning(s) not shown")

    if problems:
        summary = f"VERIFY {label}: FAIL {len(problems)} problem(s) on {nf} files, {nd} dirs"
    else:
        summary = f"VERIFY {label}: OK {nf} files, {nd} dirs"
    if warnings:
        summary += f" ({len(warnings)} warning(s))"
    print(summary)
    return not problems


def main(argv):
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    ap = argparse.ArgumentParser(description="Verify a volume against a manifest.")
    ap.add_argument("manifest")
    ap.add_argument("mountpoint")
    ap.add_argument("--label", default="volume")
    ap.add_argument("--no-mtime-dirs", action="store_true", help="skip mtime checks on directories")
    args = ap.parse_args(argv)
    return 0 if verify(load(args.manifest), args.mountpoint, args.label, args.no_mtime_dirs) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
