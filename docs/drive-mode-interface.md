# Drive mode: the `flashcp mount` interface

This is the contract between the driver core (`flashcp mount`, `src/drive/`) and the system
integration (udev/udisks takeover, the per-user daemon, fallback, eject guard, tray, off
switch, packaging). It is versioned: a change that breaks a consumer bumps `"proto"` in the
`mounted` event and in `status`. Current: **proto 1**.

## Command line

```text
flashcp mount <DEVICE> <MOUNTPOINT> [--json] [--control <SOCKET>] [--device-fd N]
              [--idle-ms N] [--max-dirty-mb N] [--threads N]
flashcp mount --check <DEVICE> [--json] [--device-fd N]
```

- `<DEVICE>`: a block device (partition or superfloppy) or an image file. With `--device-fd N`
  the inherited fd is used instead (opened O_RDWR; `<DEVICE>` is then only used in messages,
  pass the device path anyway). The fd must be a block device or a regular file; the driver
  takes the same exclusivity claim on it as `add`/`sync` (an `O_EXCL` reopen through
  `/proc/self/fd/N` where that works, plus `flock(LOCK_EX|LOCK_NB)`), and refuses if any
  partition of the disk is mounted.
- `<MOUNTPOINT>`: an existing, empty directory owned by the calling user. The driver mounts
  through `fusermount3` (no root; `allow_other` is NOT requested, so only the mounting user
  sees the files: run one `flashcp mount` per seat user).
- The process runs in the foreground until the volume is unmounted (by `unmount` on the
  control socket, `fusermount3 -u`, or SIGTERM/SIGINT). It never daemonizes.
- `--close-ms` (default 150, 0 = off): commit this long after the last file open for writing
  is closed. **`sync`/`syncfs` are not forwarded to FUSE servers** (only to virtiofs and root-only
  `fuseblk` mounts), so this is what makes `cp … && sync` close to durable: a closed file is on
  the medium within 150 ms + one commit. Eject through `prepare-eject` stays the safe way.
- `--idle-ms` (default 1000): commit this long after the last write. `--max-dirty-mb`
  (default 256): writers block once this much is staged and not yet durable (a commit starts
  at half of it). It is also the most a yank can lose. 256 MB keeps `cp` of a typical tree
  returning as fast as with the kernel's page cache; a smaller value shortens the eject wait
  on slow sticks. `--threads N` (default 1): FUSE session threads.
- Runs as the unprivileged user. It needs read-write access to the device (the uaccess ACL
  that logind gives the seat user is enough), and `/dev/fuse` + `fusermount3`.

### `--check`

Reads at most the boot region, the up-case table, the allocation bitmap's location, the root
directory and the journal locations (no full walk; ≤ ~200 ms on a USB stick). It never writes:
a pending journal is reported (`pending_journal`), not replayed; the real mount replays it.
The busy test comes FIRST: a device that is mounted (by the kernel or anything else), has a
mounted partition or a holder (dm, md, LVM), or cannot be claimed with `O_EXCL` exits **4**, never 3, whatever
its filesystem, so the daemon never mounts a volume twice. It prints one JSON line with `--json` (else a sentence) and exits with the code
below. `0` means "flashcp would serve this volume".

## Exit codes (both modes)

| code | meaning | integration should |
|---:|---|---|
| 0 | `--check`: flashcp can serve it. mount: unmounted cleanly, everything durable. | — |
| 1 | runtime error after mounting (I/O error, the device vanished). The volume is consistent (crash-safe); pending data that could not be written is reported in an `error` event. | show the error; do not retry in a loop |
| 2 | usage error (bad arguments) | bug |
| **3** | **FALLBACK: give this volume to the kernel driver.** Nothing was written. The reason is in the JSON (`refused` event). | mount it with the kernel driver (udisks) |
| 4 | BUSY: the device is mounted or held by someone else, or `O_EXCL`/`flock` failed. Nothing was written. | do not fall back blindly; retry later or report |
| 5 | NO ACCESS: the device (or `/dev/fuse`, `fusermount3`) cannot be opened with the needed rights. | fix ACL / fall back to the kernel driver |
| 6 | RECOVERY REFUSED: an interrupted flashcp commit/sync journal is on the volume but cannot be replayed safely (the volume changed since). Nothing was written. | fall back to the kernel driver read-only, tell the user to run `fsck.exfat` |

