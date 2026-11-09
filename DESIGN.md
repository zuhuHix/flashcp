# Drive mode: design notes and spike results

Drive mode means USB exFAT drives are mounted through a flashcp FUSE filesystem instead of the
kernel exfat driver, so every copy from any app (Nemo, `cp`, a browser download) gets flashcp's
one-pass sequential writes and physical-order reads without opening anything.

This file records what the spike (`flashcp mount`, `src/mount.rs`) measured, what a full
version needs, and a recommendation.

## What the spike does

`flashcp mount <device|image> <mountpoint>` (fuser 0.18, pure Rust, mounts via `fusermount3`;
no libfuse needed) serves the volume:

- **Reads** of committed files go through the validating parser in `volume.rs`. Directories
  are parsed on first use. When a third file of one directory is opened for reading (a bulk
  copy, not browsing), the directory's remaining contiguous files are read in physical order
  in a few large reads (runs up to 64 MB, gaps up to 16 KB read through, 256 MB cache cap).
- **Writes** create staged files: in `/dev/shm` (RAM) until 512 MB are staged, then in
  `~/.cache/flashcp/stage-<pid>`. Nothing on the device changes while data is staged.
- **Commit**: the staged tree (new dirs and files, merged into existing directories) is handed to
  `add`'s engine (`add::add_tree`, a 10-line refactor: `add_to` = `scan_tree` + `add_tree`),
  which verifies the volume, allocates, and writes data → bitmap → FAT → directory entries
  with a sync between phases. Triggers: 1.5 s without writes and no file open for writing; an
  fsync of a staged file; unmount (the FUSE session ends, `destroy` commits before exit).
- Ops: lookup, getattr, setattr (size/mtime of staged files), readdir, mkdir, create, open,
  read, write, flush (ENOSYS, see below), release, fsync, unlink/rmdir/rename (staged objects
  only), statfs. Anything else gets fuser's ENOSYS; changing committed files gets EOPNOTSUPP.

### Missing in the spike (on purpose)

- `flashcp mount` must run as a user that can open the device read-write (the uaccess ACL
  on removable drives was enough for both test drives; no root needed).

- Overwriting, appending to, truncating, renaming or deleting **committed** files and dirs
  (EOPNOTSUPP). Renaming a non-empty staged directory (EOPNOTSUPP).
- After fsync of a file that stays open, further writes on that handle fail (it was committed).
- No eject guard: `fusermount3 -u` returns before the final commit finishes; the commit
  completes when the `flashcp mount` process exits. SIGTERM/SIGINT are not handled (staged
  data is lost; the volume is untouched).
