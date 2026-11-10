# flashcp desktop: security notes

`flashcp-gui` runs as the logged-in user. Writing a block device needs more than that, so a few
small programs run as root via pkexec, and drive mode adds one that udev runs. Source for all:
`gui/helper/`.

| Program | polkit action | Defaults (any / inactive / active) |
| --- | --- | --- |
| `/usr/libexec/flashcp-helper` | `io.github.zuhuhix.flashcp.helper` | `auth_admin` / `auth_admin` / `auth_admin_keep` |
| `/usr/libexec/flashcp-trust` | `io.github.zuhuhix.flashcp.trust` | `auth_admin` / `auth_admin` / `auth_admin` |
| `/usr/libexec/flashcp-mountpoint` | `io.github.zuhuhix.flashcp.mountpoint` | `no` / `no` / `yes` |
| `/usr/libexec/flashcp-udev-check` | none (run by udev, `61-flashcp-drive-mode.rules`) | — |

## The guarantee

flashcp only writes to drives that an administrator explicitly trusted (by stable identity),
that are currently attached over USB, that are idle, and that do not carry the running system.
It only reads (`pull`, `info`) drives that pass the same checks. Files copied off a drive land
only where the invoking user could write anyway (for `optimize`, in the user's own
`~/.cache/flashcp/optimize`).

What this does not claim: that internal disks are unreachable. "Attached over USB" is a
property of the kernel's device topology, not proof that a drive is external. An internal
disk behind a USB bridge (some laptops and NAS boards wire card readers or even SSDs that way)
is a USB device. So is a malicious USB device that copies the serial, model and size of a
drive that was trusted. Such a disk is blocked by the trust step and the defence-in-depth
checks below, not by the USB check. See "Residual risks".

