#!/usr/bin/env python3
"""Report how an OS's exFAT driver handles Unicode normalization (report only, always exits 0).

Usage: nfc_probe.py MOUNTPOINT [LABEL]

Run it after write_test.py: it MUTATES the volume (renames and deletes files in accents/ and
creates and deletes files in nfcprobe/). It prints one "NFCPROBE" line per case:

  existing  names written by the volume's writer (flashcp, or mkfs.exfat + Linux for the control
            volume), stored in NFC: stat / read / rename / unlink, addressed by NFC and by NFD
  new       names this OS creates (given in NFC or NFD): which form readdir returns, then
            stat / unlink addressed by NFC and by NFD

Comparing a flashcp volume with the control volume tells whether a failure is the OS's or flashcp's.
"""
import os
import sys
import unicodedata


def NFC(s):
    return unicodedata.normalize("NFC", s)


def NFD(s):
    return unicodedata.normalize("NFD", s)


FORMS = {"NFC": NFC, "NFD": NFD}


def form(name):
    if NFC(name) == NFD(name):
        return "ascii"
    return "NFC" if name == NFC(name) else "NFD" if name == NFD(name) else "mixed"


def attempt(fn):
    try:
        fn()
        return "ok"
    except OSError as e:
        return f"errno{e.errno}"


def listed(d, name):
    return ",".join(form(n) for n in os.listdir(d) if NFC(n) == NFC(name)) or "absent"


def read(p):
    with open(p, "rb") as f:
        f.read()


def write(p):
    with open(p, "wb") as f:
        f.write(b"x")
        f.flush()
        os.fsync(f.fileno())


def main(argv):
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    mount = argv[0]
    label = argv[1] if len(argv) > 1 else mount

    def out(s):
        print(f"NFCPROBE {label} {s}", flush=True)

    # Existing NFC names written by the volume's writer.
    acc = os.path.join(mount, "accents")
    if os.path.isdir(acc):
        out(f"existing café.txt readdir form: {listed(acc, 'café.txt')}")
        for f, fn in FORMS.items():
            p = os.path.join(acc, fn("café.txt"))
            out(f"existing café.txt by {f}: stat={attempt(lambda: os.stat(p))} read={attempt(lambda: read(p))}")
        for f, fn, name in (("NFC", NFC, "cafè.txt"), ("NFD", NFD, "cafê.txt")):
            src, dst = os.path.join(acc, fn(name)), os.path.join(acc, f"renamed-{f}.txt")
            r = attempt(lambda: os.rename(src, dst))
            back = attempt(lambda: os.rename(dst, os.path.join(acc, NFC(name)))) if r == "ok" else "-"
            out(f"existing {name} rename by {f}: {r} (rename back: {back})")
        for f, fn, name in (("NFC", NFC, "cafè.txt"), ("NFD", NFD, "cafê.txt")):
            u = attempt(lambda: os.remove(os.path.join(acc, fn(name))))
            out(f"existing {name} unlink by {f}: {u} (now listed: {listed(acc, name)})")
    else:
        out("existing: accents/ not found")

    # Names this OS creates itself.
    d = os.path.join(mount, "nfcprobe")
    os.makedirs(d, exist_ok=True)
    a = os.path.join(d, "ascii.txt")
    out(f"new ascii.txt: create={attempt(lambda: write(a))} unlink={attempt(lambda: os.remove(a))}")
    for cf, cfn in FORMS.items():
        for of, ofn in FORMS.items():
            name = f"é-{cf}-{of}.txt"
            c = attempt(lambda: write(os.path.join(d, cfn(name))))
            shown = listed(d, name)
            s = attempt(lambda: os.stat(os.path.join(d, ofn(name))))
            u = attempt(lambda: os.remove(os.path.join(d, ofn(name))))
            out(f"new created {cf}, addressed {of}: create={c} readdir={shown} stat={s} unlink={u} "
                f"after={listed(d, name)}")
    for n in os.listdir(d):  # clean up by the exact listed name
        r = attempt(lambda: os.remove(os.path.join(d, n)))
        if r != "ok":
            out(f"cleanup unlink of listed name {ascii(n)}: {r}")
    rd = attempt(lambda: os.rmdir(d))
    if rd != "ok":
        out(f"cleanup rmdir nfcprobe: {rd} (left: {[ascii(n) for n in os.listdir(d)]})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