### The `refused` reasons (exit 3)

`{"event":"refused","reason":R,"message":"...","device":"/dev/sdb1","label":L|null,"serial":S|null}` with `R` one of
(`label`/`serial` are those of an exFAT boot region that could be read, the serial as 8 hex
digits = the `UUID` udev shows as `XXXX-XXXX`; null otherwise):

| reason | when |
|---|---|
| `not_exfat` | no exFAT boot sector (FAT32, NTFS, ext4, unformatted, ...) |
| `dirty` | VolumeDirty set and no flashcp journal explains it (another OS or driver did not unmount cleanly; the kernel driver or fsck should deal with it) |
| `invalid` | the quick validation failed (bad boot checksum, impossible geometry, damaged up-case table, bitmap, root directory) |
| `unsupported` | uses something the driver does not implement: a second FAT (TexFAT, NumberOfFats = 2), sector size ≠ 512…4096, a volume over 2^32 clusters, … |
| `too_small` | smaller than the driver's minimum (1 MiB) |

A reason the integration does not know must be treated as `invalid` (fall back).

### What the daemon does with each code (agreed with the integration, 2026-11-10)

| code | daemon behaviour |
|---:|---|
| 3, or an unknown `refused` reason | kernel mount, read-write |
| 4 | someone else has it mounted: leave it alone. Otherwise retry 3× (after 1, 3, 10 s), then notify only. Never a kernel mount. |
| 5 | kernel mount read-write, and log it |
| 6 | kernel mount READ-ONLY plus a persistent "run fsck.exfat" notification. Never read-write. |
| 1 (while mounted) | one restart of `flashcp mount` (which recovers from the journal), then kernel mount + notify |

## JSON events (stdout, `--json`)

One object per line, flushed after each. Human-readable messages go to stderr regardless.

```text
{"event":"mounted","proto":1,"device":"/dev/sdb1","mountpoint":"/media/u/KINGSTON","label":"KINGSTON",
 "serial":"1a2b3c4d","size":N,"free":N,"cluster":N,"pid":N,"recovered":null|"replayed"|"dirty_cleared"}
{"event":"dirty_bytes","bytes":N,"files":N}        pending (not durable) data; at most ~4 per second,
                                                   and always once when it drops to 0
{"event":"committing","bytes":N,"files":N,"reason":"close"|"idle"|"fsync"|"flush"|"eject"|"pressure"|"unmount"}
{"event":"committed","bytes":N,"files":N,"secs":F}   everything up to here is durable on the medium
{"event":"error","message":"...","fatal":B}          fatal=true: the mount is going away (exit 1)
{"event":"unmounted","clean":B}                      last line; clean=false only after a forced exit
{"event":"refused","reason":R,"message":"...","device":"...","label":L|null,"serial":S|null}
                                                   (exit 3), see above; also printed by --check
{"event":"busy","message":"...","device":"..."}    (exit 4)
{"event":"check","ok":B,"reason":R|null,"label":L|null,"serial":S|null,"pending_journal":B}  --check only
```

`dirty_bytes` counts bytes that were written by applications and are not yet on the medium
(including metadata-only changes, counted as 0 bytes but `files` ≥ 1). `bytes == 0 && files ==
0` means "safe to remove" as far as data goes (the device still has to be released by
unmounting).

## Control socket (`--control <SOCKET>`)

A Unix stream socket created by the driver (the path must fit `sun_path`, at most 107 bytes;
use e.g. `$XDG_RUNTIME_DIR/flashcp/<serial>.sock`; mode 0600; an existing stale socket file is
replaced). One request per line, one JSON reply per line; several requests may be sent on one
connection, and several connections may be open.

