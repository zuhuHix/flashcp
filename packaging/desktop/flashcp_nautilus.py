# flashcp: "Fast copy to USB (flashcp)" entry for the Nautilus (GNOME Files) context menu.
# Installed at /usr/share/nautilus-python/extensions/ by the flashcp-gui package.
# Needs the python3-nautilus package. Supports the Nautilus 4 (GTK 4) signature
# get_file_items(files) and the older get_file_items(window, files).
import subprocess
import sys

import gi

for _version in ("4.0", "3.0"):
    try:
        gi.require_version("Nautilus", _version)
        break
    except ValueError:
        continue

from gi.repository import GObject, Nautilus  # noqa: E402

LABEL = "Fast copy to USB (flashcp)"


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

        item = Nautilus.MenuItem(
            name="FlashcpMenuProvider::fast_copy",
            label=LABEL,
            tip="Copy the selection to a USB exFAT drive with flashcp",
            icon="flashcp-gui",
        )
        item.connect("activate", self._launch, paths)
        return [item]

    def _launch(self, _menu, paths):
        try:
            # Argument list, no shell: file names are passed through untouched.
            subprocess.Popen(["flashcp-gui", *paths], start_new_session=True)
        except OSError as exc:
            print(f"flashcp: cannot start flashcp-gui: {exc}", file=sys.stderr)
