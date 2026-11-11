# Drive mode: the driver core (`flashcp mount`)

Drive mode serves USB exFAT drives through flashcp's FUSE driver instead of the kernel exfat
driver, so every copy, move and delete from any program gets flashcp's batched sequential
writes and physical-order reads. This document covers the driver (`src/drive/`). The system
integration (udev/udisks takeover, the per-user daemon, fallback, eject guard, tray, the off
switch) is separate, and the contract between the two is
[`docs/drive-mode-interface.md`](docs/drive-mode-interface.md).

The spike that preceded this (draft PR #11) proved the mechanism: FUSE plus a staged tree
committed by `add`'s engine. It could only add new files. The driver here is a complete
read-write exFAT implementation with crash ordering for every operation.

## Layers

| module | role |
|---|---|
| `core.rs` | The inode tree applications see: the committed state plus staged changes, and mirrors of what the volume holds (every loaded directory's bytes, the bitmap). Every POSIX operation is in memory; the only I/O is reading the volume. |
| `stage.rs` | Staged file data: per file an overlay of non-overlapping pieces over the committed content, in RAM up to a cap, then in an unlinked spill file under `$XDG_CACHE_HOME/flashcp`. |
| `commit.rs` | Plans a commit: the difference between the tree and the volume as data writes into free clusters plus journaled metadata writes, in phases. |
| `djournal.rs` | The permanent on-volume journal `/.flashcp-drive-journal`, and recovery. |
| `fs.rs` | The thread-safe driver: the commit thread (idle, pressure, fsync, flush, eject, unmount), the bounded write window, durability waits, the data writer. |
| `readahead.rs` | Physical-order readahead of committed data with `pull`'s per-transport strategy. |
| `check.rs` | `mount --check`: the bounded, read-only takeover decision. |
| `dev.rs` | The device. Every write and sync goes through it, so tests can crash it at any op. |
| `fuse.rs`, `mount.rs` | The FUSE adapter (fuser 0.18 over `fusermount3`, no libfuse, no root) and the command: events, control socket, signals. |

## What is supported

- Create, write, overwrite, append, truncate (down and up, sparse), unlink, mkdir, rmdir,
  rename within and across directories, onto an existing target (atomically replacing it),
  `RENAME_NOREPLACE` and `RENAME_EXCHANGE`, case-only renames, utimens (mtime, atime, creation
  time), chmod as the read-only attribute, statfs, fsync, fsyncdir. **Not `sync`/`syncfs`**: see *Durability and the loss window* below.
- Unlinking an open file: its content stays readable until the last close. A commit that
  frees its clusters first copies the committed bytes into staging.
- Reads of any volume the parser accepts: FAT chains and NoFatChain runs, any cluster size from
  512 B to 32 MB, sectors of 512 to 4096 bytes, ValidDataLength < DataLength (zeros past it),
  vendor secondary entries (kept when a set is relocated).
- Case-insensitive lookups through the volume's own up-case table, case-preserving names.
  Names Windows refuses get EINVAL: `"*/:<>?\|`, control characters, a trailing dot or space,
  unpaired surrogates. More than 255 UTF-16 units gets ENAMETOOLONG.
- mmap: reads through the page cache (`FOPEN_KEEP_CACHE`, safe because flashcp is the only
  writer). Shared writable mappings work as FUSE allows (dirty pages arrive as writes).
- Permissions: the mount runs without `default_permissions`, which cost one GETATTR of the
  parent per created file (5.04 → 4.02 FUSE requests per created file). The driver makes the
  checks the kernel made against the modes it reports (0644/0755, 0444/0555 with the
  read-only attribute), and only the mounting user reaches the mount (no `allow_other`):
  - a read-only file refuses opens for writing and any truncation (O_TRUNC with any access
    mode, truncate, and ftruncate through a handle opened before the attribute was set);
  - a read-only directory refuses create, mkdir, unlink, rmdir and renames out of or into it;
  - `access(2)` refuses W_OK on read-only objects and X_OK on files;
  - chown to another owner gets EPERM.
  Executing a file is refused by the kernel itself (no file has an x bit).
- Not supported: symlinks, hard links, xattrs, ownership (EPERM/ENOTSUP), TexFAT (two FATs),
  volumes over 2^32 clusters. These, a dirty volume and a failed validation go to the kernel
  driver (`--check` exit 3).

## Commits

Writes are staged, and nothing on the volume changes until a commit. A commit is triggered by:
the last file open for writing being closed (after a 150 ms linger, `--close-ms`; every close
restarts it, so a `cp -r` of many files commits once, 150 ms after its last file), 1 s without
writes (idle), half the write window staged (pressure, so writers keep going while
the first half is written), fsync/fsyncdir/flush, `prepare-eject`, unmount, SIGTERM. Writers
block once the window is full, 256 MB by default (`--max-dirty-mb`). That is also the most a
yank can lose.

### Durability and the loss window

`sync(1)`, `sync -f` and `syncfs(2)` do **not** reach the driver. The kernel sends FUSE_SYNCFS
only to virtiofs and `fuseblk` servers; for a plain FUSE mount it returns 0 at once (unprivileged
servers could otherwise hang every `sync`). A `fuseblk` mount needs root: `fusermount3` refuses
the `blkdev` option for other users ("option blkdev is privileged"), and fuser 0.18 has no
syncfs callback either. So what bounds the loss is when commits happen, not `sync`:

- `fsync`/`fdatasync`/`fsyncdir`, the control socket's `flush` and `prepare-eject`, and SIGTERM
  wait until everything written before them is durable;
- **after the last write handle is closed** (`cp`, `rsync`, a saved document): durable within
  the 150 ms linger plus one commit;
- **a file kept open for writing**: durable within the 1 s idle after its last write plus one
  commit, or earlier once half the 256 MB window is staged;
- `dirty_bytes` reaches 0 when that commit finishes; nothing stays pending on an idle mount.

The commit after the linger is what `cp … && sync && pull the stick` relies on: the worst case
is 150 ms + one commit after `cp` returns (one small commit simulates to 0.07 s on the SanDisk
USB 2 model, 0.12 s on the Kingston, 3 ms on the MSI, see *An application that fsyncs every
file*; a big one is the device's sequential write time). Unplugging inside that window can lose what was not committed yet. That is the
same class of risk as the kernel's own write-back, but much shorter. Eject (prepare-eject)
remains the safe way. During a copy, the linger adds no commits: every close restarts it, so
it fires once, after the last file (measured: the same commits as without it, the last one
150 ms after `cp` returned instead of 1 s).

Planning runs under the core lock. It is memory work, plus FAT reads and, for unlinked open
files, the copy into staging. Execution runs without the lock, so readers, lookups and new
writes go on. Finalizing takes the lock again. Commits are serialized. A commit covers every
dirty inode, unless its journal would not fit the journal file. Then it is planned again with
half the inodes, until it fits: unlinked inodes first, so a replaced target always goes with
its replacement, then by depth, so a new directory is committed with or before its entries.
The rest follows in the next commit.

Data layout:

- **Never overwrite committed content.** A changed file is rewritten whole into fresh
  contiguous clusters when it is small (≤ 4 MB) or mostly changed. Otherwise only the changed
  clusters get new ones (copy-on-write, the unchanged ones are copied from the old clusters by
  the data step), with a FAT chain when the result is not contiguous. Bytes past a file's
  committed ValidDataLength are not part of its committed content, so its slack clusters are
  written in place. Appends continue right after the last cluster when it is free, so files
  stay NoFatChain.
- **One stream per commit.** A commit reserves its arena once: the smallest free extent that
  holds the whole batch (changed files and new directories), else the fewest large extents,
  like `add`. Every allocation without a preferred position is carved from it in order. Before
  this, each file took the smallest hole that fit it, so a volume full of holes turned a
  300-file commit into 316 device writes instead of 15. The arena is taken from the clusters
  free before the commit, so copy-on-write is unchanged.
- Freed clusters are reusable only by the next commit, after this one is durable.
- Every gap between data writes is decided by `add`'s gap policy: zero-fill or skip, from the
  device's measured cost per request. A gap may be filled only where it is our own slack or
  clusters the pre-commit bitmap calls free. The data goes through `add`'s O_DIRECT sequential
  writer on a separate file description, so the metadata handle stays buffered.
- The policy comes from `add`'s in-situ write probe when the device has no cached choice for
  this pattern. The probe writes test patterns into the very clusters the pending commit is
  about to fill: clusters free in the pre-commit bitmap, which nothing on the volume
  references. A kill or yank mid-probe leaves only garbage in free space and the committed
  volume untouched, just as a crash during the data step does. The probe runs only when skip
  and fill would plan different writes.

Entry sets:

- An edit whose changed bytes all lie in one 512-byte sector is written in place. Otherwise
  the set is relocated: a new set is written (vendor secondaries and creation time kept), and
  every entry of the old one is marked unused. A set goes into a run of unused entries that
  holds it (slots freed by the same commit excluded), else at the end of the directory, which
  grows by whole clusters. Inside one 512-byte sector wherever it can (any set of up to 16
  entries): the first run position that keeps it in one sector, else the first run that fits.
  At the end of a committed directory, a set that would cross a sector boundary starts at the
  next sector, and the entries before it are marked deleted in `alloc`. A set in one sector is
  never torn by a crash, which is what lets most commits go without VolumeDirty (below).
- A move writes the set in the new directory and clears it in the old one. A replaced target
  is cleared in the same phase.

### Phases and the journal

| step | writes | sync |
|---|---|---|
| data | file data, new directories' content, directory extensions (zeros): free clusters, plus in-place slack past a committed ValidDataLength | yes |
| journal | body + header ACTIVE into `/.flashcp-drive-journal` | yes |
| `dirty` | VolumeDirty (full commits only) | yes |
| `alloc` | bitmap bits of new clusters; FAT entries no committed chain uses; deleted entries padding a directory's end up to a sector | yes |
| `link` | entry sets (new, edited, relocated, cleared); FAT entries of committed chains; directory lengths | yes |
| `free` | bitmap bits of freed clusters | yes (if any) |
| `clean` | VolumeDirty cleared, PercentInUse (full commits only) | yes |
| idle | header IDLE (one sector); deferred, see below | yes |

A commit's data is written through the device handle (buffered, each run padded with zeros
inside its own last cluster up to 4 KB, runs that meet at a cluster boundary joined into one
write) when it is at most 1 MB, and through `add`'s O_DIRECT stream otherwise. The stream's
setup (three 8 MB aligned buffers and an I/O thread) cost 15-25 ms of host time per commit,
more than a small file's data takes to write.

Every write from `dirty` to `clean` is journaled with the bytes before and after it. All of
those "before" bytes come from the in-memory mirrors, so planning needs no reads for them.
The body also carries the volume's serial and geometry, and samples (offset, length, xxh3) of
the data step's runs (only those no later metadata write lands on).

The journal file is created once, when a writable volume without one is mounted (not in the
first commit, which would hold the lock through three syncs):
- Its IDLE header goes into free contiguous clusters (sync).
- Its bitmap bits (sync).
- Its entry set, inside one sector of the root, padded with deleted entries (sync).

A crash between those steps leaks the clusters, which nothing references (`fsck.exfat`
accepts that). After that the file is never rewritten except its content. Size:
volume / 512, clamped to 1–8 MB. `add`, `sync`, `pull`, `info` and `verify` skip it. `sync
--delete` never deletes it. `add` and `sync` refuse while it is ACTIVE.

### Light commits: no VolumeDirty when no crash state needs it

VolumeDirty tells another system (Windows, macOS, the kernel driver) that the metadata may be
half-written and must be checked. A commit needs it only if some crash state, read without
flashcp's journal, would not be a valid volume. For most commits there is none, and those
**light** commits skip `dirty` and `clean`: two writes of sector 0 and two syncs, most of what a
small commit cost after its data. The syncs between `alloc`, `link` and `free` stay, so a crash
leaves one of these states (sectors are written whole, the assumption the crash harness and
`flashcp sync`'s journal already make):

- inside or after `alloc`: bitmap bits (and FAT entries, deleted padding entries) that nothing
  references yet: leaked clusters, which `fsck.exfat` accepts and the journal's replay
  completes;
- inside `link`: any subset of its sector writes. That is a valid volume when every `link`
  write is one whole directory sector (a set in it is whole: an in-place edit lies in one
  sector by construction, its checksum included), no FAT entry of a committed chain changes
  (a chain longer or shorter than its file), new sets go into one sector at most (with two,
  the later one alone could land after an end-of-directory entry the earlier one replaces),
  and no set is removed in one sector while one is written in another (a move or a
  relocation, which could leave both or neither). A new set into unused slots, an in-place
  edit (times, size, a copy-on-write's new first cluster) and a removal (entries marked
  deleted, never end-of-directory) each stand alone;
- inside `free`: bits of clusters nothing references any more, some still set (leaked).

PercentInUse must not change and the volume flags must need no change either (ClearToZero set
takes a full commit, which clears it). The planner decides from the commit's own writes
(`commit::needs_no_dirty`). An fsync of a new file, an append, a delete, utimens and chmod
are light; a move, a rename into another sector, a commit of many new files in one directory,
a directory growing a FAT chain are full. In the fsync-every-file trace below, 18,610 of
18,738 commits are light. flashcp's own guarantees are unchanged: the journal is written and
replayed the same way, without the two phases.

### The IDLE header is deferred

A commit for anything but eject and unmount leaves its journal header ACTIVE, and the commit
thread marks it IDLE 100 ms later if no commit followed (`FsOpts::idle_header`). The next
commit's journal write replaces the header anyway, so an application that fsyncs file after
file saves one write and one sync per file. fsync returns once the last metadata phase is
synced, as before; the IDLE mark was never part of durability. prepare-eject and unmount
mark the header IDLE before they report, so an ejected stick never carries an ACTIVE journal.

Recovery treats an ACTIVE journal whose writes are all on the volume (every touched byte holds
its final state) as finished: it marks it IDLE and replays nothing, without checking the data
samples. That matters for the deferred header: the next commit may write into a sampled run's
own slack (in-place past a committed ValidDataLength) before its own journal lands, and a
crash then must not refuse the mount. It also covers a crash between `clean` and the IDLE
write, which was replayed (as a no-op) before. A yank while a stale header is ACTIVE, followed
by changes from another system and then a flashcp mount, behaves as a yank during a commit
always did: the sector check refuses the mount (exit 6). The window is at most 100 ms after
the last commit of a burst, instead of the length of each commit.

### An application that fsyncs every file

Some file managers fsync each file they copy. Every fsync is then a commit of one file, and
its fixed cost decides the copy's speed. The ignored `wr_fsync_bench_trace` harness replays
that pattern (create, 128 KB writes, fsync, close, one file after the other) from a shape
file of sizes and name lengths. Recorded for the Kingston (`FLASHCP_SIM_DEVICE`) on an empty
image of its size (124 GB, 4 KB clusters), with the shape of a real 787 MB folder (18,737
files, 17,714 under 64 KB, one of 229 MB, 2,071 directories), replayed on the Kingston model:

| | simulated | MB/s | writes / syncs per commit | host per commit |
|---|---:|---:|---:|---:|
| 0.4.0 | 5,665 s | 0.14 | 8.0 / 7.0 | 37.7 ms |
| light commits, deferred IDLE, small-commit data path | 2,480 s | 0.32 | 4.9 / 4.0 | 2.5 ms |

Where one small file's commit went in 0.4.0 (Kingston model, 0.30 s): 9 writes in 5 places
(data, journal, sector 0, bitmap, entry sets, sector 0 again, journal header), each
switching the stick's open allocation unit (the model's 30 ms per switch dominates; syncs cost
about 1 ms on this write-through stick), plus host time: 10 ms for a byte-by-byte scan of the
3.8 MB bitmap for free extents (now a 64-bit word at a time), 15-25 ms setting up the O_DIRECT
stream (now only for commits over 1 MB), 2 ms of bitmap copies (now only the changed bytes).
Now: data, journal, bitmap, entry sets (4 places, 5 writes, 4 syncs). Per small commit on the
other models: SanDisk 0.166 → 0.070 s, MSI 21 → 3 ms.

The model's absolute cost of a small random write on the Kingston is a guess (its FTL
parameters are the generic preset): the same model puts a kernel-like pattern (data, bitmap,
entry sets, one sync per file) at 1,917 s for this folder, while the real kernel driver
copied it at 4-5 MB/s (about 180 s). Ratios between strategies are what the model supports;
hardware decides the absolute numbers.

### Crash guarantees

Whatever happens (SIGKILL, power loss, a yank at any write or sync), the next mount shows
either the tree before the interrupted commit or the tree after it. The comparison is the
whole tree, byte for byte, and the volume is `fsck.exfat`-clean:

| crash during | on the volume | next mount |
|---|---|---|
| data | old tree; garbage in free clusters | nothing to do (journal IDLE) |
| journal | old tree; header IDLE, or ACTIVE with a body that fails its hash | treated as IDLE: old tree |
| dirty … clean (full commit) | a mix of old and new metadata; dirty | check and replay: new tree |
| alloc … free (light commit) | a mix of old and new metadata, a valid volume (not dirty) | check and replay: new tree |
| after the last phase, until the deferred IDLE | new tree, header still ACTIVE | finished (every touched byte final): marked IDLE, nothing replayed |

Recovery runs before the volume is parsed: the journal's own entry set is located by a raw
scan of the root, because a torn set elsewhere would fail the full parse. A journal whose
writes are all on the volume is finished (above). Otherwise recovery checks three things:
- the serial and the geometry;
- every data sample still hashes the same;
- every 512-byte sector the writes touch holds one of the states they take it through (the
  check `flashcp sync`'s journal uses).

Only then does it replay every phase with a sync after each, and mark the journal IDLE. If
anything differs, another system changed the volume since. Then nothing is written, and the
mount exits 6 ("run fsck.exfat").

Staged data that never reached a commit is lost on a crash, as with the kernel's page cache.
It is bounded by the window. `dirty_bytes` reports it, and fsync makes it durable before
returning.

## Reads

Committed data is read outside the core lock, under a reader gate that commit planning takes
exclusively. When a plan reuses freed clusters, no reader that saw the old mapping is still
reading them. A read goes to the readahead cache first:

- **Bulk copy of a directory.** When the second file of a directory is read from offset 0,
  or any first file while a bulk copy was seen in the last 2 s, the directory's remaining
  committed files (up to 2 MB each, 1 GB in all) are queued in directory order, together with
  the next sibling directory's files. That way the device never idles while `cp -r` opens the
  next directory. Inode numbers follow the on-disk order, because `cp -r` opens files in inode
  order. A directory is queued again on a later pass over it (after 3 s).
- **Heads.** When the reader left the previous file of a directory before its end (a
  thumbnailer, an indexer), files are queued 128 KB each instead of 2 MB.
- **Big files.** A sequential reader's window starts at 256 KB and doubles while it keeps
  reading, up to 16 MB.

The queued extents are coalesced with `pull`'s read planner, using the gap from the device's
request overhead × bandwidth on USB BOT and the transport's defaults on UAS. `inflight` worker
threads read them with O_DIRECT, on a separate file description, into a 64 MB cache. A chunk
is dropped once every byte of it was served. A chunk a reader needs is waited for while a
worker reads it. One no worker has started yet, the reader reads itself, at once: a reader in
another order never waits for the queue, and nothing is read twice. Readers and workers wait
on separate condition variables, and a hit wakes one worker. Before this, every hit woke all
of them and an out-of-order reader waited out 20 ms timeouts: reading 20k files back from a
tmpfs image took 32.9 s, now 1.0 s. Every chunk overlapping a range a commit writes is invalidated at
planning time, and a read in flight at that moment is discarded on completion (epoch counter).

## Safety

**Trust model of the journal.** The on-volume journal is exactly as trusted as the volume it
is on, and recovery is not a security boundary against it. Recovery only ever writes inside
the volume the journal came from. Anyone who can plant a forged journal there can just as
well write the same sectors directly. So recovery's checks exist to stop corruption from
spreading (a torn or buggy journal, another system's later changes), not to stop an attacker
who controls the volume:
- the serial and the geometry must match;
- the data samples must still hash the same;
- every touched sector must hold one of the journaled states;
- every record must be one a commit writes: VolumeFlags and PercentInUse, whole FAT entries,
  bitmap clusters, or directory clusters of the post-replay tree. Never file data, the up-case
  table, the journal itself, or the backup boot region.

Recovery's own resources are bounded before anything is allocated:
- the compressed body is at most 8 MiB, the decoded body at most 16 MiB (commits are split
  to stay under both);
- the touched bytes are at most 32 MiB.
Past either limit the mount is refused (exit 6).

Not checked: whether each directory or FAT record belongs to the files the transaction meant
to change. A link record relocates sets into slots of deleted entries, and moved sets keep
pointing at clusters committed long before, so that check would need a model of the whole
transaction.

- The device is claimed exclusively (`O_EXCL` on block devices, `flock` on images, refused
  when a partition or holder is mounted), and the mount runs as the unprivileged user.
- The parser is the validating one shared with `add` and `pull` (checksums, chain bounds, the
  heap limits, 256 MB directories, duplicate names refused).
- `--check` reads a bounded amount (boot region, the FAT pages the root, bitmap and up-case
  chains need, a root of at most 1 MB, the up-case table, the journal header): 95 KB on a
  64 MB volume. It opens nothing but its fd, sysfs and `/proc/self/mountinfo`, and never
  consults the environment, so it runs in the integration's Landlock sandbox. A seeded
  mutation corpus of hostile boot sectors, FAT heads and root directories exercises it: no
  panic, the read budget holds, and an accepted volume mounts and reads without panicking.

## Testing

- **Tier 1** (`src/drive/tests.rs`, in process, image files, no FUSE):
  - one table-driven test per operation family;
  - one model-based suite: random sequences of create, mkdir, write, truncate, unlink, rmdir,
    rename, commit and remount, against an in-memory reference, at 512 B, 4 KB and 64 KB
    clusters, with fsck and a full content check;
  - one crash harness: a three-commit scenario of new files, overwrites, copy-on-write of a
    big file's middle, truncation, cross-directory renames, a replaced target, deletes and
    rmdir. It is crashed at every device write and sync in turn, under three loss models for
    the unsynced writes (all reached the medium, none did, a random prefix per sector), at two
    cluster sizes. The remounted tree must equal the tree before or after the interrupted
    commit, with fsck;
  - one fsync-stream crash harness (`crash_fsync_stream`): five fsyncs from the commit thread
    with the journal header left ACTIVE between them (a new file, an append to the file the
    previous commit wrote, a new directory with a long-named file, a move, a delete; light and
    full commits), crashed the same way. Before recovery, the volume must be marked dirty or
    pass `fsck.exfat` (what another system sees); after it, the tree before or after the
    interrupted fsync, with fsck;
  - the deferred IDLE header: marked after the delay, at once by prepare-eject, and a mount
    while it is ACTIVE finds the commit finished;
  - readahead under commits that reuse clusters;
  - interop with `add`, `sync`, `pull` and `info`.
- **Tier 2** (`tests/drive_fuse.rs`, the CI job `fuse`): real FUSE mounts of image files
  exercised with `cp`, `mv`, `rm`, `rsync`, `dd`, the control socket, SIGTERM, a lazy unmount
  while dirty, `--check` exit codes, and 40 fsynced files followed by SIGKILL.
- **Tier 3**: `flashcp sim` on traces of the driver's own reads and commits (the ignored
  `ra_bench_trace` / `wr_bench_trace` / `wr_fsync_bench_trace` harnesses), then hardware once for the final numbers
  (the PR has the tables).

## Known limits

- All inodes of a loaded directory stay in memory (about 200 bytes each): fine for
  hundreds of thousands of files, not for tens of millions.
- Directories never shrink (unused entries are reused, not compacted).
- The per-file FUSE round trips stay (FUSE_PASSTHROUGH does not apply to a raw device).
