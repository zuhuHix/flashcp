# flashcp-helper security notes

`flashcp-gui` runs as the logged-in user. Writing a block device needs more than that, so the
GUI calls `pkexec /usr/libexec/flashcp-helper ...` (polkit action
`io.github.zuhuhix.flashcp.helper`). The helper is the only code that runs as root. Source:
`gui/helper/`.

## Assets

- Internal disks (NVMe, SATA, eMMC, virtio) and whatever is mounted from them.
- Files only root or other users can read (`/etc/shadow`, other users' homes).
- The integrity of the running system: no root shell, no lasting privilege for the user.

## Attackers considered

- A local unprivileged user or process in the session, including a compromised GUI process. It
  can call pkexec with any argv and environment it likes.
- Hostile file names in the copy source (spaces, newlines, leading `-`).
- A malicious USB device: odd descriptors, a crafted exFAT volume.

Out of scope: someone who already has root, physical attacks beyond plugging in a USB device.

## Trust boundary

pkexec authenticates an administrator, scrubs the environment, sets `PKEXEC_UID`, and runs the
helper as root. Everything the helper receives in argv is untrusted. pkexec only runs this
path under this action, and the helper takes no input from the environment except
`PKEXEC_UID` (or `SUDO_UID`, when started via sudo).

## What the helper does

1. **Fixed argv shapes only**: `add <src> <dev> [--to <dir>] [--skip-existing]`,
   `build <src> <dev> --yes-erase [--label <L>]`, `info <dev>`. Unknown ops, extra or
   duplicate flags, and `build` without `--yes-erase` are refused. The helper builds flashcp's
   argv itself from the parsed values and appends `--json`; nothing is forwarded verbatim.
2. **Value checks**: `<src>` must be absolute (so it can never parse as a flag), at most 4096
   bytes, no NUL. `--to` must start with `/`, have no `..` component and no control characters,
   and be at most 1024 bytes. `--label` must be 1-15 characters with no control characters, no
   `/` and no leading `-`. `<dev>` must match `/dev/sd[a-z]{1,3}[0-9]{0,3}` exactly. There is no
   normalisation, so `/dev/../dev/sda`, `/dev/disk/by-id/...` and trailing spaces all fail.
3. **Device checks** (fail closed, so a missing or unreadable input is a rejection):
   - `lstat` shows a block device, not a symlink. Its `st_rdev` (MAJ:MIN) is the identity used
     from here on.
   - `/sys/dev/block/MAJ:MIN` resolves under `/sys/devices/`, its kernel name equals the
     requested name, its `dev` file matches, partition vs whole disk matches the name, and the
     parent disk name matches (`sdb` for `sdb1`). This catches a name pointing at another
     device.
   - **Transport**: the sysfs path has a `usbN` bus component. The kernel builds that topology
     from the bus the device actually enumerated on, so a device cannot claim it. We do *not*
     rely on the `removable` attribute. It is 0 on most USB SSDs (udisks treats every USB drive
     as removable for that reason) and 1 on some internal card readers and SATA hot-swap bays.
     A non-USB device is refused even if `removable=1`.
   - The udev database must exist for the disk and the target and show USB (`ID_BUS=usb`, or
     `ID_USB_DRIVER`/`ID_PATH ...-usb-...` for UAS drives, where udev reports `ID_BUS=scsi`).
     `UDISKS_SYSTEM=1` or `UDISKS_IGNORE=1` refuses it, so admins can protect disks with a
     udev rule.
   - Not in use. A disk is refused if any of its partitions is mounted at `/`, `/boot`,
     `/boot/efi`, `/usr`, `/var`, `/home` or `/etc`. The target (and, for a whole disk, every
     partition) must not be mounted (matched by MAJ:MIN *and* by source, because btrfs reports
     an anonymous MAJ:MIN), must not be in `/proc/swaps`, and must have an empty `holders/`
     (no LVM, dm-crypt or md on top).
   - Size is not 0 (no empty card-reader slot).
4. **TOCTOU**: the checks run, then the node is opened `O_PATH|O_NOFOLLOW|O_CLOEXEC`, `fstat`
   must show the same block rdev, and the checks run again. Ownership is changed through
   that fd (`fchownat(fd, "", uid, -1, AT_EMPTY_PATH)`), so replacing or renaming the path
   cannot redirect the chown. If the device is unplugged and replugged, devtmpfs creates a fresh
   `root:disk` node and the lent inode simply disappears.
5. **Lend the device, run flashcp as the user**: the node's owner becomes the invoking user
   (group and mode are unchanged, normally `disk` and 0660). Then the helper spawns
   `/usr/bin/flashcp` with a path fixed at compile time (`FLASHCP_HELPER_FLASHCP_PATH` can change
   it only at build time). In the child, before exec: `setgroups` (the user's groups),
   `setresgid`, `setresuid` to the user, a check that `setuid(0)` now fails,
   `PR_SET_NO_NEW_PRIVS`. The environment is replaced by `PATH=/usr/bin:/bin`,
   `LANG=LC_ALL=C.UTF-8` and the user's `HOME`. stdin is `/dev/null` and cwd is `/`. No shell
   is involved anywhere. If any step fails, the spawn is aborted.
6. **Give it back**: SIGINT, SIGTERM, SIGHUP and SIGQUIT are blocked in the helper before the
   grant and forwarded to flashcp. When flashcp exits, a drop guard restores the original
   owner on every return path, including errors and panics. The helper exits with flashcp's code
   (128+N on signal N). Its own failures are one `{"event":"error",...}` line plus exit 3
   (refused) or 4 (internal).

## Why flashcp runs as the user

Root's only job is to hand one validated device node to the user for one run. flashcp reads
the source as the invoking user. So the helper cannot be used to read anything the user could
not read already, for example copying `/etc/shadow` onto a stick. The same goes for symlink
tricks in the source tree and for bugs in flashcp's copy code: they run with the user's
rights, never root's. The scrubbed environment and the fixed binary path mean `LD_PRELOAD`,
locale or `PATH` tricks cannot load attacker code into the process that holds the device.

## Residual risks (accepted)

- **A compromised session can erase a USB drive.** Malicious code running as the user can
  call pkexec itself, and if an administrator authenticates (or a cached `auth_admin_keep`
  authorization is still valid, about 5 minutes on the active session), it can `build` (erase)
  any attached, unmounted USB drive. The GUI requires the user to type the drive's label before
  an erase. The helper cannot verify that this happened, so that confirmation protects against
  mistakes, not against malware. Internal disks stay out of reach.
- During a run the user owns the device node and can open it from any of their processes, for
  example to read the stick's raw contents. That is the access the run needs anyway. Raw SCSI
  passthrough on the node is limited by the kernel's SG_IO command filter. A file descriptor
  opened during that window stays usable after ownership is restored (permissions are checked
  at open time), so the user keeps raw access to that one stick until they close it or unplug
  the stick. They already had admin consent to write it, so this is accepted.
- If the helper is SIGKILLed (only root can do that), the guard does not run and the node
  stays user-owned until the device is replugged or udev re-applies its rules.
- `add` and `info` parse an existing, untrusted exFAT volume. A malicious stick could exploit a
  flashcp parser bug, but only as the unprivileged user, with no new privileges.
- Mount and swap checks are a snapshot. If the stick is mounted between the check and
  flashcp's open, flashcp's `O_EXCL` open of the device fails instead.
- `SUDO_UID` is trusted when present without `PKEXEC_UID`. Only root can start the helper,
  and root could do all of this without it.