- Permissions are fixed (0644/0755, owner = mounting user); chmod/chown/atime are ignored.
- No symlinks, xattrs, hard links, locks; mmap works for reads (page cache) only.
- Each commit re-verifies the whole volume (the add engine's safety walk), and re-parses
  the volume afterwards; a daemon could keep its verified model instead.
- Between commits the daemon does not hold the device exclusively (the commit takes the
  O_EXCL / flock claim per commit).

## Correctness results (images)

- 20k-file tree copied in, plus a second batch merged into existing directories (mkdir,
  write, rename, unlink of staged files): `fsck.exfat` clean, `diff -r` clean through the
  FUSE mount and through a kernel mount of the same image afterwards.
- SIGKILL of the daemon at 4 points during an 839 MB commit on top of 20k pre-existing files:
  `fsck.exfat` accepted the volume every time and the pre-existing tree diffed clean. A kill
  after phase 0 leaves VolumeDirty set (only leaked clusters, `fsck.exfat -y` reclaims); the
  daemon refuses to mount a dirty volume, like `add`.

## Measurements

All runs: `flashcp gen --profile small` (20,000 files, 169 MB, 201 directories of 100 files),
release build, Linux 7.0. Runs are interleaved (ABBA order per round, kernel and FUSE back to
back) because the machine had a load of 5–7 from other work; numbers are median (min–max) of
5 rounds. *cp* = time until `cp -r` returns (what the user sees); *durable* = until the data
is on the medium (kernel: cp + `sync` + unmount; flashcp: cp + `fusermount3 -u` + the final
commit); *idle durable* = until flashcp's own idle commit (1.5 s after the last write) has
finished, without unmounting.

### Image on the local NVMe (FUSE overhead itself)

| | kernel exfat (loop) | flashcp mount | kernel/flashcp |
|---|---:|---:|---:|
| cp (perceived) | 0.52 s (0.51–1.22) | 0.97 s (0.87–1.19) | 0.53× |
| durable | 0.85 s (0.84–1.66) | 1.25 s (1.18–1.50) | 0.68× |
| idle durable | — | 2.79 s (2.67–3.37) | |
| read back (cold) | 1.18 s (1.15–1.70) | 1.53 s (1.42–1.93) | 0.77× |

The commit itself (add's engine, verify + plan + 215 MB written) takes 0.21–0.29 s. Every
flashcp run passed `fsck.exfat` and a byte-for-byte diff of the read-back.

On a fast backing store the kernel driver wins: drive mode adds ~0.45 s per 20k files to cp
and ~0.4 s to durable time. The per-file costs:

| 20,000 empty files, one directory | cp |
|---|---:|
| tmpfs (no filesystem cost) | 0.09 s |
| flashcp mount | 0.39–0.49 s (**~18 µs per file of FUSE overhead**) |
| kernel exfat (loop on NVMe) | **17.9–18.4 s** |

So the FUSE overhead is ~18 µs per small file (lookup + create + write + release; FLUSH is
switched off by answering ENOSYS, which took the empty-file case from 0.72 s to 0.40 s;
FUSE writeback caching and extra session threads were both slower and are off). The kernel
exfat driver, though, is quadratic in directory size (each lookup of a not-yet-existing name scans
the whole directory): one folder of 20k files takes 18 s on the kernel driver even on an NVMe, while
drive mode's in-memory index makes it flat.

Reads: without readahead the FUSE read-back took 2.4 s on a quiet machine; with the
bulk-read readahead (directory's files read in one sequential run once a third file of it is
opened) it is 1.4–1.9 s.

### USB devices

Interleaved on the real drives, wiped before every step, `flashcp gen --profile small`
(5 rounds) and 20,000 empty files in one directory (3 rounds). Kernel = udisks mount of the
kernel exfat driver; durable = cp + `sync -f` + unmount. Median (min–max), seconds.

**MSI DATAMAG 20Gbps (UAS SSD, 1 TB partition)**

| | kernel exfat | flashcp mount | gain |
|---|---:|---:|---:|
| `small`: cp returns | 0.57 (0.54–0.69) | 0.91 (0.75–1.00) | 0.62× (slower) |
| `small`: **durable** | 1.83 (1.78–2.02) | **1.45 (1.28–1.55)** | **1.27×** |
| `small`: durable via idle commit | — | 3.01 (2.91–3.22) | |
| `small`: read back (cold) | 5.25 (4.95–5.46) | 4.37 (4.20–4.52) | 1.20× |
| 20k files, one dir: cp returns | 17.62 (17.58–17.89) | 0.43 (0.38–0.54) | 41× |
| 20k files, one dir: **durable** | 17.89 (17.86–18.17) | **0.51 (0.46–0.64)** | **35×** |

**SanDisk Cruzer Blade (USB 2.0 BOT stick, 7.3 GB)**

| | kernel exfat | flashcp mount | gain |
|---|---:|---:|---:|
| `small`: cp returns | 0.62 (0.52–0.92) | 0.85 (0.82–0.90) | 0.73× (slower) |
| `small`: **durable** | 206 (127–247) | **132 (107–155)** | **1.56×** |
| `small`: durable via idle commit | — | 109 (108–146) | 1.9× |
| `small`: read back (cold) | 29.9 (29.1–30.4) | 52.9 (52.8–53.3) → 33–39 with a 16 KB gap | 0.57× → ~0.85× |
| 20k files, one dir: cp returns | 17.44 (17.43–17.46) | 0.41 (0.40–0.46) | 43× |
| 20k files, one dir: **durable** | 18.04 (18.00–18.10) | **0.68 (0.67–0.96)** | **27×** |

Every flashcp run passed `fsck.exfat` and a byte-for-byte diff of the read-back. Both drives
were left as empty exFAT ("MSI DATAMAG", "SANDISK"), mounted through udisks.

Reading the numbers:

- **Durable time is what improves**: 1.27× on the fast SSD, 1.56× (unmount) to 1.9× (idle
  commit) on the stick. On the stick the commit is 106–154 s for 215 MB written, i.e. the
  stick's own sequential write speed (the same bound `flashcp add`/`build` hit), against
  127–247 s for the kernel's scattered writes.
- **cp returns later** (0.85–0.91 s against 0.57–0.62 s): `cp` lands in the kernel's page
  cache with the kernel driver, but pays ~18 µs per file of FUSE round trips with drive mode.
  The user sees "done" sooner with the kernel, though the data is not on the drive.
- **Big folders**: the kernel driver is quadratic in directory size; one folder of 20k files
  takes ~18 s on either drive (CPU, not I/O). Drive mode is 27–43× faster there. Camera
  folders, `node_modules`, and dataset dumps hit this.
- **Reads**: 1.2× faster on the UAS SSD. On the BOT stick, reading through cluster slack
  (32 KB clusters, 8 KB files: 3.7× the bytes) made readahead 1.8× *slower* than the kernel;
  with a 16 KB gap it is 33–39 s against the kernel's 30 s. A real version should reuse
  `pull`'s per-transport strategy (gap from a latency × bandwidth probe, several reads in
  flight, O_DIRECT) instead of the spike's serial buffered reads.
- A bug found only on the 1 TB drive: counting free space from the bitmap on every write cost
  ~1 ms per write (cp took 17 s); the spike now caches the count. Large volumes need every
  per-op path to be O(1).


## What full drive mode needs

1. **Complete exFAT semantics.** In-place writes and appends to committed files (write into
   allocated clusters, extend with new clusters, update DataLength/ValidDataLength), truncate,
   delete (free clusters: bitmap first, then entries, the reverse of add's order), rename and
   move (entry-set rewrite inside or across directories), directory removal, timestamps,
   attributes (read-only, hidden), volume label. Each needs its own crash order. This is most
   of a filesystem driver, and the hard part is mixing it with batched commits. Suggested
   split: new data (most bytes) keeps the staged one-pass path, while metadata changes to
   existing objects are journaled in the staging area and applied in the same commit.
2. **Staged data must be crash-safe.** A power cut or a crash of the daemon loses everything
   staged in RAM, though `cp` already returned success. That is the same contract as the
   kernel page cache (dirty data is lost too), but the window is longer: idle 1.5 s plus the
   commit, against the kernel's ~5–30 s writeback that *also* loses data. Needed: stage on
   disk (fsynced) for anything an app fsyncs, honor `sync`/`syncfs` (FUSE_SYNCFS), and a
   durable intent log so a restarted daemon can finish or drop a batch.
3. **Auto-mount integration.** udisks2 has no FUSE backend: a udev rule (`ENV{UDISKS_IGNORE}=1`
   for exFAT USB partitions) plus a systemd user service that claims new devices and runs
   `flashcp mount` at `/media/$USER/LABEL`, so file managers still see a removable drive.
   Unprivileged access to the block device is needed (the uaccess ACL already gives it to the
   seat user; the spike mounted both test drives that way, without root).
   A fallback to the kernel driver for anything flashcp refuses (dirty volume, TexFAT, a
   damaged volume).
4. **Eject guard / inhibitor.** "Eject" in Nemo must wait for the final commit:
   handle `fusermount -u` / udisks power-off by committing *before* the unmount completes,
   e.g. a FUSE_DESTROY on a `fuseblk` mount (needs root for blkdev) or a D-Bus front end that
   the file manager's eject goes through; while staged data exists, take a systemd
   inhibitor lock (shutdown/sleep) and show the state in the tray.
5. **Tray status.** "N files waiting / writing to SANDISK / safe to remove", because
   `cp` returns before the data is on the drive (that's the point).
6. **Multiple writers.** The commit must not wait forever for "no open writers": commit the
   closed files and leave open ones staged (the engine supports partial batches; the overlay
   needs per-file states). Readers during a commit block today (one global lock).
7. **mmap**: shared writable mappings of staged files work via write requests;
   for committed files they need (1). `FOPEN_KEEP_CACHE` for committed files.
8. **Large files.** Staging a 4 GB video in RAM/disk doubles the local I/O and needs that much
   local space; the commit then re-reads it. For big sequential writes, writing straight to
   pre-allocated clusters (no staging) is better; staging only wins for many small files.
9. **FUSE overhead.** Every file costs 4–5 round trips to the daemon (~20 µs of the per-file
   cost on this machine, see measurements). FUSE-over-io_uring (Linux 6.14+) is the path to
   cut it, but fuser does not support it yet.

## Risks

- **Data-loss perception**: users pull the stick when `cp` returns. With the kernel driver,
  Nemo also returns early, but the desktop's "safe to remove" knows about it; drive mode must
  hook into that or it will be worse than today.
- **Scope**: a correct read-write exFAT driver in userspace, with crash ordering for every
  operation, is a large project and a long tail of compatibility bugs (Windows/macOS
  interop, odd volumes). The fast path is only new-file batches.
- **Daemon lifetime**: an OOM-kill or crash of a user service loses staged data silently.
- **FUSE latency** caps small-file throughput when the drive is fast (see the image numbers):
  on a fast UAS SSD the gain is smaller than `flashcp build`'s.

## Recommendation

**Conditional GO, as an opt-in per drive, not (yet) as a replacement for the kernel driver.**

The mechanism works and is safe: copies through the FUSE mount landed byte-for-byte, fsck was
clean every time, and a SIGKILL mid-commit never damaged pre-existing files. The gains are
real but moderate for ordinary trees (durable 1.27× on a fast SSD, 1.6–1.9× on a USB 2 stick)
and very large for big folders (27–43×, where the kernel driver is quadratic). The costs:
`cp` *returns* ~0.3 s later per 20k files (FUSE round trips), reads need `pull`'s tuning to
beat the kernel on BOT sticks, and on a fast local disk the kernel driver wins outright.

The deciding factor is scope, not speed: "every copy from any app" needs a complete,
crash-ordered, read-write exFAT implementation in userspace (item 1 above) plus the eject
guard and tray status (items 4–5), without which drive mode is *less* safe than today
because `cp` returns before anything is on the drive. Suggested path:

1. Ship drive mode as an opt-in "fast drive" toggle per drive (tray/GUI), using the spike's
   architecture: new-file batches via add's engine, physical-order reads via pull's strategy.
2. Before that ships: modify/delete/rename of committed objects with their own crash order
   (slow path is fine; correctness first), an eject guard that commits before the unmount
   completes, an inhibitor while data is staged, fsync/syncfs honored, and an fsck-based
   fallback to the kernel driver for anything flashcp refuses.
3. Make it the default only after it has survived a fuzzing/compatibility campaign (Windows
   and macOS reading the result, power-cut tests on real sticks).

If the owner does not want to own a userspace filesystem driver, the NO-GO alternative is
to keep flashcp as an explicit tool (`add` / `pull` behind the existing "Fast copy to/from
USB" file-manager entries), which already gets the sequential-write gain without the risk.

## Appendix: how the numbers were taken

`ibench.sh img|sdb|sda <dataset> <rounds>` below (CSV on stdout; medians computed from it).
Images are fresh 1.5 GB files on the local disk, kernel-mounted with `udisksctl loop-setup`
(automounted). Reads copy the tree to /dev/shm from a fresh mount after dropping the
image/device page cache (`dd iflag=nocache count=0`). The 20k-empty-files test used the same
mounts with `cp -r` of a directory of 20,000 empty files.

<details><summary>ibench.sh</summary>

```bash
#!/bin/bash
# Interleaved benchmark: kernel exfat vs flashcp mount (FUSE), same load for both.
# usage: ibench.sh img|sdb|sda SRC_DIR ROUNDS  > results.csv   (progress on stderr)
# Rounds alternate the order (ABBA): odd rounds kernel first, even rounds FUSE first.
# CSV: round,method,metric,secs   metrics: cp (perceived), durable, idle_durable, read
# img: fresh 1.5 GB image on the local disk; sdb/sda: the partition is WIPED each step.
set -u
export LC_ALL=C
B=/home/zuhu/.t3/worktrees/flashcp/spike-drive-mode/target/release/flashcp
KIND=$1; SRC=$(realpath "$2"); ROUNDS=${3:-5}; NAME=$(basename "$SRC")
W=$(dirname "$(realpath "$0")")/w; MNT=$W/mnt; mkdir -p "$MNT"
log() { echo "$*" >&2; }
now() { date +%s.%N; }
d() { echo "$1 - $2" | bc -l | xargs printf "%.3f"; }
emit() { echo "$ROUND,$1,$2,$3"; log "round $ROUND  $1 $2 $3"; }

case "$KIND" in
  img) IMG=$W/b.img ;;
  sdb) WANT="Cruzer Blade"; LABEL=SANDISK; DISK=sdb ;;
  sda) WANT="DATAMAG"; LABEL="MSI DATAMAG"; DISK=sda ;;
  *) log "refusing: img | sdb | sda only"; exit 1 ;;
esac
check_model() {
  [ "$KIND" = img ] && return 0
  local m; m=$(cat /sys/block/$DISK/device/model)
  [[ "$m" == *"$WANT"* ]] || { log "MODEL MISMATCH on $DISK: '$m' (want $WANT), refusing"; exit 1; }
}
if [ "$KIND" != img ]; then DEV=/dev/${DISK}1; check_model; log "$DISK model: $(cat /sys/block/$DISK/device/model)"; fi

# ── backend helpers ──
umount_k() {
  if [ "$KIND" = img ]; then
    [ -n "${LOOP:-}" ] || return 0
    for i in $(seq 50); do findmnt -n "$LOOP" >/dev/null || break; udisksctl unmount -b "$LOOP" --no-user-interaction >/dev/null 2>&1 && break; sleep 0.2; done
    for i in $(seq 50); do losetup -l | grep -q "$IMG" || { LOOP=; return 0; }; sleep 0.1; done
    log "loop device still attached"; exit 1
  else
    for i in $(seq 50); do findmnt -n "$DEV" >/dev/null || return 0; udisksctl unmount -b "$DEV" --no-user-interaction >/dev/null 2>&1; sleep 0.2; done
    log "cannot unmount $DEV"; exit 1
  fi
}
fresh() {
  check_model; umount_k
  if [ "$KIND" = img ]; then rm -f "$IMG"; truncate -s 1500M "$IMG"; mkfs.exfat -L BENCH "$IMG" >/dev/null; sync -d "$IMG"
  else mkfs.exfat -L "$LABEL" "$DEV" >/dev/null; sync; sleep 1; umount_k; fi   # automounter may grab it
}
mount_k() {
  if [ "$KIND" = img ]; then
    LOOP=$(udisksctl loop-setup -f "$IMG" --no-user-interaction | grep -o '/dev/loop[0-9]*')
    for i in $(seq 100); do M=$(findmnt -n -o TARGET "$LOOP") && [ -n "$M" ] && return 0; sleep 0.05; done
    udisksctl mount -b "$LOOP" --no-user-interaction >/dev/null; M=$(findmnt -n -o TARGET "$LOOP")
  else
    findmnt -n "$DEV" >/dev/null || udisksctl mount -b "$DEV" --no-user-interaction >/dev/null
    M=$(findmnt -n -o TARGET "$DEV")
  fi
  [ -n "$M" ] || { log "kernel mount failed"; exit 1; }
}
durable_k() { if [ "$KIND" = img ]; then umount_k; sync -d "$IMG"; else sync -f "$M"; umount_k; fi; }
target() { if [ "$KIND" = img ]; then echo "$IMG"; else echo "$DEV"; fi; }
drop() { dd if="$(target)" iflag=nocache count=0 status=none; }
mount_f() {
  umount_k
  "$B" mount "$(target)" "$MNT" 2> "$W/m.log" & P=$!
  until mountpoint -q "$MNT"; do sleep 0.02; kill -0 $P 2>/dev/null || { cat "$W/m.log" >&2; exit 1; }; done
}
umount_f() { fusermount3 -u "$MNT"; wait $P; }
cleanup() { mountpoint -q "$MNT" && fusermount3 -u "$MNT"; umount_k 2>/dev/null; rm -rf /dev/shm/fcp-out; }
trap cleanup EXIT
out() { rm -rf /dev/shm/fcp-out; mkdir /dev/shm/fcp-out; }
verify() { diff -rq "$SRC" "/dev/shm/fcp-out/$NAME" >/dev/null || log "!! READ DIFF ($1)"; }

# ── the measured steps ──
kernel_round() {
  fresh; mount_k
  t0=$(now); cp -r "$SRC" "$M/"; t1=$(now); durable_k; t2=$(now)
  emit kernel cp "$(d $t1 $t0)"; emit kernel durable "$(d $t2 $t0)"
  drop; mount_k; out
  t0=$(now); cp -r "$M/$NAME" /dev/shm/fcp-out/; t1=$(now); emit kernel read "$(d $t1 $t0)"; verify kernel
  umount_k
}
fuse_round() {
  fresh; mount_f
  t0=$(now); cp -r "$SRC" "$MNT/"; t1=$(now); umount_f; t2=$(now)
  emit fuse cp "$(d $t1 $t0)"; emit fuse durable "$(d $t2 $t0)"
  log "   $(grep 'commit #' "$W/m.log" | tail -1)"
  fsck.exfat -n "$(target)" 2>&1 | tail -1 | grep -q clean || log "!! FSCK NOT CLEAN"
  drop; mount_f; out
  t0=$(now); cp -r "$MNT/$NAME" /dev/shm/fcp-out/; t1=$(now); emit fuse read "$(d $t1 $t0)"; verify fuse
  umount_f
  # idle-commit path: until the daemon has committed on its own (no unmount)
  fresh; mount_f
  t0=$(now); cp -r "$SRC" "$MNT/"; t1=$(now)
  until grep -q "commit #1\|failed" "$W/m.log"; do sleep 0.01; done; t2=$(now)
  emit fuse idle_durable "$(d $t2 $t0)"
  umount_f
}

echo "round,method,metric,secs"
log "start $(date +%T)  load $(cut -d' ' -f1-3 /proc/loadavg)"
for ROUND in $(seq "$ROUNDS"); do
  if [ $((ROUND % 2)) = 1 ]; then kernel_round; fuse_round; else fuse_round; kernel_round; fi
done
log "end $(date +%T)  load $(cut -d' ' -f1-3 /proc/loadavg)"
if [ "$KIND" != img ]; then
  # restore: empty exFAT with the original label, mounted via udisks
  trap - EXIT; fresh; udisksctl mount -b "$DEV" --no-user-interaction >&2
fi
```

</details>