| request | reply | notes |
|---|---|---|
| `status` | `{"ok":true,"proto":1,"state":"idle"\|"dirty"\|"committing"\|"read_only"\|"ejecting","dirty_bytes":N,"dirty_files":N,"open_files":N,"free":N}` | immediate |
| `flush` | `{"ok":true,"state":"idle"}` once everything written before the request is durable | blocks; writers are not stopped |
| `prepare-eject` | `{"ok":true,"safe":true}` | flush, then refuse new writes (EROFS) and new opens for write, then flush again; reports safe when nothing is pending. Reads still work. Files still open for writing: their written data is committed; later writes fail with EROFS. A second `prepare-eject` is a no-op. `resume` undoes it. |
| `resume` | `{"ok":true}` | writes allowed again after a `prepare-eject` |
| `unmount` | `{"ok":true}` then the process unmounts and exits | equivalent to SIGTERM |

On failure: `{"ok":false,"error":"..."}`. Unknown commands get `{"ok":false,"error":"unknown command"}`.

## Signals and lifetime

- **SIGTERM / SIGINT**: commit everything pending, then unmount (lazy unmount via
  `fusermount3 -u -z` if busy) and exit 0.
- **A second SIGTERM**, or a commit that does not finish within 30 s of the signal: stop
  staging, finish (or abandon) the commit in progress at a phase boundary, unmount and exit 1.
  The volume is consistent either way: every file is its old or its new committed content;
  data that never got committed is lost and reported (`error` event).
- **SIGKILL / power loss / yank**: the on-volume + host journal makes the next `flashcp mount`
  (and `flashcp sync`) replay or roll back the interrupted commit. The volume is fsck-clean
  after recovery, and before it is at worst "dirty with leaked clusters" (`fsck.exfat -y` also
  fixes that).
- `fusermount3 -u` from outside: the FUSE session ends; the driver commits what is pending
  before exiting (it cannot delay the unmount itself: use `prepare-eject` before unmounting).
  The integration should always do `prepare-eject` → `unmount` and only then power off.

## Semantics the integration can rely on

- Only the driver writes to the device while it runs; the kernel driver must not be mounted
  at the same time (the claim enforces this).
- Names are case-insensitive (exFAT up-case table), case-preserving; Windows-illegal
  characters (`"*/:<>?\|` and controls) give EINVAL; names over 255 UTF-16 units ENAMETOOLONG.
- Ownership is the mounting user, modes 0644/0755 (0444/0555 for the read-only attribute);
  chmod toggles only the read-only attribute; chown, symlinks, hard links, xattrs: EPERM/ENOTSUP.
- statfs reports the real free space minus the pending allocation.

## Offering `flashcp recluster` ("Make this stick faster")