**Drive mode changes this for USB exFAT drives.** While the logged-in seat user's
`flashcp-drived` runs with "Speed up USB drives" on, every USB exFAT partition that flashcp can
serve is handed to that user read-write (udev's `uaccess` ACL on the device node), without the
trust step: the FUSE driver runs as the user. This is the same reach the user already has
through udisks for such a drive (mount it read-write, or reformat it, with no password in an
active session), but it is raw block access. Internal disks, non-USB disks, non-exFAT
partitions, and every drive while drive mode is off (per user, or `flashcp drive-mode off
--system`) keep the guarantee above. See "Drive mode: flashcp-udev-check".

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
- A user-chosen `pull` destination that points somewhere the user cannot write (`/etc`,
  another user's home), directly or through symlinks swapped in during the copy.
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
- model: `ID_MODEL_ENC` with its `\xHH` escapes decoded and trimmed (exactly what udisks shows
  as `Drive.Model`), else `ID_MODEL` with `_` turned into spaces, else `-`
- size: sysfs `<disk>/size` times 512

### Identity policy

The key is the serial, unless the serial is a **placeholder**. Then it is the WWN, unless that
is a placeholder too. Then the drive has no usable identity and **cannot be trusted
persistently**. Many cheap bridges and sticks report the same serial on every unit, and
trusting one of those would trust every drive of that make.

A serial counts as a placeholder if, after trimming, it:

- is empty or shorter than 4 characters;
- is one character repeated (`000000000000`, `FFFFFFFF`, ignoring separators); or
- is on a short known-junk list (`0123456789ABCDEF`, `1234567890`, `12345678`, `DEADBEEF`,
  `To Be Filled By O.E.M.`, `None`, …).

A WWN is a placeholder if it is missing or its hex digits are one repeated digit.

The rules live in one dependency-free file, `gui/helper/src/weak.rs`, which the GUI compiles
too (`#[path]`). So both sides always agree on the key and on which drives can't be trusted. For
those drives the app explains why and points to the command line (`sudo flashcp add <folder>
/dev/sdXN`). There is deliberately no "use once" path in the app.

The GUI shows udisks' `Drive.Serial`/`WWN`/`Model`/`Size`, applies the same key rule and passes
the whole tuple to `flashcp-trust`. Values containing a TAB, newline or any other control
character, a key over 128 characters and a size of 0 are refused.

**Limits.** Trust is per *identity*, not per physical unit.

- Two genuinely identical drives with the same serial, WWN, model and size can't be told apart.
  Trusting one trusts both.
- A malicious device can clone a trusted identity (see Residual risks).
- A drive whose bridge only reports a vendor-generated serial is identified by that value. If a
  firmware update changes it, the drive has to be trusted again.

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

`flashcp-trust <dev> --expect <key> --wwn <wwn|-> --model <model|-> --size <bytes>`, nothing
else, in that order.

1. `<dev>` must match `/dev/sd[a-z]{1,3}[0-9]{0,3}` exactly. `--expect` is 1-128 characters,
   with no control characters.
2. Opens `<dev>` with `O_RDONLY|O_NOFOLLOW|O_CLOEXEC` and runs the device checks below on the
   fd, except the idle checks. A mounted stick may be trusted. A disk carrying a system mount is
   still refused.
3. Reads the identity from the opened fd's rdev and requires **every field** of the tuple the
   user was shown (key, WWN, model, size) to match exactly. Otherwise it refuses with "the
   drive changed, please re-select it" and writes nothing. If the device was swapped or
   re-enumerated between the dialog and the open, a different drive that merely shares a
   serial can't be enrolled. A placeholder identity is refused here as well.
4. Appends the entry and prints `{"event":"trusted","key":..,"model":..,"size":N}`.

The polkit action is `auth_admin` with no `keep`. Every trust needs a fresh administrator
password, so a cached authorization from a recent copy can never be used to add drives.

## flashcp-helper

```
flashcp-helper add   <dev> --expect <key> [--skip-existing] --item <src-dir> <to-dir> ...   (1-256 items)
flashcp-helper build <dev> --expect <key> --confirm <text> [--label <L>] <src-dir>
flashcp-helper info  <dev> --expect <key>
flashcp-helper pull  <dev> --expect <key> [--from <rel>] [--skip-existing | --overwrite] <dest>
flashcp-helper optimize <dev> --expect <key> [--dry-run]
```

1. **Fixed argv shapes only.** Unknown ops and flags, duplicate flags, `--flag=value`, missing
   `--expect`, `build` without `--confirm`, zero or more than 256 items, and non-UTF-8 argv
   are refused, and so are `pull` with both `--skip-existing` and `--overwrite`, or without a
   destination. The helper builds every flashcp argv itself from the parsed values. Nothing is
   forwarded verbatim. `pull` always becomes `flashcp pull --target-fd 3 <dest> [--from <rel>]
   [--skip-existing|--overwrite] --json`. `optimize` takes no path and no other flag (no
   `--temp`, `--temp-volatile-i-accept-data-loss`, `--force`); it always becomes `flashcp
   optimize --target-fd 3 --temp <home>/.cache/flashcp/optimize --yes [--dry-run] --json`, see
   "optimize: the temporary copy" below.
2. **Value checks.** Sources must be absolute (so they never parse as a flag), at most 4096
   bytes, no NUL. Destinations start with `/`, have no `..` component and no control
   characters, and are at most 1024 bytes. `--label` is 1-15 characters with no control
   characters, no `/` and no leading `-`. `--expect` and `--confirm` are 1-128 characters with
   no control characters. They are only compared, never forwarded. `pull --from` is a folder
   inside the volume in canonical relative form: non-empty components separated by single
   `/`, none `.` or `..`, no leading `/` or `-` (so it can't be read as a flag), no control
   characters, at most 1024 bytes. flashcp resolves it inside the volume it parses, never on
   the local file system. The `pull` destination must be absolute, at most 4096 bytes, no NUL;
   see "The pull destination" below for why nothing else is checked. `<dev>` is matched exactly,
   with no normalisation, so `/dev/../dev/sda`, `/dev/disk/by-id/...` and trailing spaces fail.
3. **Open once.** `<dev>` is opened once per job: `O_RDWR|O_EXCL|O_NOFOLLOW|O_CLOEXEC` for
   `add`/`build`/`optimize` (its dry run too: flashcp requires a read-write fd for optimize), `O_RDONLY|O_EXCL|O_NOFOLLOW|O_CLOEXEC` for `pull`, `O_RDONLY|O_NOFOLLOW|O_CLOEXEC`
   for `info`. With `O_EXCL` the kernel refuses
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

### pull: read-only, but exclusive

`pull` never writes the drive, so the fd flashcp gets is `O_RDONLY`: a bug in flashcp's pull
code, or a process that takes the fd over, cannot modify the stick. It is still opened with
`O_EXCL`, which on a block device is an exclusive claim independent of the access mode:

- The idle check (not mounted, no holders) stays true for the whole copy instead of being a
  snapshot. Without the claim, udisks could automount the stick mid-copy and the kernel's exFAT
  driver would start writing metadata (at least the dirty flag) under a reader that assumes a
  quiescent volume. The copy would then be inconsistent, not unsafe, since flashcp already
  treats the volume as untrusted, but there is no reason to allow it.
- The cost is that the user can't mount the drive while a pull runs. The app unmounts it before
  the job and remounts it afterwards, exactly as for `add`.
- A read-only open does not make udev re-probe the device on close (that follows
  `IN_CLOSE_WRITE`), so a pull leaves no trace on the drive or in udev.

### optimize: the temporary copy

`optimize` copies the whole volume to a local directory, verifies it, erases and rebuilds the
drive from it, verifies the drive and deletes the copy. While the drive is being rebuilt that
copy is the only complete one, so where it lives matters for both safety and privacy.

- **Who picks it: the helper, not the caller.** The request has no path in it. The helper
  builds `<home>/.cache/flashcp/optimize` from the invoking user's **passwd** home directory
  (the same `getpwuid_r` entry the privilege drop uses), never from argv or the environment
  (pkexec scrubs it anyway, and `XDG_CACHE_HOME` is deliberately not honoured). A home that is
  not absolute, is `/`, has `.`/`..`/empty components or control characters, or is over 3072
  bytes is refused before the device is opened.
- **Root never touches it.** Like the pull destination, the helper never stats, creates,
  chowns or resolves that path; it is one argv string. flashcp creates it (`mkdir -p`, the job
  directory itself `0700`) after the privilege drop, so the kernel applies the user's own
  rights, and a symlink planted there can only lead to places the user could write anyway.
  flashcp also refuses a job directory not owned by the user, one on the drive being
  optimized, and one on a filesystem not known to be disk-backed (tmpfs, ramfs, overlayfs,
  zram, network): the helper never passes `--temp-volatile-i-accept-data-loss`, so a home on
  RAM makes the real run refuse instead of risking the only copy.
- **Why the home and not `/var/tmp` (flashcp's own first default).** The copy is private (in
  the user's home, under their quota, the job directory `0700`) instead of sitting in a shared
  sticky directory; `systemd-tmpfiles` ages out `/var/tmp` and clears `/tmp`, but not
  `~/.cache`; and the location is fixed, so every run of every drive, and above all every
  resume, finds an interrupted job in the same place regardless of the session's environment.
- **Resume is just the same request again.** flashcp detects an interrupted job in that
  directory itself and only continues onto the very volume it was building (job id, geometry,
  device identity), or onto the untouched original. The helper needs no extra mode and allows
  no extra flag for it; a stick left without a filesystem passes the same device, idle and
  trust checks as any other (none of them looks at the filesystem). A job started from the
  command line with flashcp's default temp location is resumed from the command line.
- **No typed confirmation.** Unlike `build`, `optimize` keeps the data, so the helper does not
  require `--confirm`; the app asks for one click after showing the dry-run plan. Malware in
  the session could start it (see Residual risks), which costs time and local disk space but
  not the drive's files.

### The pull destination

The destination is chosen by the user and is the only path in a helper request that points at
the local file system. The helper, running as root, **never looks at it**: no `stat`, no
ownership or permission check, no `realpath`, no `mkdir`. Every such check would be a root-side
check of a user-controlled path, open to symlink swaps between check and use, and would also
require the helper to reproduce the kernel's permission rules (ACLs, groups, mount options,
LSMs) for another user. Instead:

- The helper only requires an absolute path (so it can't parse as a flag and doesn't depend on
  the cwd) and passes it as one argv element.
- flashcp runs as the invoking user (dropped uid, gid and groups, no new privileges, see step
  7), and it is flashcp that creates and opens the destination. So the kernel enforces "a
  directory the user owns or can write", with the user's real credentials, at the moment of
  use. `pull` into `/etc` or `/root` fails with EACCES like it would for the user in a shell.
- Inside the destination flashcp writes only through directory fds (`openat2` with
  `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, `O_NOFOLLOW`), and `--overwrite` renames a temp file
  over the old one instead of truncating it. A symlink planted in the destination during the
  copy can't redirect a write, and the worst case of a hostile volume is files the user could
  have created themselves.

The GUI refuses a destination inside the drive's own mount point, and runs the
not-empty/skip/overwrite dialog as the user, but none of that is a security boundary.

## flashcp-mountpoint (drive mode)

Drive mode serves a USB exFAT partition through a per-user FUSE mount and wants it at
`/media/$USER/<LABEL>`, where udisks puts drives and file managers look. `/media/$USER` is
`root:root 0750` with an ACL for the user, so the user cannot create the directory. Source:
`gui/helper/src/mountpoint.rs`. Polkit allows it for an active local session without a
password (`allow_active=yes`, the level of udisks' own `filesystem-mount`); remote and
inactive sessions are refused.

**What it can do.** `create <NAME>` and `remove <NAME>`, nothing else: make an empty `0700`
directory owned by the caller at `/media/<caller>/<NAME>` (or `NAME1`..`NAME99` if taken, as
udisks does), create `/media/<caller>` (`0750`, `root:<caller's primary group>`) if missing,
and `rmdir` such a directory. It never writes files, mounts, runs programs or reads the
environment other than `PKEXEC_UID`.

**Checks.**

- Refused with exit 3 unless euid is 0 and `PKEXEC_UID` is a plain non-zero decimal uid with a
  passwd entry. `SUDO_UID` is not accepted. The user name and directory come from that uid,
  never from the caller's input.
- `NAME` is one path component: 1 to 255 bytes, UTF-8, no `/`, NUL or control characters, not
  `.`/`..`, no leading dot. Invalid names touch nothing.
- `/media` and `/media/<caller>` are opened with `O_DIRECTORY|O_NOFOLLOW`. An existing
  `/media/<caller>` must be a real directory owned by root and not group/other writable.
- An existing entry is reused or removed only if it is a real directory (not a symlink), owned
  by the caller, empty, and not a mountpoint (its `st_dev` differs from the parent's). Anything
  else is skipped (`create`) or refused with nothing removed (`remove`).
- Everything is `openat`/`mkdirat`/`unlinkat` relative to those directory fds, and ownership
  is set with `fchown`/`fchmod` on the opened entry's fd, never by path. The user cannot write
  to `/media/<caller>`, and a symlink swapped in anywhere during the run cannot redirect an
  operation. `remove` is `unlinkat(AT_REMOVEDIR)`, which the kernel itself refuses for a
  non-empty directory or an active mount.

**Residual risk.** Any process of the logged-in user can create up to 100 empty directories
per name under their own `/media/<user>` and remove their own empty ones, with no password. A
process can therefore squat a label (`create STICK`) so that a drive mounts as `STICK1`; this
is cosmetic. A root-owned `/media/<caller>` that was made group- or world-writable by someone
else is refused, not repaired. The group ownership of a created `/media/<caller>` is a
stand-in for udisks' ACL: members of the user's primary group (usually private) can list it.

## Drive mode: flashcp-udev-check

`61-flashcp-drive-mode.rules` runs `/usr/libexec/flashcp-udev-check $devnode` (as root, from
udev's `IMPORT{program}`) for `SUBSYSTEM=block`, `ID_USB_DRIVER=usb-storage|uas` (USB mass storage, including UAS SSDs, which udev reports as `ID_BUS=scsi`), `ID_FS_TYPE=exfat`,
`ID_FS_USAGE=filesystem` and `DEVTYPE=partition` (or a whole disk without a partition table),
unless `/etc/flashcp/drive-mode-disabled` exists. Its output decides whether udisks may see the
device (`UDISKS_IGNORE=1`) and whether the seat user gets the `uaccess` ACL
(`FLASHCP_DRIVE_MODE=1`). Source: `gui/helper/src/bin/flashcp-udev-check.rs` (about 200 lines).

**Threat model.** The input is a hostile USB stick: an arbitrary exFAT image built to crash,
hang or exploit the parser, possibly also stalling I/O (a device that never answers). The
helper itself parses nothing from the device; `flashcp mount --check` does, in a transient
systemd service that PID 1 starts for it (`systemd-run --wait --pipe`):

- Why PID 1: everything udev runs inherits `systemd-udevd.service`'s seccomp filter, which
  denies Landlock, mount, pivot_root and seccomp (`SystemCallErrorNumber=EPERM`), so no
  filesystem sandbox can be built inside the helper. The service is built by systemd instead,
  and any part of it failing fails the unit, which means no takeover.
- The service (`SANDBOX` in the source) runs as a fresh `DynamicUser` with no capabilities and
  `NoNewPrivileges`. It has a private network (`PrivateNetwork`, `RestrictAddressFamilies=none`,
  `IPAddressDeny=any`), private IPC, devices and /tmp, a read-only system (`ProtectSystem=strict`),
  no /home, and empty tmpfs over /etc (only `ld.so.cache` bound in), /var, /run, /srv, /opt, /mnt,
  /media and /root. It also gets the kernel/cgroup/clock/hostname protections, a
  `@system-service` seccomp filter minus `@privileged @resources @mount`, no W^X memory, no
  realtime, no SUID, no new namespaces, `MemoryMax=256M`, `RuntimeMaxSec=2`, `LimitCPU=2`,
  `LimitFSIZE=0`, `LimitNOFILE=16` and no core dumps.
- Its only input is the device, opened **read-only** by the helper and passed as stdin (fd 0);
  the environment is empty. It can read the system's binaries and libraries, `/sys` and its
  own `/proc` (the check's busy test: holders, partitions, mounts), nothing else.
- So a parser bug gives the attacker code execution as a throwaway uid with a read-only fd of
  their own stick, no writable files, no network, and two seconds. The worst outcome of a wrong
  answer is cosmetic or a fallback: "serve it" means the user's own FUSE driver opens it (and
  its own checks run again), "don't" means udisks does.
- **udev is never stalled:** the helper waits at most 1.5 s for `systemd-run`, then SIGKILLs it
  and does not wait for it; the service itself is ended by `RuntimeMaxSec`. No answer, a
  failure, a timeout, or any oddity prints nothing: the device is left to udisks exactly as
  without flashcp. Its stdout (at most 4 KB) comes back through a pipe: a check that could not
  do its busy test (`busy_unknown`) is not a takeover.

**The active user.** The helper reads `ACTIVE_UID` of `/run/systemd/seats/seat0` (logind's
state file; no D-Bus). uid 0 or none: nothing. It then looks under `/run/user/<uid>/flashcp/`
with `lstat` only (never following a link), and every entry must be owned by that uid:
`drive-mode-on` (a regular file the user's daemon creates while it runs with drive mode on) and
`kernel/<UUID or device name>` (the daemon handed this drive back to the kernel driver, so the
helper prints `UDISKS_AUTO=0`: visible, mounted by the daemon through udisks). The only thing a
user controls here is whether files exist in their own runtime dir, and the only effect is
whether *their own* seat gets their drive through flashcp or through udisks.

**Residual risk.** The ACL that `uaccess` applies stays on the device node until it is
unplugged or the seat changes hands (logind moves the ACLs then), also after the daemon hands
the drive back to the kernel driver. A drive whose label or contents change while plugged is
re-checked on udev's change events; a busy answer keeps the previous decision
(`FLASHCP_KEEP=1`, `IMPORT{db}`).

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
- **Reading a trusted drive gives the user its raw contents.** During a `pull` (and `info`) the
  user's flashcp holds a read-only fd to the whole device, so it can read deleted-but-not-
  overwritten data on that drive too. The drive is one an administrator trusted and the user
  could usually mount anyway; the polkit authentication is the same as for a copy to it.
- **A compromised session can start an optimize.** Same preconditions as an erase. It keeps
  the files (a verified copy is made first and the drive is checked against it), but it
  rewrites the drive and needs free space in the user's home. An interrupted optimize leaves
  the drive unreadable until it is resumed on this computer; Windows may offer to format it,
  which would erase it while the only other copy is the one in `~/.cache/flashcp/optimize`.
  The app says so in the plan and in every error that carries flashcp's recovery text.
- **The temporary copy holds the drive's files in the user's home** until optimize finishes
  (or, after a refused resume, until the user deletes it). They are files the user could
  already read off the drive.
- `add`, `pull`, `info`, `optimize` and the confirmation run parse an existing, untrusted exFAT volume. A malicious
  stick could exploit a flashcp parser bug, but only as the unprivileged user, with no new
  privileges.
- Opening a device read-write (even one that is then refused) makes udev re-probe it when the
  fd is closed. No data is written.
- The mount, swap and holder checks are a snapshot. `O_EXCL` keeps new mounts and exclusive
  claims off the device while the helper holds it.
- `SUDO_UID` is trusted when present without `PKEXEC_UID`. Only root can start the helper,
  and root could do all of this without it.
