# flashcp desktop: security notes

`flashcp-gui` runs as the logged-in user. Writing a block device needs more than that, so two
small programs run as root via pkexec. Source for both: `gui/helper/`.

| Program | polkit action | Defaults (any / inactive / active) |
| --- | --- | --- |
| `/usr/libexec/flashcp-helper` | `io.github.zuhuhix.flashcp.helper` | `auth_admin` / `auth_admin` / `auth_admin_keep` |
| `/usr/libexec/flashcp-trust` | `io.github.zuhuhix.flashcp.trust` | `auth_admin` / `auth_admin` / `auth_admin` |

## The guarantee

flashcp only writes to drives that an administrator explicitly trusted (by stable identity),
that are currently attached over USB, that are idle, and that do not carry the running system.

What this does not claim: that internal disks are unreachable. "Attached over USB" is a
property of the kernel's device topology, not proof that a drive is external. An internal
disk behind a USB bridge (some laptops and NAS boards wire card readers or even SSDs that way)
is a USB device. So is a malicious USB device that copies the serial, model and size of a
drive that was trusted. Such a disk is blocked by the trust step and the defence-in-depth
checks below, not by the USB check. See "Residual risks".

## Assets

- Disks the administrator has not trusted, and whatever is stored on them.
- The disk carrying the running system (`/`, `/boot`, `/usr`, ...), even if it was trusted.
- Files only root or other users can read (`/etc/shadow`, other users' homes).
- The integrity of the running system: no root shell, no lasting privilege for the user.

## Attackers considered

- A local unprivileged user or process in the session, including a compromised GUI process. It
  can call pkexec with any argv and environment it likes, and can signal (including SIGKILL)
  the helper, because pkexec keeps the caller's real uid.
- Hostile file names in the copy source (spaces, newlines, leading `-`).
- A malicious USB device: odd descriptors, a crafted exFAT volume, a forged serial number.

Out of scope: someone who already has root, physical attacks beyond plugging in a USB device.

## Trust boundary

pkexec authenticates an administrator, scrubs the environment, sets `PKEXEC_UID`, and runs the
program as root. Everything either program receives in argv is untrusted. Neither takes input
from the environment except `PKEXEC_UID` (or `SUDO_UID`, when started via sudo; only the helper
reads it).

## Drive identity

The identity of a drive is that of its whole disk (a partition uses its disk's identity). Both
programs read it themselves from the disk's udev database entry (`/run/udev/data/bMAJ:MIN`,
MAJ:MIN taken from `fstat` of the opened device) and sysfs:

- serial: the first non-empty of `ID_SERIAL_SHORT`, `ID_SCSI_SERIAL`
- wwn: the first non-empty of `ID_WWN_WITH_EXTENSION`, `ID_WWN`, else `-`
- model: `ID_MODEL`, else `-`
- size: sysfs `<disk>/size` times 512

The key is the serial, or the wwn if there is no serial. A drive with neither cannot be
trusted. This is the same priority udisks uses for `Drive.Serial`, which the GUI shows and
passes as `--expect <key>`. Values containing a TAB, newline or any other control character, a
key over 128 characters and a size of 0 are refused.

## The trust list

`/etc/flashcp/trusted-drives` (directory 0755, file 0644, both root-owned), one drive per line:

```
v1<TAB><key><TAB><wwn or -><TAB><model or -><TAB><size-bytes>
```

`#` lines and blank lines are ignored. Malformed lines are skipped and reported on stderr. A
device matches an entry only if key, wwn, model and size are all equal. Before reading it the
helper checks that the directory and the file belong to root and are not group- or
world-writable, and opens the file with `O_NOFOLLOW`. Anything else is refused.

Only `flashcp-trust` writes it. Under an `flock` on `/etc/flashcp/.lock` it writes the whole new
content to a temporary file in `/etc/flashcp` (`O_CREAT|O_EXCL`, mode 0644), fsyncs it, renames
it over the list and fsyncs the directory. Existing lines are kept verbatim. Trusting a drive
that is already listed is a no-op success.

## flashcp-trust

`flashcp-trust <dev> --expect <key>`, nothing else.

1. `<dev>` must match `/dev/sd[a-z]{1,3}[0-9]{0,3}` exactly. `--expect` is 1-128 characters,
   with no control characters.
2. Opens `<dev>` with `O_RDONLY|O_NOFOLLOW|O_CLOEXEC` and runs the device checks below on the
   fd, except the idle checks. A mounted stick may be trusted. A disk carrying a system mount is
   still refused.
3. Reads the identity and requires key == `--expect`. That ties what the administrator
   approved (the GUI shows model, size, serial and device before asking) to the drive actually
   found at `<dev>`.
4. Appends the entry and prints `{"event":"trusted","key":..,"model":..,"size":N}`.

The polkit action is `auth_admin` with no `keep`. Every trust needs a fresh administrator
password, so a cached authorization from a recent copy can never be used to add drives.

## flashcp-helper

```
flashcp-helper add   <dev> --expect <key> [--skip-existing] --item <src-dir> <to-dir> ...   (1-256 items)
flashcp-helper build <dev> --expect <key> --confirm <text> [--label <L>] <src-dir>
flashcp-helper info  <dev> --expect <key>
```

1. **Fixed argv shapes only.** Unknown ops and flags, duplicate flags, `--flag=value`, missing
   `--expect`, `build` without `--confirm`, zero or more than 256 items, and non-UTF-8 argv
   are refused. The helper builds every flashcp argv itself from the parsed values. Nothing is
   forwarded verbatim.
2. **Value checks.** Sources must be absolute (so they never parse as a flag), at most 4096
   bytes, no NUL. Destinations start with `/`, have no `..` component and no control
   characters, and are at most 1024 bytes. `--label` is 1-15 characters with no control
   characters, no `/` and no leading `-`. `--expect` and `--confirm` are 1-128 characters with
   no control characters. They are only compared, never forwarded. `<dev>` is matched exactly,
   with no normalisation, so `/dev/../dev/sda`, `/dev/disk/by-id/...` and trailing spaces fail.
3. **Open once.** `<dev>` is opened once per job: `O_RDWR|O_EXCL|O_NOFOLLOW|O_CLOEXEC` for
   `add`/`build`, `O_RDONLY|O_NOFOLLOW|O_CLOEXEC` for `info`. With `O_EXCL` the kernel refuses
   a device that is mounted or claimed exclusively (by LVM, md, dm), and keeps others from
   claiming it while the helper holds it. Every check below runs on this fd, and flashcp gets
   this fd. Unplugging and replugging after the open cannot redirect the job to another device:
   the fd stays bound to the device that was opened and validated, and if that device goes
   away, writes fail with EIO. This closes the old "validate a path, then reopen it" window.
4. **Device checks on the opened fd** (fail closed: a missing or unreadable input is a
   rejection):
   - `fstat` shows a block device. Its `st_rdev` (MAJ:MIN) is the identity used from here on.
   - `/sys/dev/block/MAJ:MIN` resolves under `/sys/devices/`, its kernel name equals the
     requested name, its `dev` file matches, partition vs whole disk matches the name, the
     parent disk name matches (`sdb` for `sdb1`), and it sits at `.../block/<disk>`.
   - **Transport:** the sysfs path has a `usbN` bus component. The kernel builds that topology
     from the bus the device enumerated on. `removable` is not used: it is 0 on most USB SSDs
     and 1 on some internal card readers and SATA hot-swap bays.
   - The udev database must exist for the disk and the target and show USB (`ID_BUS=usb`, or
     `ID_USB_DRIVER`/`ID_PATH ...-usb-...` for UAS drives). `UDISKS_SYSTEM=1` or
     `UDISKS_IGNORE=1` refuses the disk, so admins can protect disks with a udev rule.
   - **Not the system disk:** refused if the disk or any of its partitions is mounted at `/`,
     `/boot`, `/boot/efi`, `/usr`, `/var`, `/home` or `/etc`.
   - **Idle:** the target (and, for a whole disk, every partition) is not mounted (matched by
     MAJ:MIN and by source), not in `/proc/swaps`, and has an empty `holders/`.
   - Size is not 0.
5. **Identity and trust:** the disk's identity must equal `--expect` and match a trusted entry
   on all four fields. Otherwise the error starts with `flashcp-helper: drive not trusted`.
6. **Build confirmation, checked by the helper.** Before an erase, the helper runs
   `flashcp info --target-fd 3 --json` as the user (same fd, stdout captured, not forwarded).
   The expected text is the volume label if `info` exited 0 with a non-empty `"label"`, else the
   drive's key (serial). `--confirm` must equal it, otherwise nothing is written. The GUI
   applies the same rule, so the user types what they can see on the drive.
7. **Run flashcp as the user, holding only fd 3.** For each item (or the single build/info
   run): `lseek(fd, 0)`, then spawn `/usr/bin/flashcp <op> ... --target-fd 3 --json` (path
   fixed at compile time). In the child, before exec: `dup2(fd, 3)` (without close-on-exec),
   every fd >= 4 marked close-on-exec (`close_range(4, ~0, CLOSE_RANGE_CLOEXEC)`, or a
   `/proc/self/fd` walk on older kernels), so the exec closes them. Then `setgroups` (the
   user's groups), `setresgid`, `setresuid`, a check that `setuid(0)` now fails, and
   `PR_SET_NO_NEW_PRIVS`. The environment is only `PATH=/usr/bin:/bin`, `LANG=LC_ALL=C.UTF-8`
   and the user's `HOME`. stdin is `/dev/null`, cwd is `/`, no shell is involved. If any step
   fails, the spawn is aborted. The helper stops at the first non-zero exit and returns it
   (128+N for signal N). SIGINT, SIGTERM, SIGHUP and SIGQUIT are forwarded to the running
   flashcp, and no further run starts after one. Its own failures are one
   `{"event":"error","message":"flashcp-helper: ..."}` line plus exit 3 (refused) or 4
   (internal).

### No ownership changes

The helper never chowns, chmods or sets ACLs on anything. flashcp gets access to the drive
only through the inherited fd 3. So nothing is left to restore if the helper dies. The user
can SIGKILL the helper (pkexec keeps their real uid), and this is now harmless: the worst
outcome is that the current flashcp run continues or stops, holding an fd to that one trusted
drive until it exits.

## Why flashcp runs as the user

Root's only jobs are to open one validated, trusted device and to vouch for its identity.
flashcp reads the sources as the invoking user, so the helper cannot be used to read anything
the user could not read already (no copying `/etc/shadow` onto a stick). Symlink tricks in the
source tree and bugs in flashcp's copy code run with the user's rights, never root's. The
scrubbed environment and fixed binary path keep `LD_PRELOAD`, locale or `PATH` tricks out of
the process that holds the device.

## Residual risks (accepted)

- **Identity spoofing.** A malicious USB device can report the serial, WWN, model and size of
  a drive that was trusted. The helper cannot tell it apart, and will write to it. This only
  matters if the attacker can plug a device into the machine and wants it written. The checks
  that remain against a spoofed identity on an internal disk behind a USB bridge are the
  system-disk refusal, the idle checks and `O_EXCL`, plus `UDISKS_SYSTEM`/`UDISKS_IGNORE` udev
  hints the administrator sets.
- **A trusted internal disk is writable.** If an administrator trusts a disk that is internal
  but attached over USB, flashcp can write to it whenever it is idle and not carrying a system
  mount. The GUI warns: "Only trust drives you own and plug in yourself."
- **A compromised session can erase a trusted drive.** Malicious code running as the user can
  call pkexec itself. If an administrator authenticates (or a cached `auth_admin_keep`
  authorization is still valid, about 5 minutes on the active session), it can `build` (erase)
  an attached, idle, trusted drive. The confirmation is checked by the helper against the
  on-disk label (or serial), but malware can read those as well, so it protects against
  mistakes, not malware. It cannot add drives to the trust list without a fresh admin password.
- **The user holds a write fd during a run.** While flashcp runs, the user's own process holds
  a read-write fd to the trusted drive. Another process of the same user could take it over
  (for example with `ptrace`, if the kernel's Yama setting allows it). The scope is that one
  trusted drive, which the user had admin consent to write anyway. Raw SCSI passthrough via
  the fd is limited by the kernel's SG_IO command filter for non-root callers.
- `add`, `info` and the confirmation run parse an existing, untrusted exFAT volume. A malicious
  stick could exploit a flashcp parser bug, but only as the unprivileged user, with no new
  privileges.
- Opening a device read-write (even one that is then refused) makes udev re-probe it when the
  fd is closed. No data is written.
- The mount, swap and holder checks are a snapshot. `O_EXCL` keeps new mounts and exclusive
  claims off the device while the helper holds it.
- `SUDO_UID` is trusted when present without `PKEXEC_UID`. Only root can start the helper,
  and root could do all of this without it.
