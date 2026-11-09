# Unit tests for the pure helpers in flashcp_nautilus.py. No gi or Nautilus needed.
# Run from the repository root: python3 -m unittest packaging/desktop/test_flashcp_nautilus.py
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import flashcp_nautilus as fn  # noqa: E402

USB_EXFAT = "40 35 8:17 / {mp} rw,nosuid shared:5 - exfat /dev/sdb1 rw,uid=1000"
ROOT_EXT4 = "36 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw"


def mountinfo(*lines):
    return "\n".join(lines) + "\n"


class ParseMountinfoTests(unittest.TestCase):
    def test_fields_and_optional_fields(self):
        text = mountinfo(
            ROOT_EXT4,
            "41 36 8:17 / /media/u/STICK rw - exfat /dev/sdb1 rw,uid=1000",
            "42 36 8:33 / /media/u/OTHER rw shared:9 master:3 - vfat /dev/sdc1 rw",
        )
        self.assertEqual(
            fn.parse_mountinfo(text),
            [
                ("/", "ext4", "/dev/sda2"),
                ("/media/u/STICK", "exfat", "/dev/sdb1"),
                ("/media/u/OTHER", "vfat", "/dev/sdc1"),
            ],
        )

    def test_octal_escapes_are_decoded(self):
        text = mountinfo(
            r"50 36 8:17 / /media/u/My\040Stick rw - exfat /dev/sdb1 rw",
            r"51 36 8:33 / /media/u/tab\011here\134back rw - exfat /dev/sdc1 rw",
        )
        mounts = fn.parse_mountinfo(text)
        self.assertEqual(mounts[0][0], "/media/u/My Stick")
        self.assertEqual(mounts[1][0], "/media/u/tab\there\\back")

    def test_malformed_lines_are_skipped(self):
        text = mountinfo("garbage", "1 2 3 - exfat /dev/sdb1", ROOT_EXT4)
        self.assertEqual(fn.parse_mountinfo(text), [("/", "ext4", "/dev/sda2")])


class AncestorMatchTests(unittest.TestCase):
    def test_sibling_with_common_string_prefix_does_not_match(self):
        self.assertFalse(fn.is_ancestor_or_self("/media/u/STICK", "/media/u/STICK2"))
        self.assertFalse(fn.is_ancestor_or_self("/media/u/STICK", "/media/u/STICK2/photos"))

    def test_exact_and_descendant_match(self):
        self.assertTrue(fn.is_ancestor_or_self("/media/u/STICK", "/media/u/STICK"))
        self.assertTrue(fn.is_ancestor_or_self("/media/u/STICK", "/media/u/STICK/a/b"))

    def test_root_matches_everything(self):
        self.assertTrue(fn.is_ancestor_or_self("/", "/home/u"))

    def test_longest_mount_wins(self):
        mounts = fn.parse_mountinfo(mountinfo(
            ROOT_EXT4,
            USB_EXFAT.format(mp="/media/u/STICK"),
            "60 41 8:49 / /media/u/STICK/inner rw - ext4 /dev/sdd1 rw",
        ))
        self.assertEqual(fn.find_mount("/media/u/STICK/inner/x", mounts)[0], "/media/u/STICK/inner")
        self.assertEqual(fn.find_mount("/media/u/STICK/x", mounts)[0], "/media/u/STICK")

    def test_string_prefix_sibling_picks_its_own_mount(self):
        mounts = fn.parse_mountinfo(mountinfo(
            USB_EXFAT.format(mp="/media/u/STICK"),
            "70 40 8:65 / /media/u/STICK2 rw - exfat /dev/sde1 rw",
        ))
        self.assertEqual(fn.find_mount("/media/u/STICK2/Photos", mounts)[0], "/media/u/STICK2")
        self.assertEqual(fn.find_mount("/media/u/STICK/Photos", mounts)[0], "/media/u/STICK")

    def test_no_mount_when_only_sibling_is_mounted(self):
        mounts = fn.parse_mountinfo(mountinfo(USB_EXFAT.format(mp="/media/u/STICK")))
        self.assertIsNone(fn.find_mount("/media/u/STICK2", mounts))

    def test_same_mount_point_twice_keeps_topmost(self):
        mounts = fn.parse_mountinfo(mountinfo(
            "80 40 8:17 / /media/u/S rw - vfat /dev/sdb1 rw",
            "81 40 8:33 / /media/u/S rw - exfat /dev/sdc1 rw",
        ))
        self.assertEqual(fn.find_mount("/media/u/S", mounts)[1], "exfat")


class SysfsUsbTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        root = self._tmp.name
        self.sys_block = os.path.join(root, "sys_class_block")
        devices = os.path.join(root, "devices")
        os.makedirs(self.sys_block)

        usb_part = os.path.join(devices, "pci0000:00", "0000:00:14.0", "usb2", "2-1",
                                "2-1:1.0", "host6", "target6:0:0", "6:0:0:0", "block", "sdb", "sdb1")
        ata_part = os.path.join(devices, "pci0000:00", "0000:00:17.0", "ata1", "host0",
                                "target0:0:0", "0:0:0:0", "block", "sda", "sda1")
        lookalike = os.path.join(devices, "pci0000:00", "usbx", "1-1", "block", "sdc", "sdc1")
        for path in (usb_part, ata_part, lookalike):
            os.makedirs(path)
        os.symlink(usb_part, os.path.join(self.sys_block, "sdb1"))
        os.symlink(ata_part, os.path.join(self.sys_block, "sda1"))
        os.symlink(lookalike, os.path.join(self.sys_block, "sdc1"))

    def test_usb_partition_detected(self):
        self.assertTrue(fn.is_usb_source("/dev/sdb1", self.sys_block))

    def test_internal_disk_not_usb(self):
        self.assertFalse(fn.is_usb_source("/dev/sda1", self.sys_block))

    def test_usb_lookalike_component_not_usb(self):
        self.assertFalse(fn.is_usb_source("/dev/sdc1", self.sys_block))

    def test_missing_sysfs_entry_not_usb(self):
        self.assertFalse(fn.is_usb_source("/dev/sdz9", self.sys_block))

    def test_source_without_name_not_usb(self):
        self.assertFalse(fn.is_usb_source("/", self.sys_block))
        self.assertFalse(fn.is_usb_source("", self.sys_block))


class UsbExfatMountTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.sys_block = os.path.join(self._tmp.name, "block")
        os.makedirs(self.sys_block)
        usb = os.path.join(self._tmp.name, "usb2", "2-1", "block", "sdb", "sdb1")
        internal = os.path.join(self._tmp.name, "ata1", "block", "sda", "sda1")
        os.makedirs(usb)
        os.makedirs(internal)
        os.symlink(usb, os.path.join(self.sys_block, "sdb1"))
        os.symlink(internal, os.path.join(self.sys_block, "sda1"))

    def test_usb_exfat_found(self):
        text = mountinfo(USB_EXFAT.format(mp="/media/u/STICK"))
        self.assertEqual(
            fn.find_usb_exfat_mount("/media/u/STICK/Photos", text, self.sys_block),
            "/media/u/STICK",
        )

    def test_non_exfat_usb_rejected(self):
        text = mountinfo("40 35 8:17 / /media/u/STICK rw - vfat /dev/sdb1 rw")
        self.assertIsNone(fn.find_usb_exfat_mount("/media/u/STICK", text, self.sys_block))

    def test_exfat_on_internal_disk_rejected(self):
        text = mountinfo("41 35 8:1 / /media/u/data rw - exfat /dev/sda1 rw")
        self.assertIsNone(fn.find_usb_exfat_mount("/media/u/data", text, self.sys_block))

    def test_path_outside_mounts_rejected(self):
        text = mountinfo(USB_EXFAT.format(mp="/media/u/STICK"))
        self.assertIsNone(fn.find_usb_exfat_mount("/home/u/Music", text, self.sys_block))

    def test_sibling_mount_not_offered(self):
        text = mountinfo(USB_EXFAT.format(mp="/media/u/STICK"))
        self.assertIsNone(fn.find_usb_exfat_mount("/media/u/STICK2/x", text, self.sys_block))


class PullCandidateTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.sys_block = os.path.join(self._tmp.name, "block")
        os.makedirs(self.sys_block)
        usb = os.path.join(self._tmp.name, "usb1", "1-1", "block", "sdb", "sdb1")
        os.makedirs(usb)
        os.symlink(usb, os.path.join(self.sys_block, "sdb1"))
        self.folder = os.path.join(self._tmp.name, "dummy-mount", "Photos")
        os.makedirs(self.folder)

    def test_directory_on_usb_exfat_offered(self):
        text = mountinfo(USB_EXFAT.format(mp=os.path.join(self._tmp.name, "dummy-mount")))
        self.assertEqual(
            fn.pull_candidate(self.folder, text, self.sys_block),
            os.path.realpath(self.folder),
        )

    def test_regular_file_not_offered(self):
        path = os.path.join(self.folder, "a.txt")
        open(path, "w").close()
        text = mountinfo(USB_EXFAT.format(mp=os.path.join(self._tmp.name, "dummy-mount")))
        self.assertIsNone(fn.pull_candidate(path, text, self.sys_block))

    def test_relative_or_missing_path_not_offered(self):
        self.assertIsNone(fn.pull_candidate("relative/Photos", "", self.sys_block))
        self.assertIsNone(fn.pull_candidate(os.path.join(self._tmp.name, "gone"), "", self.sys_block))
        self.assertIsNone(fn.pull_candidate("", "", self.sys_block))

    def test_argv_has_no_shell_and_keeps_path(self):
        self.assertEqual(fn.pull_argv("/media/u/My Stick/a;b"),
                         ["flashcp-gui", "--pull", "/media/u/My Stick/a;b"])


if __name__ == "__main__":
    unittest.main()
