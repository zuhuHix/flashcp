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
| `/usr/share/nemo/actions/flashcp.nemo_action` | `flashcp.nemo_action` | Nemo: "Fast copy to USB" on selected files and folders |
| `/usr/share/nemo/actions/flashcp-pull.nemo_action` | `flashcp-pull.nemo_action` | Nemo: "Fast copy from USB" on one selected folder |
| `/usr/share/nautilus-python/extensions/flashcp_nautilus.py` | `flashcp_nautilus.py` | Nautilus: both copy entries (needs `python3-nautilus`) |
| `/usr/share/kio/servicemenus/flashcp.desktop` | `flashcp-dolphin.desktop` | Dolphin (KDE): "Fast copy to USB" |
| `/usr/share/kio/servicemenus/flashcp-pull.desktop` | `flashcp-dolphin-pull.desktop` | Dolphin (KDE): "Fast copy from USB" on one folder |
| `/usr/share/icons/hicolor/scalable/apps/flashcp-gui.svg` | `flashcp-gui.svg` | Scalable icon |
| `/usr/share/icons/hicolor/*/apps/flashcp-gui.png` | `gui/src-tauri/icons` | Raster icons, placed by the Tauri bundler |
| `/usr/share/doc/flashcp-gui/SECURITY-NOTES.md` | `SECURITY-NOTES.md` | What the privileged path can and cannot do |
| `/usr/lib/udev/rules.d/61-flashcp-drive-mode.rules` | `61-flashcp-drive-mode.rules` | Drive mode: decides at plug-in whether flashcp or udisks gets a USB exFAT drive |
| `/usr/libexec/flashcp-udev-check` | `gui/helper` | Run by that rule: a sandboxed `flashcp mount --check` (as nobody, ≤ 1.5 s) |
| `/usr/libexec/flashcp-mountpoint` | `gui/helper` | Root helper (pkexec, no password in an active session): an empty `/media/$USER/LABEL` |
| `/usr/share/polkit-1/actions/io.github.zuhuhix.flashcp.mountpoint.policy` | `packaging/desktop` | polkit action for it (`allow_active=yes`, nothing else) |
| `/usr/lib/systemd/user/flashcp-drived.service` | `flashcp-drived.service` | The per-user drive daemon (`flashcp drived`) |
| `/usr/lib/systemd/user-preset/80-flashcp-drived.preset` | `80-flashcp-drived.preset` | Enables it for every user |

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
- *Optimize this stick…* (on a trusted exFAT drive) rewrites a stick that was
  filled file by file into flashcp's layout without losing data. The app first
  runs a dry run and shows the plan: roughly how long it takes, how much free
  space the temporary copy needs on this computer (it refuses if there isn't
  enough), and that the stick is erased and rebuilt from a verified copy. One
  click confirms; the stick must stay plugged in until it says it's done. The
  copy lives in `~/.cache/flashcp/optimize/` and is deleted at the end. If the
  job is interrupted, the app shows where your files are and a *Resume
  optimize* button; if Windows offers to format the stick meanwhile, say No.
  After a copy of many small files to a stick with clusters above 4 KB, the app
  suggests optimizing it (the tip can be turned off per drive).
- Context menus: "Fast copy to USB (flashcp)" passes the selected paths to
  `flashcp-gui`, which opens with those files queued.
- Context menus: "Fast copy from USB (flashcp)" runs `flashcp-gui --pull <folder>`
  for a folder on a USB exFAT drive. The menu entries are only
  a filter. The app resolves the path itself and reports "That folder is not on
  a USB exFAT drive flashcp can see" if it does not match. Nautilus
  shows it for one selected folder, or on the background of the current folder,
  only when that folder is on an exFAT mount whose partition sits on a USB bus
  (checked through `/proc/self/mountinfo` and `/sys/class/block`). Nemo and
  Dolphin cannot filter by file system, so they offer it for any single selected
  folder. (There is no Nemo background entry for the same reason: it would show
  on every folder.)
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
- **Optimize**: the same helper and checks. It accepts no paths at all: the
  temporary copy always goes to `~/.cache/flashcp/optimize` of the user who
  asked, and `flashcp` (already running as that user) creates it.

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
`FAKE_FLASHCP_MODE=optimize-interrupt` (or `optimize-resume`, `optimize-already`,
`optimize-nospace`) makes the fake optimize fail half-way with a recovery text, resume
an interrupted job, report nothing to do, or refuse for lack of space. In a plain
browser (no Tauri), `ui/index.html?optimize=resume,recovery,nospace,already,tip`
simulates the same cases.

## Tests

```sh
cd gui && cargo test
cd .. && node --test "gui/ui/tests/*.test.mjs"
python3 -m unittest discover -s packaging/desktop -p 'test_*.py'
```

The Nautilus unit tests need no `gi`. Use `discover -s` rather than
`unittest packaging/desktop/test_flashcp_nautilus.py`: the file path form
imports the `packaging` pip library instead of this directory on some systems.

## Why there is no AppImage

An AppImage cannot install the polkit policy, the root helper, or the file
manager actions into the system locations they need. Without those, copying to
a drive would not work from an AppImage. The `.deb` is the supported installer.

## Drive mode ("Speed up USB drives", on by default)

Every USB exFAT drive is served by flashcp's FUSE driver (`flashcp mount`) instead of the
kernel exfat driver, so copies from any program go through flashcp. A drive flashcp can't
serve (not exFAT, not clean, damaged, unsupported, in use) is mounted normally by udisks.

