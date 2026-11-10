"""Evaluates 61-flashcp-drive-mode.rules against udev property sets: which devices reach the
flashcp-udev-check (the takeover decision) and which never do. A small evaluator of exactly the
constructs the rule uses (==, !=, |-alternatives, globs, TEST, GOTO/LABEL, IMPORT)."""
import fnmatch
import os
import re
import unittest

RULE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "61-flashcp-drive-mode.rules")
TOKEN = re.compile(r'\s*([A-Z_]+(?:\{[^}]*\})?)\s*(==|!=|\+=|=)\s*"([^"]*)"\s*,?')


def parse():
    rules = []
    for line in open(RULE, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        toks, pos = [], 0
        while pos < len(line):
            m = TOKEN.match(line, pos)
            assert m, f"cannot parse: {line[pos:]!r}"
            toks.append(m.groups())
            pos = m.end()
        rules.append(toks)
    return rules


def matches(value, pattern):
    return any(fnmatch.fnmatchcase(value, alt) for alt in pattern.split("|"))


def reaches_check(props, existing_files=()):
    """True if the rule runs flashcp-udev-check for a device with these properties."""
    skip_to = None
    for toks in parse():
        if skip_to:
            if toks[0][0] == "LABEL" and toks[0][2] == skip_to:
                skip_to = None
            continue
        ok = True
        for key, op, val in toks:
            if op not in ("==", "!="):
                continue
            if key == "TEST":
                have = val in existing_files
            else:
                name = key[4:-1] if key.startswith("ENV{") else key
                have = matches(props.get(name, ""), val)
            ok = ok and (have if op == "==" else not have)
        if not ok:
            continue
        for key, op, val in toks:
            if key == "GOTO":
                skip_to = val
            if key == "IMPORT{program}" and "flashcp-udev-check" in val:
                return True
    return False


BASE = {"ACTION": "add", "SUBSYSTEM": "block", "DEVTYPE": "partition", "ID_FS_TYPE": "exfat",
        "ID_FS_USAGE": "filesystem"}


class DriveModeRule(unittest.TestCase):
    def test_which_devices_reach_the_check(self):
        cases = [
            ("UAS SSD (ID_BUS=scsi)", {"ID_BUS": "scsi", "ID_USB_DRIVER": "uas"}, True),
            ("usb-storage stick", {"ID_BUS": "usb", "ID_USB_DRIVER": "usb-storage"}, True),
            ("exFAT superfloppy", {"ID_USB_DRIVER": "usb-storage", "DEVTYPE": "disk"}, True),
            ("USB disk with a partition table", {"ID_USB_DRIVER": "uas", "DEVTYPE": "disk",
                                                 "ID_PART_TABLE_TYPE": "gpt"}, False),
            ("SATA disk", {"ID_BUS": "ata"}, False),
            ("NVMe", {"ID_BUS": "nvme"}, False),
            ("virtio / iSCSI SCSI disk", {"ID_BUS": "scsi"}, False),
            ("USB but FAT32", {"ID_USB_DRIVER": "usb-storage", "ID_FS_TYPE": "vfat"}, False),
            ("USB, exFAT, not a filesystem", {"ID_USB_DRIVER": "uas", "ID_FS_USAGE": "raid"}, False),
            ("other USB driver (e.g. MTP)", {"ID_USB_DRIVER": "usbfs"}, False),
            ("remove event", {"ID_USB_DRIVER": "uas", "ACTION": "remove"}, False),
            ("not a block device", {"ID_USB_DRIVER": "uas", "SUBSYSTEM": "usb"}, False),
        ]
        for name, extra, want in cases:
            with self.subTest(name):
                self.assertEqual(reaches_check({**BASE, **extra}), want)
        # The system-wide switch turns it off for everything.
        on = {**BASE, "ID_USB_DRIVER": "uas"}
        self.assertFalse(reaches_check(on, existing_files={"/etc/flashcp/drive-mode-disabled"}))


if __name__ == "__main__":
    unittest.main()
