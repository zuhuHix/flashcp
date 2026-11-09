# flashcp: "Fast copy to USB (flashcp)" and "Fast copy from USB (flashcp)" entries
# for the Nautilus (GNOME Files) context menu.
# Installed at /usr/share/nautilus-python/extensions/ by the flashcp-gui package.
# Needs the python3-nautilus package. Supports the Nautilus 4 (GTK 4) signatures
# get_file_items(files) / get_background_items(current_folder) and the older
# get_file_items(window, files) / get_background_items(window, current_folder).
#
# The pure helpers at the top do not need gi, so the test suite can import this
# module on a machine without Nautilus (see test_flashcp_nautilus.py).
import os
import re
import subprocess
import sys

try:
    import gi

    for _version in ("4.0", "3.0"):
        try:
            gi.require_version("Nautilus", _version)
            break
        except ValueError:
            continue

    from gi.repository import GObject, Nautilus
except (ImportError, ValueError):
    GObject = None
    Nautilus = None

LABEL = "Fast copy to USB (flashcp)"
PULL_LABEL = "Fast copy from USB (flashcp)"
PULL_TIP = "Copy the selected folder from a USB exFAT drive with flashcp"
PULL_BACKGROUND_TIP = "Copy this folder's USB exFAT drive contents with flashcp"
MOUNTINFO = "/proc/self/mountinfo"
SYS_BLOCK = "/sys/class/block"

_OCTAL_ESCAPE = re.compile(r"\\([0-7]{3})")


def _unescape(field):
    """Undo the kernel's octal escapes in mountinfo fields (\\040 is a space)."""
    return _OCTAL_ESCAPE.sub(lambda m: chr(int(m.group(1), 8)), field)


def parse_mountinfo(text):
    """Return a list of (mount_point, fstype, source) tuples from mountinfo text.

    Format: "id parent maj:min root mount_point opts [optional...] - fstype source superopts".
    Malformed lines are skipped.
    """
    mounts = []
    for line in text.splitlines():
        head, sep, tail = line.partition(" - ")
        if not sep:
            continue
        pre = head.split()
        post = tail.split()
        if len(pre) < 5 or len(post) < 2:
            continue
        mounts.append((_unescape(pre[4]), _unescape(post[0]), _unescape(post[1])))
    return mounts


def read_mountinfo(path=MOUNTINFO):
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return ""


def _normalise(path):
    return os.path.normpath(path)


def is_ancestor_or_self(mount_point, path):
    """True when mount_point is path or a component-wise ancestor of it."""
    mp = _normalise(mount_point)
    p = _normalise(path)
    if mp == "/":
        return p.startswith("/")
    return p == mp or p.startswith(mp + "/")


def find_mount(real_path, mounts):
    """Longest mount point that is a component-wise ancestor of real_path, or None."""
    best = None
    for mount in mounts:
        if not is_ancestor_or_self(mount[0], real_path):
            continue
        # Equal-length matches (stacked mounts) keep the later, topmost entry.
        if best is None or len(_normalise(mount[0])) >= len(_normalise(best[0])):
            best = mount
    return best


def is_usb_source(source, sys_block_root=SYS_BLOCK):
    """True when the block device behind source (/dev/sdXN) sits on a USB bus."""
    name = os.path.basename(source.rstrip("/"))
    if not name or name in (".", ".."):
        return False
    sys_path = os.path.join(sys_block_root, name)
    if not os.path.exists(sys_path):
        return False
    return re.search(r"/usb\d+/", os.path.realpath(sys_path) + "/") is not None


def find_usb_exfat_mount(real_path, mountinfo_text, sys_block_root=SYS_BLOCK):
    """Mount point of the USB exFAT volume holding real_path, or None."""
    mount = find_mount(real_path, parse_mountinfo(mountinfo_text))
    if mount is None:
        return None
    mount_point, fstype, source = mount
    if fstype != "exfat":
        return None
    if not is_usb_source(source, sys_block_root):
        return None
    return mount_point


def pull_argv(path):
    # Argument list, no shell: the path is passed through untouched.
    return ["flashcp-gui", "--pull", path]


def pull_candidate(path, mountinfo_text, sys_block_root=SYS_BLOCK):
    """Real path to offer "Copy from USB" for, or None.

    path must be a local directory. Its realpath must sit under a USB exFAT mount.
    """
    if not path or not os.path.isabs(path) or not os.path.isdir(path):
        return None
    real = os.path.realpath(path)
    if find_usb_exfat_mount(real, mountinfo_text, sys_block_root) is None:
        return None
    return real


if Nautilus is not None:

    class FlashcpMenuProvider(GObject.GObject, Nautilus.MenuProvider):
        def get_file_items(self, *args):
            files = args[-1]
            if not files:
                return []

            paths = []
            for info in files:
                location = info.get_location()
                path = location.get_path() if location is not None else None
                if path is None:
                    # Remote or virtual locations (smb://, mtp://, ...) are not offered.
                    return []
                paths.append(path)

            items = [self._copy_to_item(paths)]
            if len(paths) == 1:
                pull_path = pull_candidate(paths[0], read_mountinfo())
                if pull_path is not None:
                    items.append(self._pull_item("FlashcpMenuProvider::fast_pull", PULL_LABEL, PULL_TIP, pull_path))
            return items

        def get_background_items(self, *args):
            folder = args[-1]
            if folder is None:
                return []
            location = folder.get_location()
            path = location.get_path() if location is not None else None
            if path is None:
                return []
            pull_path = pull_candidate(path, read_mountinfo())
            if pull_path is None:
                return []
            return [self._pull_item("FlashcpMenuProvider::fast_pull_here", PULL_LABEL, PULL_BACKGROUND_TIP, pull_path)]

        def _copy_to_item(self, paths):
            item = Nautilus.MenuItem(
                name="FlashcpMenuProvider::fast_copy",
                label=LABEL,
                tip="Copy the selection to a USB exFAT drive with flashcp",
                icon="flashcp-gui",
            )
            item.connect("activate", self._launch, paths)
            return item

        def _pull_item(self, name, label, tip, path):
            item = Nautilus.MenuItem(name=name, label=label, tip=tip, icon="flashcp-gui")
            item.connect("activate", self._launch_pull, path)
            return item

        def _launch(self, _menu, paths):
            try:
                # Argument list, no shell: file names are passed through untouched.
                subprocess.Popen(["flashcp-gui", *paths], start_new_session=True)
            except OSError as exc:
                print(f"flashcp: cannot start flashcp-gui: {exc}", file=sys.stderr)

        def _launch_pull(self, _menu, path):
            try:
                subprocess.Popen(pull_argv(path), start_new_session=True)
            except OSError as exc:
                print(f"flashcp: cannot start flashcp-gui: {exc}", file=sys.stderr)