A stick formatted the usual way has big clusters (32 KB at 8 GB, 128 KB at 128 GB), so every
small file takes a whole one and commits write mostly padding. `flashcp recluster` gives the
volume 4 KB clusters in place: it rewrites only the metadata (boot sector, FAT, bitmap,
directories); file data stays where it is, except what lies in the first ~0.1 % of the volume,
where the bigger FAT has to go. This section is the contract the daemon (`flashcp drived`) and
the GUI build on; [What the daemon does](#what-the-daemon-does-with-it) below is how they use it.
It is never run without a click.

```text
flashcp recluster --check <DEVICE> [--json] [--target-fd N]
flashcp recluster <DEVICE> [--cluster SIZE] [--fat prefix|full] [--dry-run] [--json] [--target-fd N]
```

### 1. Decide whether to offer: `--check`

Read-only and without an exclusive claim (like `info`): it reads the boot regions, the root
directory and the allocation bitmap, no other directory (well under a second on a stick). With
`--target-fd` the fd may be O_RDONLY. Run it when the stick arrives, before or after
`mount --check`; the volume must not be dirty.

```text
{"event":"check","op":"recluster","benefit":B,"reason":R,"message":"...","cluster":N,"new_cluster":N,
 "free":N,"need_free":N,"write_bytes":N,"est_secs":F,"finish":B}
```

Exit **0**: `benefit` is true (`reason` is `"ok"`): the clusters are 16 KB or bigger and there
is room to convert. Offer it once per stick (key the "asked" state by the volume serial plus
the device identity): **"Make this stick faster (about N seconds)"**, N = `est_secs` rounded up,
with **Accept** and **Not now**. `est_secs` and `write_bytes` are estimates from the device's
cached profile and the bitmap (no directory is read, no file is walked). They hold for the
usual stick; they can be far too low when a big FAT-chained file starts in the first ~0.1 % of
the volume (it moves whole, from its first cluster). The real run's `plan` line is exact: show
its `est_secs` once it arrives (and stop with SIGTERM before the first `switched` if it is
much more than offered: nothing is changed before then). Do
not promise a speed-up factor: what changes is that small files need smaller writes.
Exit **3**: do not offer; `reason` says why (table below). Exit 1: an error (unreadable
device); do not offer.

`"finish":true` (with either exit code) means an earlier conversion was interrupted between
its two last writes: the volume is complete and valid, only the backup boot sector still
describes the previous state. Run `flashcp recluster <DEVICE>` once without asking (it is
part of the conversion the user accepted; with small clusters already it writes that one
sector and exits 0).

### 2. On Accept

The command needs the exclusive claim, so the driver must not have the volume mounted:
`prepare-eject` → `unmount` on the control socket, then
`flashcp recluster <DEVICE> --json` (or `--target-fd N` with an O_RDWR|O_EXCL fd), then mount
again with `flashcp mount`. One JSON object per line on stdout:

```text
{"event":"plan","op":"recluster","cluster":N,"new_cluster":N,"files":N,"dirs":N,"bytes":N,
 "write_bytes":N,"move_bytes":N,"est_secs":F,"free":N,"free_after":N,"fragmented":N,"switches":1|2}
{"event":"progress","op":"recluster","bytes_done":N,"bytes_total":N}      at most ~10 per second
{"event":"switched","op":"recluster","step":"relocated"|"reclustered","cluster":N}
{"event":"done","op":"recluster","changed":B,"cluster":N,"old_cluster":N,"written":N,"free":N,
 "secs":F,"finished_switch":B}
{"event":"refused","op":"recluster","reason":R,"message":"..."}            exit 3, nothing was written
{"event":"error","message":"..."}                                          exit 1
```

- `plan` comes before the first write (`--dry-run` stops after it): `bytes` is the file data
  on the volume, `write_bytes` everything that will be written, `move_bytes` the file data
  that has to make room for the new FAT, `est_secs` the estimate for the progress bar,
  `fragmented` the files that are in more than one piece (they stay so).
- `switched` marks a commit point. After `"step":"reclustered"` the volume has the new cluster
  size; `"relocated"` (only when `switches` is 2) is the intermediate one: still the old
  cluster size, the room for the new FAT is free.
- `done` with `"changed":false`: the clusters already had the wanted size.
- Exit 0 = done, 3 = refused (a `refused` line, nothing written, except that an interrupted
  run's backup boot sector is always completed first: its message then says so), 1 = error,
  2 = usage.

**If it is interrupted** (yanked, power loss, killed): the volume is the old one or the new
one, complete and fsck-clean, never a mix: every write goes to free space until one 512-byte
write of the boot sector switches over. Nothing has to be cleaned up and nothing is kept on the
host. One exception, measured: a yank in the few milliseconds between the main and the backup
boot sector write (once per switch) leaves a volume that Linux and macOS mount but **Windows
does not** (it wants both boot regions equal, and may offer to format the stick). The data is
intact; plugging it into a machine with flashcp completes it (`--check` reports `finish`, run
`flashcp recluster` once). So: run it only from drive mode, never ask the user to unplug
during it, and say "do not unplug" in the progress UI. On the next arrival run `--check` again: `finish` → complete it as above; `benefit`
still true → the conversion did not happen (or got halfway: the second run is then shorter);
offer or resume it as the product decides.

It cannot be undone cheaply (bigger clusters again mean moving every file: `optimize`). The
label, the serial number (`UUID`), every file's name, attributes and timestamps stay. Four
bytes of the boot code area of the boot sector change (that is what makes the switch one
sector; they lie below offset 440, clear of where a partition table would be); nothing reads
them unless a BIOS boots from the stick's exFAT boot code. Drive mode's
idle journal file is not carried over; `flashcp mount` creates a new one at its first commit.

### Reasons (`--check` and `refused`)

| reason | meaning | integration should |
|---|---|---|
| `ok` | (`--check` only) worth offering | offer |
| `small-clusters` | (`--check` only) clusters below 16 KB | nothing |
| `dirty` | VolumeDirty is set | nothing (the mount path already deals with it) |
| `not-exfat`, `invalid-volume` | no exFAT volume, or the validation the other commands apply failed (the message says what) | nothing |
| `unsupported-sector-size` | sectors are not 512 bytes | nothing |
| `unsupported-entry` | TexFAT, or a vendor entry that owns clusters | nothing |
| `pending-commit` | an interrupted drive-mode commit: `flashcp mount` replays it | mount first, check again later |
| `pending-sync` | an interrupted `flashcp sync` (its journal file is on the volume) | nothing until that sync is finished |
| `backup-boot-mismatch` | the backup boot region differs from the main one in a way a recluster does not leave | nothing (`fsck.exfat` territory) |
| `no-space`, `no-contiguous-space` | too little free space behind the first ~0.1 % of the volume, or it is too fragmented | nothing; may be worth asking again when files were deleted |
| `geometry`, `too-many-clusters`, `too-many-files`, `checksum` | the volume cannot be described with clusters that small (or is too large to plan in memory) | nothing |
| `bad-clusters` | clusters are marked bad in the FAT; a recluster would lose the marks | nothing |
| `bad-cluster-size` | `--cluster` is not a power of two below the current size | bug in the caller |

A reason the integration does not know means "do not offer".

### What the daemon does with it

(In the next release.) After `flashcp mount --check` said 0, and only with drive mode on:

1. `flashcp recluster --check --json DEV` (10 s timeout; a failure or no `check` line = "no").
   - `"finish":true`, first time for this plug-in: `flashcp recluster --json DEV` BEFORE
     mounting, without asking. Exit 0: one notification ("KINGSTON was repaired"), then
     `flashcp mount`. Exit 3: mounted as usual (a pending drive-mode commit is replayed by the
     mount). Anything else: one notification, kernel mount READ-ONLY. One attempt per plug-in, so
     the two checks cannot loop.
   - Otherwise `benefit` (exit 0) is kept as `can_convert`, and the drive is mounted.
2. Once flashcp has it mounted, the offer notification goes out when: `can_convert`, clusters
   ≥ **32 KB** (`OFFER_MIN_CLUSTER`: every stick of 256 MB and up formatted the usual way; a
   16 KB stick, which `--check` also accepts, gets only the app button), the volume has a
   serial, its key has no answer yet, and the offer is on. Once per plug-in at most.
3. Accept (the notification, the app's **Make faster**, `flashcp drive-mode recluster DEV`):
   `prepare-eject` (writes refused from now on, everything committed) → `unmount` → the driver's
   exit goes through the same handover choke point as an eject: only **Clean** (exit 0 after a
   clean `unmounted` with nothing pending) goes on to `flashcp recluster --json DEV`; anything
   else is the usual unclean path (kernel read-only, "couldn't finish saving"). recluster opens
   the device itself, with the same rights and claim as `flashcp mount` (the uaccess ACL; no
   helper). Then `flashcp mount` again on the same mountpoint, and a notification once it is
   mounted: done, refused (exit 3, "nothing was written; it works as before") or failed (exit 1,
   the volume is the old or the new one, complete). `prepare-eject` refused: `resume`, nothing
   changed, a notification.
4. While it runs (`"state":"converting"`): writes fail (EROFS), Eject waits and then powers off
   instead of remounting, the sleep/shutdown delay inhibitor is held, a daemon stop waits for
   recluster and then hands the drive back, drive mode off hands it to the kernel afterwards. A
   yank: a notification ("plug it in here again before using it on Windows"); the next plug-in
   runs step 1 again.

The answers: `$XDG_STATE_HOME/flashcp/recluster-offers` (default `~/.local/state/...`), one
`<UUID>:<label>\t<accepted|declined>` line per volume (`volume_key`: the serial and label both
survive a conversion). Accept → `accepted`; Not now, dismissed or expired → `declined`; never
offered again by notification. No answer (the server has no "actions" capability, no server,
the drive was unplugged first) → nothing is written, it is offered again on the next plug-in.
The global off switch is `offer_faster=false` in `$XDG_CONFIG_HOME/flashcp/drive-mode.conf`
(`flashcp drive-mode offer on|off`).

The notification is sent through `org.freedesktop.Notifications` with `gdbus`:
`GetCapabilities` must list `actions`; `gdbus monitor` is started first, then `Notify` with
actions `accept` ("Accept") and `notnow` ("Not now"), expire timeout 0; the answer is the
`ActionInvoked` (then `CloseNotification`) or `NotificationClosed` (reason 1 or 2 = declined,
3 = closed by the daemon = no answer) for that id. Texts (KINGSTON = the label, N from `est_secs`):

| when | title | body |
|---|---|---|
| offer | Make KINGSTON faster for small files? | flashcp can change how KINGSTON stores files so that small files copy much faster. Nothing is erased. It takes about N seconds and you must not unplug it meanwhile. |
| running | Making KINGSTON faster… | Don't unplug it. This takes about N seconds. |
| done | KINGSTON is ready | Small files now copy to it much faster. |
| refused | KINGSTON was not changed | There is not enough free space on it. Delete some files and try again. (or: flashcp found something it does not change safely (MESSAGE).) Nothing was written; it works as before. |
| failed | Could not make KINGSTON faster | MESSAGE. Your files are safe and it works as before. |
| pause refused | KINGSTON was not changed | It is in use (ERROR). Nothing was changed. Close the files on it and try again. |
| yanked | KINGSTON was unplugged while it was being changed | Your files are safe. Plug it in here again before using it on Windows: flashcp checks it and finishes the change if needed. |
| finished on plug-in | KINGSTON was repaired | A change to make it faster had been interrupted; it is now complete. Nothing was lost. |
| finish failed | KINGSTON is read-only for now | flashcp could not complete an earlier change to it (MESSAGE). Your files are there: copy them off, or plug it in again to retry. |

## The daemon's control socket (`flashcp drived`)

`$XDG_RUNTIME_DIR/flashcp/drived.sock` (mode 0600), one request per line, one JSON line back.
`flashcp drive-mode ...` is its client; the app runs that CLI with `--json`.

| request | reply |
|---|---|
| `status` | `{"ok":true,"running":true,"enabled":B,"offers":B,"drives":[DRIVE,...]}` |
| `enable` / `disable` | `{"ok":true,"enabled":B}` |
| `eject DEV` | `{"ok":true}`, or `{"ok":false,"error":"no such drive"}` |
| `recluster DEV` | `{"ok":true}` (the conversion started), or `{"ok":false,"error":"..."}`: `no such drive`, `drive mode is off`, `it is being converted already`, `it is not ready (flashcp must have it mounted)`, `it would not get faster (...)` |
| `offers on` / `offers off` | `{"ok":true,"offers":B}` (the CLI writes the config first) |

`DRIVE`: `{"device":S,"label":S,"mode":"flashcp"|"kernel"|"none","state":S,"mountpoint":S|null,
"dirty_bytes":N,"dirty_files":N,"safe_to_remove":B,"can_convert":B,"cluster":N,"convert_secs":N,
"convert_done":N,"convert_total":N,"convert_result":"done"|"refused"|"failed"|null,
"convert_message":S|null}`. `state` is one of `checking`, `busy`, `starting`, `ready`, `saving`,
`ejecting`, `stopping`, `mounting`, `kernel`, `read_only`, `unmounted` (a kernel mount someone
unmounted by hand), `converting`, `safe_to_remove`, `stopped`, `failed`. `cluster` and
`convert_secs` come from `recluster --check` (0 when it said no); while converting,
`convert_secs` is the plan's estimate and `convert_done`/`convert_total` the progress bytes.
`convert_result`/`convert_message` describe the last conversion of this plug-in (the message is
the notification's title and body).

`flashcp drive-mode recluster DEV [--yes] [--json]` asks first (on a terminal; without one it
needs `--yes`), sends `recluster DEV`, prints progress while `state` is `converting`, and once the
drive is back prints the message and exits 0 (done), 3 (refused, or it could not start) or 1
(failed). With `--json` the last line is `{"ok":B,"result":R,"message":S}` (plus `"error":S` when
not done).