**How a drive is taken over.**

1. The kernel announces the partition; udev runs its rules. `61-flashcp-drive-mode.rules`
   matches USB + exFAT + filesystem partitions (or an exFAT disk without a partition table)
   and runs `flashcp-udev-check`, which answers yes only if the active seat user's
   `flashcp-drived` runs with drive mode on (`/run/user/<uid>/flashcp/drive-mode-on`) and a
   sandboxed `flashcp mount --check` accepts the volume within 1.5 s. It tags the device with
   that user's uid (`FLASHCP_DRIVE_UID`): other users' daemons ignore it.
2. Yes: the rule sets `UDISKS_IGNORE=1` (udisks neither mounts nor shows the partition) and
   tags it `uaccess` (the seat user gets rw on the node). No, or no answer: nothing is set, and
   udisks and the desktop automount it as they always did.
3. udev broadcasts the finished event. `flashcp-drived` sees `FLASHCP_DRIVE_MODE=1`, runs
   `--check` again (for exit codes like busy), creates `/media/$USER/LABEL` through
   `flashcp-mountpoint`, and runs `flashcp mount` there. File managers list it under that
   label like any drive.

**Why both drivers never touch the device.** The decision is part of udev's rule processing,
which finishes before udisks, the desktop's automounter or the daemon receive the event, so
nothing can mount the partition before it is decided. After the decision, udisks has been told
to ignore it, and `flashcp mount` holds the device exclusively (`O_EXCL` + `flock`) while it
serves it, so even a manual kernel mount fails with "busy" instead of mounting it twice. In the
other direction, a partition that something already mounted makes `--check` answer busy, and
the daemon never mounts over it (it retries after 1, 3 and 10 s, then only notifies). When
the daemon hands a drive back to the kernel driver (the driver failed twice, recovery refused,
drive mode turned off), it waits for the driver to exit first, then flags the drive in its
runtime dir, has udisks re-run the rules (`Block.Rescan`), and only then mounts it through
udisks (read-only if recovery was refused).

**Eject.** The tray, `flashcp drive-mode eject /dev/sdX1`, or the file manager's
eject/unmount button. The first two ask the driver to `prepare-eject` (commit everything, refuse
new writes), then unmount; the file manager's button unmounts first and the driver commits
afterwards. Either way, a notification shows "Saving N MB to LABEL…" while data is pending, then
"LABEL can be removed", and the drive is powered off like udisks does. Pulling it out while
data is pending shows what may be missing. On the next plug-in the driver's journal recovery
completes or rolls back the interrupted commit, and a notification says so.

**One rule for every handover** (`handover()` in `src/daemon.rs`, the only code that powers a
drive off, mounts it with the kernel driver or gives it back to udisks): read-write, power-off
and "can be removed" need a *clean* end, which means the driver exited 0 after an `unmounted`
event with nothing pending. Anything else gets no power-off, no "safe" message and no Rescan,
only a read-only kernel mount at most, plus a persistent "flashcp couldn't finish saving to LABEL.
Plug it in again to recover." The interrupted commit's journal is on the volume, and the udev
check sees it (`pending_journal`), so the next plug-in goes to flashcp's recovery first, even
with drive mode off (the daemon mounts it once, then hands it to the kernel driver). If no
daemon runs, such a volume stays hidden rather than being mounted unrecovered.

**Robustness.** A driver that dies is restarted once (its recovery runs); a second failure
falls back to the kernel driver with a notification. systemd restarts the daemon itself
(`Restart=on-failure`); if it dies, its drivers are killed with it (systemd), and the restarted
daemon serves the drives again after the driver's journal recovery (data that was still pending
is lost, as with a crash of the kernel's page cache), and `ExecStopPost` makes sure new drives
go to udisks while it is down. While data is pending the daemon holds a logind *delay*
inhibitor for sleep and shutdown. On `PrepareForSleep` it flushes every drive and releases the
lock once nothing is pending. logind waits at most `InhibitDelayMaxSec` (5 s by default); the
driver keeps at most about 2 s of the drive's write speed pending, so that is normally enough,
but a stick that slows down while saving can be suspended with data pending. Logout or `systemctl --user stop` sends SIGTERM: every drive is flushed and
unmounted before the daemon exits.

**Switches.**

| Command | Effect |
| --- | --- |
| the app's "Speed up USB drives" / `flashcp drive-mode on\|off` | Per user, persisted in `~/.config/flashcp/drive-mode.conf`. Off: current flashcp drives are flushed and remounted by the kernel driver (read-only, with a notification, if flashcp could not finish saving: turn it on and replug to recover); new drives go to udisks. |
| `flashcp drive-mode status [--json]` | Mode, state and pending bytes per drive |
| `sudo flashcp drive-mode off --system` | For every user (`/etc/flashcp/drive-mode-disabled`); re-runs the rules so plugged drives go back to udisks. `on --system` undoes it. |
| `systemctl --user disable --now flashcp-drived` | Stop the daemon for one user (its drives are flushed and unmounted; new drives get udisks) |
| `sudo apt remove flashcp-gui` | Stops every user's daemon, removes the rule, re-runs the rules: normal automount |

Manual rollback without the package tools: `sudo rm /usr/lib/udev/rules.d/61-flashcp-drive-mode.rules
&& sudo udevadm control --reload && sudo udevadm trigger -s block -c change`.
