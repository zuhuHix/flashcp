<div align="center">

# flashcp

**Copy thousands of small files to a USB drive as normal loose files, measured up to 10.3× faster than `mkfs` + `cp` on a USB SSD under Linux.** Results vary by drive and OS; see the [benchmarks](#benchmarks).

[![CI](https://github.com/zuhuHix/flashcp/actions/workflows/ci.yml/badge.svg)](https://github.com/zuhuHix/flashcp/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Interactive site](https://img.shields.io/badge/site-benchmark%20explorer-0f9d8a.svg)](https://zuhuhix.github.io/flashcp/)
[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20me%20a%20coffee-support-FFDD00?logo=buymeacoffee&logoColor=000)](https://buymeacoffee.com/zuhuHix)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/copy-vs-build-dark.gif">
  <img alt="Two drives drawn as grids of blocks. The normal copy's write head jumps back and forth between the metadata area and the data area for every file and takes 2.80 s. flashcp sweeps once from left to right and is done in 0.42 s, 6.7 times faster." src="assets/copy-vs-build-light.gif" width="800">
</picture>

**[▶ Try the interactive benchmark explorer](https://zuhuhix.github.io/flashcp/)**

</div>

Copying a `node_modules`-sized tree to a USB drive is slow even on a fast drive. That isn't the
drive's bandwidth. Every file costs a directory entry, an allocation table update and a bitmap
update, all tiny writes scattered across the disk. `flashcp build` skips that: it plans the whole
exFAT volume in memory and writes it to the drive as **one sequential stream**. What you get is an
ordinary exFAT drive with ordinary files on it.

> [!WARNING]
> `flashcp build` **erases the whole target device** and replaces it with a new exFAT volume.
> Double-check the device path (`lsblk`) before running it. It refuses to run without `--yes-erase`.

## Quick start

```sh
# install (needs Rust; Linux only for now)
cargo install --git https://github.com/zuhuHix/flashcp

# see the plan without writing anything
flashcp build ./my-files /dev/sdX --dry-run

# build the drive: ERASES /dev/sdX (unmount it first; writing a block device needs root)
sudo flashcp build ./my-files /dev/sdX --yes-erase

# or build into an image file (create it first) and inspect it
truncate -s 8G stick.img
flashcp build ./my-files stick.img --yes-erase
```

## Benchmarks

Measured 2026-11-09 on an MSI DATAMAG 20Gbps USB SSD (UAS, 10 Gb/s link, 1 TB exFAT partition, Linux 7.0).
The baseline is `mkfs.exfat`, then mount, `cp -r` and `sync`. **flashcp's time includes formatting.**
Every build passed `fsck.exfat` and a byte-for-byte `diff -r` after a kernel mount.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/benchmark-dark.gif">
  <img alt="Bar chart. 20k files / 169 MB: mkfs + cp 2.80 s, flashcp 0.42 s (6.7×). 100k files / 413 MB: 8.90 s vs 0.86 s (10.3×). 8.4k mixed files / 1.4 GB: 3.71 s vs 1.72 s (2.2×)." src="assets/benchmark-light.gif" width="800">
</picture>

| dataset | mkfs + cp | mkfs + smart copy | **flashcp build** | build (`--fat full`) |
|---|---:|---:|---:|---:|
| 20k files / 169 MB | 2.80 s | 2.85 s | **0.42 s (6.7×)** | 0.73 s |
| 100k files / 413 MB (`node_modules`-like) | 8.90 s | 8.80 s | **0.86 s (10.3×)** | 1.83 s |
| 8.4k mixed files / 1.4 GB | 3.71 s | 3.50 s | **1.72 s (2.2×)** | 2.21 s |

The more files you have and the smaller they are, the bigger the win. With big files the drive's
raw bandwidth is the limit for both methods, so the gap shrinks.

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/volume-layout-dark.gif">
  <img alt="An exFAT volume drawn as one bar: boot region, FAT, allocation bitmap, up-case table, directories, file data. A single write head fills it from left to right." src="assets/volume-layout-light.gif" width="800">
</picture>

1. **Plan.** flashcp walks the source tree and lays out the whole volume in memory: boot region,
   FAT, allocation bitmap, up-case table, every directory (grouped together in one region), and
   every file's clusters. Each file is contiguous, so it uses exFAT's NoFatChain flag, the same as
   a file written by a well-behaved driver. The cluster size is picked to minimise bytes written
   (FAT + bitmap + per-file slack).
2. **Stream.** It writes the volume front to back as one strictly ascending stream of 8 MB aligned
   `O_DIRECT` writes, double-buffered so reading the next chunk overlaps with the device writing
   the current one.
3. **Done.** There is no step 3. The drive mounts anywhere exFAT mounts, with normal loose files.

The drive sees one ascending run of large, aligned writes instead of many small scattered ones,
and a handful of big USB commands replace hundreds of thousands of tiny ones. Our measurements show
the gap: the test SSD wrote 999 MB/s in 8 MB chunks but only 97 MB/s in 4 KB chunks.

## The journey (what didn't work)

I started by measuring the drive with `flashcp probe`:

| write pattern | throughput |
|---|---:|
| sequential, 8 MB writes | 999 MB/s |
| 4 KB writes | 97 MB/s |
| small files | ~9k files/s |

**Small files were 27× slower than sequential, on a top-end drive.** The question was where the time goes.

1. **A smarter userspace copier** (`flashcp copy`): parallel reads, ordered writes,
   preallocation, a single flush at the end. Result: **1.08–1.17×**. Barely anything. Linux's page
   cache already batches writes. The cost is the per-file work inside the kernel's exFAT driver,
   and no amount of cleverness in userspace gets around it.
2. **Pack everything into one stream** (`flashcp pack`): a tar file, with adaptive zstd that samples
   the data and only compresses when it helps. Result: **4.3–4.9×** on 20k small files. So one
   sequential stream *is* the answer, but now you have an archive, not files you can open.
3. **Write the filesystem myself** (`flashcp build`): if the driver is the bottleneck, don't use
   it. Build the entire exFAT volume in memory and stream it out. Result: **6.7× / 10.3× / 2.2×**,
   *with* loose files, including the time to format.

<details>
<summary><b>Earlier experiment results: copying onto an existing volume</b></summary>

Same drive and date. Speedup relative to the naive copier; `pack` and `pack-auto` produce a tar archive.

| dataset | naive | cp | smart | smart-w4 | pack | pack-auto |
|---|---|---|---|---|---|---|
| 20k files / 169 MB | 1.66 s | 1.01× | 1.08× | 1.17× | 4.31× | **4.93×** |
| 8.4k files / 1.4 GB | 2.63 s | 0.97× | 0.97× | 0.66× | 0.86× | 0.99× |

- With a good UAS drive on Linux the page cache already batches writes, so reordering in userspace buys at most ~15%.
- One sequential stream (pack) is ~5× faster for small files. Getting that speed with loose files meant skipping the
  filesystem driver, which led to `flashcp build`.
- Bigger gaps are expected on cheap BOT sticks and on Windows "quick removal" (no write cache). Not measured yet.

Known caveat: the probe's QD8 test uses 8 threads on one `O_DIRECT` file, so it can hit the filesystem's inode lock.
It may understate how much the device gains from parallel writes.

</details>

<details>
<summary><b><code>--fat prefix</code> vs <code>--fat full</code></b></summary>

`--fat prefix` (the default) writes only the FAT entries that are in use. exFAT treats the
allocation bitmap as the authority on which clusters are free, so free FAT entries don't matter.
On large drives with small clusters that saves up to a gigabyte of writes.

Tested: building over a junk-filled image, then writing 300 MB plus 300 new files with the Linux
kernel driver: `fsck.exfat` clean.

`--fat full` zeroes the whole FAT like `mkfs.exfat` does. Always spec-clean, a bit slower.

</details>

<details>
<summary><b>How the results were verified</b></summary>

After each build the volume was checked with `fsck.exfat`, mounted with the Linux kernel exFAT
driver, and compared to the source with `diff -r`. All passed. The volume layout follows
Microsoft's public exFAT specification (2019).

</details>

## Command reference

```
flashcp gen   <dir> --profile small|mixed|large|tiny   # reproducible test data
flashcp probe <mounted-dir>                            # measure the drive (temp file, no raw writes)
flashcp copy  <src> <dst> [--writers N] [--verify]     # pipelined, ordered loose-file copy
flashcp pack  <src> <out.tar> --compress auto|zstd|none
flashcp bench <src> <mounted-dir> --runs 3             # every method, timed incl. final flush
flashcp build <src> <device|image> --yes-erase         # whole exFAT volume in ONE sequential pass
```

`build` options: `--dry-run` (print the plan only), `--cluster <bytes>` (default: optimizer picks),
`--label <name>` (default `FLASHCP`), `--fat prefix|full`, `--readers <n>` (default 12).

## Honest limits

- **It builds a whole volume.** The target is erased. You can't add files to a drive that already has data (yet).
- **Linux only so far.** Windows and macOS reading the resulting drives is not yet verified.
- **Measured on one fast USB SSD.** Not yet benchmarked on a cheap BOT USB stick, where I expect the gap to be bigger, but haven't measured it.

## Roadmap

- [ ] **Append mode**: add files to an existing exFAT drive without erasing it (next up)
- [ ] Verify the drives on Windows and macOS
- [ ] Benchmarks on cheap USB sticks (BOT)
- [ ] A small desktop GUI (Tauri)
- [ ] A supporter edition for [Buy Me a Coffee](https://buymeacoffee.com/zuhuHix) supporters (planned; details to come)

## Support

If flashcp saved you some waiting, you can buy me a coffee. It helps pay for the time that goes
into the roadmap above.

<a href="https://buymeacoffee.com/zuhuHix"><img src="https://img.shields.io/badge/☕_Buy_me_a_coffee-zuhuHix-FFDD00?style=for-the-badge&logoColor=000" alt="Buy me a coffee"></a>

## Contributing

Issues and pull requests are welcome, especially:

- results from other drives (run `flashcp probe` and `flashcp bench`, and include the drive model and OS),
- reports of whether flashcp-built drives work on Windows and macOS,
- bugs found by `fsck.exfat` on a built volume.

Please don't run `build` on a device you care about while testing. Use an image file
(`truncate -s 4G test.img`, `flashcp build src test.img --yes-erase`, then `fsck.exfat test.img`).

## License

[GPL-3.0-or-later](LICENSE)
