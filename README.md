<div align="center">

# flashcp

**Copy thousands of small files to and from a USB drive as normal loose files.
Measured up to 10.3× faster writes and up to 26.8× faster reads than `mount` + `cp` on a USB SSD
under Linux, and up to 2.5× on a cheap USB 2.0 stick.** Results vary by drive; see the [benchmarks](#benchmarks).

[![CI](https://github.com/zuhuHix/flashcp/actions/workflows/ci.yml/badge.svg)](https://github.com/zuhuHix/flashcp/actions/workflows/ci.yml)
[![Windows + macOS checks](https://github.com/zuhuHix/flashcp/actions/workflows/xos.yml/badge.svg)](https://github.com/zuhuHix/flashcp/actions/workflows/xos.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Interactive site](https://img.shields.io/badge/site-benchmark%20explorer-0f9d8a.svg)](https://zuhuhix.github.io/flashcp/)
[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20me%20a%20coffee-support-FFDD00?logo=buymeacoffee&logoColor=000)](https://buymeacoffee.com/zuhu)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/copy-vs-build-dark.gif">
  <img alt="Two drives drawn as grids of blocks. The normal copy's write head jumps back and forth between the metadata area and the data area for every file and takes 2.80 s. flashcp sweeps once from left to right and is done in 0.42 s, 6.7 times faster." src="assets/copy-vs-build-light.gif" width="800">
</picture>

**[▶ Try the interactive benchmark explorer](https://zuhuhix.github.io/flashcp/)**

</div>

Copying a `node_modules`-sized tree to a USB drive is slow even on a fast drive, and the drive's
bandwidth isn't the reason. Every file costs a directory entry, an allocation-table update and a
bitmap update, all tiny writes scattered across the disk, and every one is a separate trip over USB.
flashcp reads and writes exFAT itself, so it can turn all of that into **one sequential pass**:

- **`flashcp build`** writes a whole new exFAT drive in one stream. **It erases the drive.**
- **`flashcp add`** adds a folder to a drive that already has files, without erasing anything.
- **`flashcp pull`** copies files off a drive, reading it front to back in physical order.

What you get is an ordinary exFAT drive with ordinary files on it. Windows, macOS and Linux read
and write it like any other (checked in CI, see [Compatibility](#compatibility)). flashcp itself
runs on Linux. There's a command-line tool and a [desktop tray app](#desktop-app).

## Contents

[Install](#install) · [Desktop app](#desktop-app) · [Quick start](#quick-start) ·
[Benchmarks](#benchmarks) · [Compatibility](#compatibility) · [How it works](#how-it-works) ·
[The journey](#the-journey) · [Command reference](#command-reference) · [Honest limits](#honest-limits) ·
[Roadmap](#roadmap) · [Support](#support) · [Contributing](#contributing)

## Install

flashcp runs on Linux (x86_64 and aarch64). The release binaries are static, so they work on any distribution.
`fsck.exfat` from `exfatprogs` is recommended for checking volumes.

**Prebuilt binary**

```sh
VERSION=0.1.0
ARCH=$(uname -m)   # x86_64 or aarch64
curl -LO "https://github.com/zuhuHix/flashcp/releases/download/v$VERSION/flashcp-$VERSION-$ARCH-unknown-linux-musl.tar.gz"
tar -xzf "flashcp-$VERSION-$ARCH-unknown-linux-musl.tar.gz"
sudo install -m 755 "flashcp-$VERSION-$ARCH-unknown-linux-musl/flashcp" /usr/local/bin/
```

Checksums are in `SHA256SUMS` on the [release page](https://github.com/zuhuHix/flashcp/releases)
(`sha256sum -c --ignore-missing SHA256SUMS`).

**Debian / Ubuntu (.deb)**

```sh
VERSION=0.1.0
ARCH=$(dpkg --print-architecture)   # amd64 or arm64
curl -LO "https://github.com/zuhuHix/flashcp/releases/download/v$VERSION/flashcp_${VERSION}_$ARCH.deb"
sudo apt install "./flashcp_${VERSION}_$ARCH.deb"
```

This installs `/usr/bin/flashcp`.

> [!NOTE]
> If `mtd-utils` is installed, `flashcp` may run its unrelated MTD flash tool (`/usr/sbin/flashcp`,
> which comes first in PATH) instead of this one. Check with `command -v flashcp`, or `flashcp --help`:
> ours starts with "Fast file transfer to USB sticks and external drives". To be sure, call `/usr/bin/flashcp`.

**From source** (current stable Rust)

```sh
cargo install --locked --git https://github.com/zuhuHix/flashcp
```

## Desktop app

On Linux Mint, Ubuntu and Debian you can install a small tray app alongside the CLI. Build its
`.deb` with the steps in [packaging/desktop/README.md](packaging/desktop/README.md), then install both packages together:

```sh
sudo apt install ./flashcp_*.deb ./flashcp-gui_*.deb
```

It starts at login and shows a notification when you plug in a USB exFAT drive. It has two modes:

- **Copy to USB.** Pick files or drop them on the window, and flashcp adds them to the drive in one
  sequential pass. The app unmounts the drive, writes, remounts it and opens it in your file manager.
- **Copy from USB.** Pick the drive and, if you like, a folder on it (the whole drive by default), and
  where to copy to (default `~/Downloads/<drive label>-<date>`). If that folder already has files,
  you choose **Skip existing**, **Overwrite** or **Cancel**. The drive is opened read-only, and the
  destination is written as you, never as root.

It also adds **Fast copy to USB (flashcp)** and **Fast copy from USB (flashcp)** to the right-click
menu in Nemo, Nautilus (needs `python3-nautilus`) and Dolphin. Nautilus shows the "from USB" entry
only on USB exFAT drives; Nemo and Dolphin show it on any folder, and the app checks. Normal
drag-and-drop copies in the file manager are not changed; the fast path is always an explicit choice.

The first time you use a drive, the app asks you to trust it (admin password; it shows the model,
size and serial). After that, copying needs admin rights once per session. A small helper only touches
trusted, unmounted USB drives that don't carry the system, and it runs flashcp as you, not as root.
See [packaging/desktop/SECURITY-NOTES.md](packaging/desktop/SECURITY-NOTES.md).

Drives that aren't exFAT can be erased and rebuilt from the app's *Advanced* section, after you
type the drive's label (or its serial number if it has no exFAT label) to confirm. Nothing is erased automatically.

## Quick start

Find the drive with `lsblk`. flashcp works on an **unmounted** partition (`/dev/sdX1`) or an image
file; reading or writing a block device needs root (or the desktop app's helper).

**Fill a fresh drive: `build`**

> [!WARNING]
> `flashcp build` **erases the whole target** and replaces it with a new exFAT volume.
> Double-check the device path before running it. It refuses to run without `--yes-erase`.

```sh
flashcp build ./my-files /dev/sdX --dry-run              # see the plan, write nothing
sudo flashcp build ./my-files /dev/sdX --yes-erase       # ERASES /dev/sdX

truncate -s 8G stick.img                                 # or build into an image file
flashcp build ./my-files stick.img --yes-erase
```

**Add to a drive that already has files: `add`**

```sh
sudo flashcp add ~/Photos/2026 /dev/sdX1 --to "Photos/2026"   # unmount the drive first
flashcp add ./more-files drive.img --skip-existing             # images work too
```

- `--to DIR` sets the folder inside the volume (missing folders are created). Existing folders are merged.
- A file whose name already exists (exFAT names are case-insensitive) is an error unless
  `--skip-existing` is given, in which case it is skipped. Nothing is written when `add` refuses.
- `add` refuses volumes it does not fully understand: TexFAT/2 FATs, bad checksums, a dirty volume
  (run `fsck.exfat` first), mounted devices, and images attached to a loop device.
- It writes data first, then the allocation bitmap and FAT, then directory entries, with a sync between
  phases and the volume marked dirty until the end. A crash can at worst leave unreferenced clusters;
  it cannot leave entries pointing at unwritten data.
- When free space is fragmented, files are split across the biggest free extents with normal FAT chains.

**Copy files off a drive: `pull`**

```sh
sudo flashcp pull /dev/sdX1 ./backup                     # everything (the drive is only read)
flashcp pull stick.img ./photos --from "DCIM/2026"       # one folder; its contents land in ./photos
sudo flashcp pull /dev/sdX1 ./backup --skip-existing     # keep files already in ./backup
sudo flashcp pull /dev/sdX1 ./backup --overwrite         # replace them (temp file + rename)
```

`pull` treats the drive as untrusted: loops, cross-links, directory cycles and absurd sizes are
refused before anything is written, and it never writes outside the destination or follows a symlink
there. Files keep their modification times, and empty files and folders are created. Pulling from a
**mounted** drive works when nothing is writing to it, but files written since the last `sync` may be
missing or partial, so flashcp warns about a read-write mount; unmount it for a guaranteed-consistent copy.

**Look before you leap: `info`**

```sh
flashcp info /dev/sdX1 --json
{"label":"MSI DATAMAG","cluster":131072,"size":1000156102656,"free":999850000000,"files":28404,"dirs":309,"dirty":false}
```

Read-only; works on mounted drives too. `dirs` excludes the root.

<!-- PR9: "Optimize an existing stick" (flashcp optimize) and "Automatic tuning" subsections go here once PR #9 merges. -->

## Benchmarks

Two very different drives, both on Linux 7.0, same day (2026-11-09). Datasets come from `flashcp gen`:
**small** = 20k files / 169 MB, **many** = 100k files / 413 MB (`node_modules`-like),
**mixed** = 8.4k files / 1.4 GB. Every baseline includes its `mkfs` or its mount and unmount, plus
`cp -r` and `sync`. Every flashcp time is the whole process, including formatting for `build`.
Volumes were checked with `fsck.exfat` and a byte-for-byte `diff -r`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/benchmark-dark.gif">
  <img alt="Two bar charts. USB SSD, 20k small files: build 2.80 s vs 0.42 s (6.7×), add 1.72 s vs 0.48 s (3.6×), pull 6.01 s vs 0.66 s (9.1×). Cheap USB 2.0 stick, 20k small files: build 86.3 s vs 34.6 s (2.5×), pull from a kernel-filled stick 30.86 s vs 29.52 s (1.05×, on par), pull from a flashcp-built stick 32.78 s vs 16.38 s (2.0×)." src="assets/benchmark-light.gif" width="800">
</picture>

### Fast drive: MSI DATAMAG 20Gbps USB SSD (UAS, 10 Gb/s link, 1 TB)

Medians of 3.

| operation | dataset | normal copy | **flashcp** | speed-up |
|---|---|---:|---:|---:|
| `build` vs `mkfs.exfat` + mount + `cp -r` + sync | small | 2.80 s | **0.42 s** | 6.7× |
| | many | 8.90 s | **0.86 s** | 10.3× |
| | mixed | 3.71 s | **1.72 s** | 2.2× |
| `add` onto a drive holding 1.4 GB, vs mount + `cp -r` + sync + unmount | small | 1.72 s | **0.48 s** | 3.6× |
| | many | 9.01 s | **1.75 s** | 5.1× |
| | mixed | 3.16 s | **1.57 s** | 2.0× |
| `pull` from a kernel-filled drive, vs mount + `cp -r` + sync + unmount | small | 6.01 s | **0.66 s** | 9.1× |
| | many | 27.02 s | **2.27 s** | 11.9× |
| | mixed | 5.39 s | **1.82 s** | 3.0× |
| `pull` from a flashcp-built drive | small | 5.58 s | **0.38 s** | 14.6× |
| | many | 31.39 s | **1.17 s** | 26.8× |
| | mixed | 5.44 s | **1.68 s** | 3.2× |

The more files you have and the smaller they are, the bigger the win. With big files the drive's
raw bandwidth is the limit for both methods, so the gap shrinks.

<details>
<summary><b><code>build</code> variants: smart copy and <code>--fat full</code></b></summary>

| dataset | mkfs + cp | mkfs + smart copy | **flashcp build** | build (`--fat full`) |
|---|---:|---:|---:|---:|
| 20k files / 169 MB | 2.80 s | 2.85 s | **0.42 s (6.7×)** | 0.73 s |
| 100k files / 413 MB | 8.90 s | 8.80 s | **0.86 s (10.3×)** | 1.83 s |
| 8.4k mixed files / 1.4 GB | 3.71 s | 3.50 s | **1.72 s (2.2×)** | 2.21 s |

</details>

### Cheap drive: SanDisk Cruzer Blade 8 GB (USB 2.0, usb-storage / BOT)

The small dataset (20k files / 169 MB). Baselines include mount and unmount.

| operation | normal copy | **flashcp** | speed-up |
|---|---:|---:|---:|
| `build` vs `mkfs.exfat` + `cp` | 86.3 s | **34.6 s** | 2.5× |
| `pull` from a kernel-filled stick | 30.86 s | **29.52 s** | 1.05× (on par) |
| `pull` from a flashcp-built stick | 32.78 s | **16.38 s** | 2.0× |

`add` on the stick is being re-tuned (in review); its numbers will be published with that change.

<!-- PR9: on merge, add this row to the table above (numbers are PROVISIONAL, from PR #9; confirm or replace):
| `add` 20k more files onto a stick already holding 20k (mkfs-default 32 KB clusters) | 209.8 s | **84.1 s** | 2.5× |
| same, on a stick after `flashcp optimize` (4 KB clusters) | 209.8 s | **36.5 s** | 5.7× |
plus optimize rows if wanted: pull after optimize 28.9 → 16.0 s (1.8×); optimize itself 154.8 s. -->

### Two kinds of drives

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/two-drives-dark.gif">
  <img alt="Schematic of a cheap USB 2.0 stick and a fast USB SSD. On the stick, the flash chip is a narrow neck (measured 6.5 MB/s sequential write); flashcp removes most of the per-file overhead but the chip time stays, so it is 2.5 times faster (86.3 s to 34.6 s). On the SSD, the overhead is most of the time, so flashcp is 6.7 times faster (2.80 s to 0.42 s)." src="assets/two-drives-light.gif" width="800">
</picture>

The gains differ because the bottleneck differs. The stick measured 6.5 MB/s for sequential 8 MB
writes (6.2 MB/s at 64 KB, 2.5 MB/s at 4 KB, 48 random 4 KB IOPS) and about 11 MB/s for sequential
reads; a model fit puts its write ceiling at an estimated ≈6.9 MB/s. No software gets past that. flashcp removes the per-file
overhead, so copies get up to 2.5× faster, but the chip is the ceiling. On a fast SSD the per-file
overhead is most of the time, so removing it gives up to 10.3× on writes and 26.8× on reads with
many small files (2.0–3.2× on the mixed 1.4 GB dataset, where bandwidth is the limit again).

Pulling from a stick that the kernel filled is on par with `cp`: both are bound by the stick's read
speed, and every scattered file needs its own read either way. A stick written by `flashcp build`
has every file contiguous, and `pull` is 2× faster there.

Cheap sticks are also noisy: the same `mkfs` + `cp` took anywhere from 110 to 180 s across runs as
the stick's controller did its own housekeeping. Treat single numbers on cheap sticks as rough.

## Compatibility

flashcp volumes are checked on every push by `.github/workflows/xos.yml` (details in [TESTING.md](TESTING.md)):
the volume is attached on GitHub's Windows and macOS runners, checked with the OS's own repair
tool in read-only mode, every file is compared against a SHA-256 manifest, then the OS writes to
the volume and it is checked again.

| | Linux (exfatprogs, kernel driver) | Windows (`chkdsk`) | macOS (`fsck_exfat`) |
|---|---|---|---|
| `--fat prefix` (default) | ✅ | ✅ | ✅ |
| `--fat full` | ✅ | ✅ | ✅ |
| 4 KiB and 128 KiB clusters | ✅ | ✅ | ✅ |
| 1 GiB volume | ✅ | ✅ | ✅ |
| No partition table (superfloppy, flashcp's default) | ✅ | ✅ | ✅ |
| File contents, sizes, mtimes (1980–2100, non-UTC clock) | ✅ | ✅ | ✅ |
| OS writes to the volume afterwards, still clean | ✅ | ✅ | ✅ |

<!-- PR9: add a row for 512 B, 1 KiB and 2 KiB clusters (all ✅ in PR #9's xos run) once it merges. -->

Tested on `windows-latest` and `macos-latest` GitHub runners with attached disk images, not yet
on physical USB drives. macOS lists accented exFAT names in NFD (decomposed) form; that is the
macOS driver, and it does the same on volumes made by `mkfs.exfat`.

## How it works

### `build`: one pass for the whole drive

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/volume-layout-dark.gif">
  <img alt="An exFAT volume drawn as one bar: boot region, FAT, allocation bitmap, up-case table, directories, file data. A single write head fills it from left to right." src="assets/volume-layout-light.gif" width="800">
</picture>

1. **Plan.** flashcp walks the source tree and lays out the whole volume in memory: boot region,
   FAT, allocation bitmap, up-case table, every directory (grouped together in one region), and
   every file's clusters. Each file is contiguous, so it uses exFAT's NoFatChain flag, the same as
   a file written by a well-behaved driver. The cluster size is picked to minimise bytes written
   (FAT + bitmap + per-file slack). <!-- PR9: replace the last sentence with the time-based automatic cluster policy (512 B–2 KB on small, full volumes of tiny files). -->
2. **Stream.** It writes the volume front to back as one strictly ascending stream of 8 MB aligned
   `O_DIRECT` writes, double-buffered so reading the next chunk overlaps with the device writing
   the current one.
3. **Done.** There is no step 3. The drive mounts anywhere exFAT mounts, with normal loose files.

The drive sees one ascending run of large, aligned writes instead of many small scattered ones,
and a handful of big USB commands replace hundreds of thousands of tiny ones. On the test SSD that's
the difference between 999 MB/s (8 MB writes) and 97 MB/s (4 KB writes).

### `add`: new files into free space, then the tables

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/add-no-erase-dark.gif">
  <img alt="A drive grid holding existing files, which never change. New files are written in one left-to-right pass into the free cells only. Then the metadata updates in order: 1 data, 2 allocation bitmap and FAT, 3 folder entries." src="assets/add-no-erase-light.gif" width="800">
</picture>

`add` first validates the volume: boot region and checksum, FAT geometry, the allocation bitmap,
the up-case table, and every directory entry set it will touch. Then it places the new files in
free space (best-fitting free extent, or the biggest extents in ascending order when space is
fragmented) and writes them in one ascending pass. Only after the data is on the drive does it
write the bitmap and FAT, then link new folders into existing ones, then the directory entries,
with a sync between each phase. Existing files are never rewritten.

### `pull`: one sweep in physical order

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/pull-sweep-dark.gif">
  <img alt="Two drive grids. mount + cp reads files in name order, hopping back and forth across the drive with a round trip per file, and takes 6.01 s. flashcp pull plans first, then reads everything in one left-to-right sweep in physical order and takes 0.66 s, 9.1 times faster." src="assets/pull-sweep-light.gif" width="800">
</picture>

A normal `cp` from a mounted drive reads one file at a time, in name order, and waits for the drive
for each one. `pull` reads the exFAT structures itself (with the same validating parser `add` uses),
lists where every file's clusters are, sorts them by position, and reads the drive front to back,
handing the data to writer threads that fill in the destination files.

How it reads depends on the drive, which it detects from sysfs:

- **UAS / NVMe:** up to 32 reads in flight, reading straight through gaps of up to 32 KB between files.
- **usb-storage (BOT) sticks**, which run one command at a time: 2 reads in flight, and it reads
  through a gap only when a ~0.5 s micro-probe shows that is cheaper than issuing another request.

## The journey

I started by measuring the SSD with `flashcp probe`:

| write pattern | throughput |
|---|---:|
| sequential, 8 MB writes | 999 MB/s |
| 4 KB writes | 97 MB/s |
| small files | ~9k files/s |

**On a top-end drive, small files were the problem.** The question was where the time goes.

1. **A smarter userspace copier** (`flashcp copy`): parallel reads, ordered writes,
   preallocation, a single flush at the end. Result: **1.08–1.17×**. Barely anything. Linux's page
   cache already batches writes. The cost is the per-file work inside the kernel's exFAT driver,
   and no amount of cleverness in userspace gets around it.
2. **Pack everything into one stream** (`flashcp pack`): a tar file, with adaptive zstd that samples
   the data and only compresses when it helps. Result: **4.3–4.9×** on 20k small files. So one
   sequential stream *is* the answer, but now you have an archive, not files you can open.
3. **Write the filesystem myself** (`flashcp build`): if the driver is the bottleneck, don't use
   it. Build the entire exFAT volume in memory and stream it out. Result: **6.7× / 10.3× / 2.2×**,
   *with* loose files, including the time to format. That was the breakthrough.
4. **Stop erasing** (`flashcp add`). A tool that wipes your drive every time is a tool for one job.
   `add` parses and validates the existing volume, writes new files into free space in one pass,
   and orders its metadata writes so a crash can't corrupt what was there. Its first benchmark
   *lost* to `cp` on 100k files (13.3 s vs 9.5 s) because it zero-filled 12.8 GB of cluster slack;
   skipping the slack made it **3.6× / 5.1× / 2.0×**.
5. **Read it back** (`flashcp pull`). The same idea in reverse: sort every file's clusters by
   position and sweep the drive once. **9.1× to 26.8×** on the SSD.
6. **The cheap-stick surprise.** Then I measured a SanDisk Cruzer Blade, a typical USB 2.0 stick.
   `build` was 2.5× faster, less than on the SSD but real. The first `pull`, though, was
   **1.8× slower than `cp`** (62.9 s vs 35.2 s). It was using the SSD's tuning: 32 parallel 8 MB
   reads only interleave on a transport that runs one command at a time, and reading through gaps
   of up to 32 KB meant reading 650 MB instead of 208 MB at the stick's ~11 MB/s. The fix was
   **adaptive reads**: detect BOT, keep 2 reads in flight, and let a half-second probe decide whether
   reading through a gap is cheaper than a new request. Now `pull` is on par with `cp` on a
   kernel-filled stick and 2× faster on a flashcp-built one.
7. **The speed council.** To find out what was left, I put a council of 10 agents on the question
   of where the next speed-up comes from. Their conclusion: on the stick, the flash chip sets the
   pace, so the next big win isn't physics. It's **not re-sending data you already copied** last
   time, which is what [`sync`](#roadmap) is for.

<details>
<summary><b>Earlier experiment results: copying onto an existing volume</b></summary>

MSI DATAMAG SSD. Speedup relative to the naive copier; `pack` and `pack-auto` produce a tar archive.

| dataset | naive | cp | smart | smart-w4 | pack | pack-auto |
|---|---|---|---|---|---|---|
| 20k files / 169 MB | 1.66 s | 1.01× | 1.08× | 1.17× | 4.31× | **4.93×** |
| 8.4k files / 1.4 GB | 2.63 s | 0.97× | 0.97× | 0.66× | 0.86× | 0.99× |

- With a good UAS drive on Linux the page cache already batches writes, so reordering in userspace buys at most ~15%.
- One sequential stream (pack) is ~5× faster for small files. Getting that speed with loose files meant skipping the
  filesystem driver, which led to `flashcp build`.

Known caveat: the probe's QD8 test uses 8 threads on one `O_DIRECT` file, so it can hit the filesystem's inode lock.
It may understate how much the device gains from parallel writes.

</details>

<details>
<summary><b><code>--fat prefix</code> vs <code>--fat full</code></b></summary>

`--fat prefix` (the default) writes only the FAT entries that are in use. exFAT treats the
allocation bitmap as the authority on which clusters are free, so free FAT entries don't matter.
On large drives with small clusters that saves a lot: a 1 TB volume with 4 KB clusters has
a FAT of about 976 MB (244 million clusters × 4 bytes), and prefix mode writes only the used part.
Windows `chkdsk` and macOS `fsck_exfat` accept both modes (see [Compatibility](#compatibility)).

`--fat full` zeroes the whole FAT like `mkfs.exfat` does. A bit slower.

</details>

## Command reference

```
flashcp build <src> <device|image> --yes-erase         # whole exFAT volume in ONE sequential pass (ERASES)
flashcp add   <src> <device|image> [--to DIR] [--skip-existing]
                                                       # add to an existing exFAT volume, nothing erased
flashcp pull  <device|image> <dest> [--from DIR] [--skip-existing | --overwrite]
                                                       # copy files off a volume in physical order
flashcp info  <device|image> [--json]                  # label, size, free space, file count (read-only)
flashcp probe <mounted-dir>                            # measure the drive (temp file, no raw writes)
flashcp gen   <dir> --profile small|mixed|large|tiny   # reproducible test data
flashcp bench <src> <mounted-dir> --runs 3             # the early copy methods, timed incl. final flush
flashcp copy  <src> <dst> [--writers N] [--verify]     # pipelined, ordered loose-file copy (experiment)
flashcp pack  <src> <out.tar> --compress auto|zstd|none  # one tar stream (experiment)
```
<!-- PR9: add `flashcp optimize <device|image> --yes [--temp DIR] [--force]` above once PR #9 merges. -->

- `build`: `--dry-run` (print the plan only), `--cluster <bytes>` (default: optimizer picks),
  `--label <name>` (default `FLASHCP`), `--fat prefix|full`, `--readers <n>` (default 12).
- `add`: `--to DIR`, `--skip-existing`, `--dry-run`, `--readers <n>`.
- `build`, `add`, `pull` and `info` take `--target-fd N` instead of a path, to use an already-open
  block device (for example from a privileged helper; that's how the desktop app works).

**Machine-readable progress.** `build --json`, `add --json` and `pull --json` print JSON lines on
stdout and nothing else:

```
{"event":"plan","op":"add","files":20000,"dirs":200,"bytes":168800000,"write_bytes":250000000}
{"event":"progress","files_done":5120,"bytes_done":43000000}     (at most ~10 per second)
{"event":"done","files":20000,"bytes":168800000,"secs":0.476}
{"event":"error","message":"..."}                                 (with a non-zero exit code)
```

For `pull`, the plan event also carries the read strategy it chose
(`"strategy":{"transport","gap_kb","inflight","direct","overhead_ms","mbps"}`).
The same operations are available as a Rust library: `flashcp::build`, `flashcp::add` and
`flashcp::info`; the first two take a `&mut dyn FnMut(&flashcp::Event)` progress callback.

## Honest limits

- **flashcp runs on Linux only, for now.** The drives it makes work on Windows and macOS, but the tool
  itself doesn't run there yet (see [Roadmap](#roadmap)).
- **exFAT only.** `build` creates exFAT; `add` and `pull` read exFAT. FAT32 and NTFS drives aren't supported.
- **`build` erases the target.** Use `add` to keep what's on the drive.
- **The drive must be unmounted** for `build` and `add` (the desktop app unmounts and remounts for you).
  `pull` works on a mounted drive but warns, because it can't see writes the kernel hasn't flushed.
- **`add` is cautious.** It refuses TexFAT, two FATs, dirty or media-failure volumes, and volumes it
  doesn't fully understand. It never reuses deleted directory slots and can't remove files.
- **`pull`** restores modification times only (not attributes or creation/access times), and doesn't
  cross-check the volume against its allocation bitmap the way `add` does.
- **Cheap sticks are chip-bound.** Expect about 2–2.5× on a USB 2.0 stick, and no gain when pulling
  from a stick that another system filled file by file.
- **Measured on two drives.** One fast UAS SSD and one cheap BOT stick. Other drives will differ;
  please [send results](#contributing).
- **Cross-OS checks use disk images** on GitHub's runners, not yet physical USB drives on Windows and macOS.

## Roadmap

**In review**
- [ ] Automatic cluster size per drive and dataset, `flashcp optimize` (rewrite a stick that was
  filled file by file into the fast layout, without losing data), a smarter gap policy for `add`,
  and a per-device speed profile cache. <!-- PR9: move to "Shipped" with a link once merged. -->

**Planned**
- [ ] **`flashcp sync` / `pull --update`:** copy only what changed, so repeat copies take seconds.
- [ ] **Drive mode** *(exploring; a prototype is underway)*: a native FUSE mount, so every app's
  copies to the drive get faster automatically, not just flashcp's.
- [ ] **Write-behind and one-click jobs:** copies that finish in the background, and repeatable
  copy jobs you start with one click.
- [ ] **Windows and macOS versions** of flashcp. [Supporters](#support) get them first.

**Shipped in 0.1.0:** `build`, `add`, `pull`, `info`, JSON progress, the Linux desktop app,
Windows/macOS verification in CI, static release binaries and `.deb` packages for the CLI.

## Support

flashcp is **sponsorware**. Everything in this repository is free software under the GPL and stays
that way. If you [buy me a coffee](https://buymeacoffee.com/zuhu), you get the planned Windows and
macOS versions first, and you pay for the time that goes into the roadmap above.

<a href="https://buymeacoffee.com/zuhu"><img src="https://img.shields.io/badge/☕_Buy_me_a_coffee-zuhu-FFDD00?style=for-the-badge&logoColor=000" alt="Buy me a coffee"></a>

## Contributing

Issues and pull requests are welcome, especially:

- results from other drives (run `flashcp probe` and `flashcp bench`, or time `build`/`add`/`pull`
  against `cp`, and include the drive model, its transport from `lsblk -o NAME,TRAN,MODEL` and the OS),
- reports of flashcp drives on real Windows and macOS machines,
- bugs found by `fsck.exfat`, `chkdsk` or `fsck_exfat` on a volume flashcp wrote.

Please don't run `build` or `add` on a device you care about while testing. Use an image file
(`truncate -s 4G test.img`, `flashcp build src test.img --yes-erase`, then `fsck.exfat test.img`).
[TESTING.md](TESTING.md) describes every verification layer and how to check a real drive by hand.

## License

[GPL-3.0-or-later](LICENSE)
