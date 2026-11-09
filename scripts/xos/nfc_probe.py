#!/usr/bin/env python3
"""Report how an OS's exFAT driver treats Unicode normalization (report only, always exits 0).

Usage: nfc_probe.py MOUNTPOINT [LABEL]

For the existing NFC file "café.txt" (written by flashcp or by the control writer) and for a new
file this probe creates with an NFC name, it prints which form readdir returns and whether a
lookup by the NFC and by the NFD spelling finds the file.
"""
import os
import sys
import unicodedata

NFC = lambda s: unicodedata.normalize("NFC", s)  # noqa: E731
NFD = lambda s: unicodedata.normalize("NFD", s)  # noqa: E731


def form(name):
    if name == NFC(name) == NFD(name):
        return "ascii"
    return "NFC" if name == NFC(name) else "NFD" if name == NFD(name) else "mixed"


def probe(mount, name):
    listed = [n for n in os.listdir(mount) if NFC(n) == NFC(name)]
    res = {}
    for f, fn in (("NFC", NFC), ("NFD", NFD)):
        try:
            os.stat(os.path.join(mount, fn(name)))
            res[f] = "found"
        except OSError as e:
            res[f] = "ENOENT" if e.errno == 2 else f"errno {e.errno}"
    shown = ",".join(form(n) for n in listed) or "absent"
    return f"readdir={shown} lookup(NFC)={res['NFC']} lookup(NFD)={res['NFD']}"


def main(argv):
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    mount = argv[0]
    label = argv[1] if len(argv) > 1 else mount
    print(f"NFCPROBE {label} existing café.txt: {probe(mount, NFC('café.txt'))}")
    new = NFC("probe-é.txt")
    try:
        with open(os.path.join(mount, new), "wb") as f:
            f.write(b"x")
        print(f"NFCPROBE {label} new probe-é.txt (created NFC): {probe(mount, new)}")
        for n in os.listdir(mount):
            if NFC(n) == new:
                os.remove(os.path.join(mount, n))
    except OSError as e:
        print(f"NFCPROBE {label} new file: {e}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
