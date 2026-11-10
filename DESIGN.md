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
  time), chmod as the read-only attribute, statfs, fsync, fsyncdir, `sync`.
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
- Not supported: symlinks, hard links, xattrs, ownership (EPERM/ENOTSUP), TexFAT (two FATs),
  volumes over 2^32 clusters. These, a dirty volume and a failed validation go to the kernel
  driver (`--check` exit 3).

## Commits

Writes are staged, and nothing on the volume changes until a commit. A commit is triggered by:
1 s without writes (idle), half the write window staged (pressure, so writers keep going while
the first half is written), fsync/fsyncdir/flush, `prepare-eject`, unmount, SIGTERM. Writers
block once the window is full, 256 MB by default (`--max-dirty-mb`). That is also the most a
yank can lose.

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
- Freed clusters are reusable only by the next commit, after this one is durable.
- Every gap between data writes is decided by `add`'s gap policy: zero-fill or skip, from the
  device's measured cost per request. A gap may be filled only where it is our own slack or
  clusters the pre-commit bitmap calls free. The data goes through `add`'s O_DIRECT sequential
  writer on a separate file description, so the metadata handle stays buffered.

Entry sets:

- An edit whose changed bytes all lie in one 512-byte sector is written in place. Otherwise
  the set is relocated: a new set is written (vendor secondaries and creation time kept), and
  every entry of the old one is marked unused. A set goes into the first run of unused entries
  that fits (slots freed by the same commit excluded), else at the end of the directory, which
  grows by whole clusters.
- A move writes the set in the new directory and clears it in the old one. A replaced target
  is cleared in the same phase.

### Phases and the journal

| step | writes | sync |
|---|---|---|
| data | file data, new directories' content, directory extensions (zeros): free clusters, plus in-place slack past a committed ValidDataLength | yes |
| journal | body + header ACTIVE into `/.flashcp-drive-journal` | yes |
| `dirty` | VolumeDirty | yes |
| `alloc` | bitmap bits of new clusters; FAT entries no committed chain uses | yes |
| `link` | entry sets (new, edited, relocated, cleared); FAT entries of committed chains; directory lengths | yes |
| `free` | bitmap bits of freed clusters | yes (if any) |
| `clean` | VolumeDirty cleared, PercentInUse | yes |
| idle | header IDLE (one sector) | yes |

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

### Crash guarantees

Whatever happens (SIGKILL, power loss, a yank at any write or sync), the next mount shows
either the tree before the interrupted commit or the tree after it. The comparison is the
whole tree, byte for byte, and the volume is `fsck.exfat`-clean:

| crash during | on the volume | next mount |
|---|---|---|
| data | old tree; garbage in free clusters | nothing to do (journal IDLE) |
| journal | old tree; header IDLE, or ACTIVE with a body that fails its hash | treated as IDLE: old tree |
| dirty … clean | a mix of old and new metadata; dirty | check and replay: new tree |
| idle | new tree, header still ACTIVE | replay (every sector is already in its final state): new tree |

Recovery runs before the volume is parsed: the journal's own entry set is located by a raw
scan of the root, because a torn set elsewhere would fail the full parse. It then checks
three things:
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
  next directory.
- **Big files.** A sequential reader keeps 16 MB queued ahead of it.

The queued extents are coalesced with `pull`'s read planner, using the gap from the device's
request overhead × bandwidth on USB BOT and the transport's defaults on UAS. `inflight` worker
threads read them with O_DIRECT, on a separate file description, into a 64 MB cache. A chunk
is dropped once every byte of it was served. A chunk a reader needs is waited for while a
worker reads it; one no worker has started within 20 ms is read directly, so a reader in
another order never stalls. Every chunk overlapping a range a commit writes is invalidated at
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
  - readahead under commits that reuse clusters;
  - interop with `add`, `sync`, `pull` and `info`.
- **Tier 2** (`tests/drive_fuse.rs`, the CI job `fuse`): real FUSE mounts of image files
  exercised with `cp`, `mv`, `rm`, `rsync`, `dd`, the control socket, SIGTERM, a lazy unmount
  while dirty, and `--check` exit codes.
- **Tier 3**: `flashcp sim` on traces of the driver's own reads and commits (the ignored
  `ra_bench_trace` / `wr_bench_trace` harnesses), then hardware once for the final numbers
  (the PR has the tables).

## Known limits

- All inodes of a loaded directory stay in memory (about 200 bytes each): fine for
  hundreds of thousands of files, not for tens of millions.
- Directories never shrink (unused entries are reused, not compacted).
- The per-file FUSE round trips stay (FUSE_PASSTHROUGH does not apply to a raw device).
