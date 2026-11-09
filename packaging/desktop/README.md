# flashcp desktop app: packaging

The `flashcp-gui` package is a tray app for the `flashcp` CLI. It notices USB
exFAT drives, offers a one-pass copy, and adds "Fast copy to USB (flashcp)" to
the file manager context menus. It is a Debian package (`.deb`) that depends on
the `flashcp` package.

## Prerequisites

Build machine (amd64; CI uses Ubuntu 24.04):

```sh
sudo apt install build-essential libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libssl-dev libxdo-dev pkg-config
```

Rust (stable) from rustup, and Node.js 22 or newer. The Tauri CLI is run through
`npx`, so it is not installed globally.

## Build

```sh
cd gui/src-tauri
npx --yes @tauri-apps/cli@2.12.4 build --bundles deb
```

The package is written to `gui/target/release/bundle/deb/flashcp-gui_0.1.0_amd64.deb`.
The build also compiles the privileged helper (`flashcp-helper`) through the
`beforeBuildCommand` in `tauri.conf.json`.

## Install

The GUI needs the `flashcp` CLI, which is a separate package. Install both
together so apt can resolve the dependency:

```sh
sudo apt install ./flashcp_*.deb ./flashcp-gui_*.deb
```

Uninstall with `sudo apt remove flashcp-gui`.

## What gets installed where

| Path | Source | Purpose |
| --- | --- | --- |
| `/usr/bin/flashcp-gui` | Tauri build | The GUI and tray app |
| `/usr/libexec/flashcp-helper` | `gui/helper` | Root helper, run through pkexec (mode 0755, root) |
| `/usr/libexec/flashcp-trust` | `gui/helper` | Root helper that records a drive as trusted (mode 0755, root) |
| `/usr/share/polkit-1/actions/io.github.zuhuhix.flashcp.helper.policy` | `packaging/desktop` | polkit action for copy/erase runs (`auth_admin_keep`) |
| `/usr/share/polkit-1/actions/io.github.zuhuhix.flashcp.trust.policy` | `packaging/desktop` | polkit action for trusting a drive (`auth_admin`, never cached) |
| `/etc/flashcp/trusted-drives` | created at runtime | Drives an admin trusted (root-owned, 0644); not shipped in the package |
| `/usr/share/applications/flashcp-gui.desktop` | `flashcp-gui.desktop` (Tauri template) | Application menu entry |
| `/etc/xdg/autostart/flashcp-gui.desktop` | `flashcp-gui-autostart.desktop` | Starts the tray at login (`--tray`) |
| `/usr/share/nemo/actions/flashcp.nemo_action` | `flashcp.nemo_action` | Nemo context menu entry |
| `/usr/share/nautilus-python/extensions/flashcp_nautilus.py` | `flashcp_nautilus.py` | Nautilus context menu entry (needs `python3-nautilus`) |
| `/usr/share/kio/servicemenus/flashcp.desktop` | `flashcp-dolphin.desktop` | Dolphin (KDE) context menu entry |
| `/usr/share/icons/hicolor/scalable/apps/flashcp-gui.svg` | `flashcp-gui.svg` | Scalable icon |
| `/usr/share/icons/hicolor/*/apps/flashcp-gui.png` | `gui/src-tauri/icons` | Raster icons, placed by the Tauri bundler |
| `/usr/share/doc/flashcp-gui/SECURITY-NOTES.md` | `SECURITY-NOTES.md` | What the privileged path can and cannot do |

Check the real file list with `dpkg -c gui/target/release/bundle/deb/flashcp-gui_*.deb`.

## How it works

- At login the tray starts hidden (`flashcp-gui --tray`). It shows a desktop
  notification when a USB exFAT drive is plugged in, if notifications are on.
  Autostart can be turned off in the app's settings. That writes a
  `~/.config/autostart/flashcp-gui.desktop` override with `Hidden=true`.
- The window can also be opened from the tray or the application menu. You can
  drop files or folders on it, choose a drive, and start a copy. The first time
  you use a drive, the app asks you to trust it (admin password). Erasing and
  rebuilding a drive is a separate action under *Advanced*. It requires typing
  the drive's label, or its serial number if it has no exFAT label.
- Context menus: "Fast copy to USB (flashcp)" passes the selected paths to
  `flashcp-gui`, which opens with those files queued.
- Nemo needs a restart to pick up a new action: `nemo -q`.
- Nautilus needs `python3-nautilus` installed, then `nautilus -q`.
- Dolphin picks up the service menu automatically.

## Privileged access

The GUI runs as your user. Writing to a raw block device needs root, so the GUI
starts small helpers through `pkexec`:

- **Trusting a drive** (once per drive): `/usr/libexec/flashcp-trust` records the
  drive's serial number, WWN, model and size in `/etc/flashcp/trusted-drives`.
  polkit asks for an admin password every time (`auth_admin`, no caching). The
  app shows the model, size and serial before you confirm.
- **Copy or erase**: `/usr/libexec/flashcp-helper` opens the device once per job
  with `O_EXCL` and checks that the opened device is a trusted, idle USB drive
  that doesn't carry the system. It then runs `/usr/bin/flashcp` as *your* user
  with only that open device handed over as fd 3. Nothing changes owner or
  permissions. An erase also needs the drive's label (or its serial number if it
  has no exFAT label), and the helper checks that against the disk itself.

See [SECURITY-NOTES.md](SECURITY-NOTES.md) for the threat model and the limits of
this design.

## Development and dry runs

No USB drive is needed to try the GUI. The two switches below swap the drive
list and the CLI:

```sh
cd gui
FLASHCP_GUI_FAKE_DRIVES=1 FLASHCP_GUI_DIRECT=$PWD/tests/fake-flashcp cargo run -p flashcp-gui
```

`FLASHCP_GUI_FAKE_DRIVES=1` shows two fake drives, and `FLASHCP_GUI_DIRECT`
runs the given binary directly with no pkexec or helper. `tests/fake-flashcp`
prints the JSON event stream and writes nothing. Add `--tray` to start hidden.

## Tests

```sh
cd gui && cargo test
cd .. && node --test "gui/ui/tests/*.test.mjs"
```

## Why there is no AppImage

An AppImage cannot install the polkit policy, the root helper, or the file
manager actions into the system locations they need. Without those, copying to
a drive would not work from an AppImage. The `.deb` is the supported installer.
