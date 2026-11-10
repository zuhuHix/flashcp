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
{"event":"committing","bytes":N,"files":N,"reason":"idle"|"fsync"|"flush"|"eject"|"pressure"|"unmount"}
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

A Unix stream socket created by the driver (mode 0600; an existing stale socket file is
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
