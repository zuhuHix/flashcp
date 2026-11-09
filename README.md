# flashcp

Experiments in making file transfers to USB sticks and external drives fast, especially many small files.

License: GPL-3.0-or-later · Support the project: https://buymeacoffee.com/zuhuHix

```
flashcp gen   <dir> --profile small|mixed|large|tiny   # reproducible test data
flashcp probe <mounted-dir>                            # measure the drive (temp file, no raw writes)
flashcp copy  <src> <dst> [--writers N] [--verify]     # pipelined, ordered loose-file copy
flashcp pack  <src> <out.tar> --compress auto|zstd|none
flashcp bench <src> <mounted-dir> --runs 3             # every method, timed incl. final flush
flashcp build <src> <device|image> --yes-erase         # whole exFAT volume in ONE sequential pass
```

## `flashcp build`: the one-pass exFAT builder

Instead of the kernel creating files one by one, flashcp plans the whole volume in memory first:
boot region, FAT, allocation bitmap, up-case table, every directory, every file's clusters.
Then it writes it all as one ascending stream of 8 MB aligned O_DIRECT writes, double-buffered
so reading the next chunk overlaps with the device writing the current one. Directories sit
together in one region, file data follows in folder order, and every file is contiguous
(NoFatChain). It picks the cluster size automatically to minimise bytes written
(FAT + bitmap + per-file slack). The result is a normal exFAT drive with normal loose files.

`--fat prefix` (default) writes only the FAT entries in use. exFAT treats the allocation bitmap
as the authority, so free FAT entries don't matter. Tested: building over a junk-filled image,
then 300 MB plus 300 new files written by the Linux kernel driver: fsck clean. `--fat full`
zeroes the whole FAT like mkfs does.

### Results, 2026-11-09: MSI DATAMAG 20Gbps, 1 TB partition, Linux 7.0

Baseline is `mkfs.exfat` then mount, `cp -r`, sync. flashcp time includes formatting.
Every build passed `fsck.exfat` and a byte-for-byte `diff -r` after a kernel mount.

| dataset | mkfs + cp | mkfs + smart copy | **build (prefix)** | build (full FAT) |
|---|---|---|---|---|
| 20k files / 169 MB | 2.80 s | 2.85 s | **0.42 s (6.7×)** | 0.73 s |
| 100k files / 413 MB | 8.90 s | 8.80 s | **0.86 s (10.3×)** | 1.83 s |
| 8.4k mixed / 1.4 GB | 3.71 s | 3.50 s | **1.72 s (2.2×)** | 2.21 s |

Limits right now: builds a whole volume (erases the target). Not yet tested on Windows or macOS.
Not yet tested on a cheap BOT stick. No append-to-existing-volume mode yet.

## Results, 2026-11-09: MSI DATAMAG 20Gbps (UAS, 10 Gb/s link, exFAT, Linux)

Probe: 999 MB/s sequential at 8 MB writes, 97 MB/s at 4 KB writes, ~9k small files/s → **small files 27× slower than sequential**.

| dataset | naive | cp | smart | smart-w4 | pack | pack-auto |
|---|---|---|---|---|---|---|
| 20k files / 169 MB | 1.66 s | 1.01× | 1.08× | 1.17× | 4.31× | **4.93×** |
| 8.4k files / 1.4 GB | 2.63 s | 0.97× | 0.97× | 0.66× | 0.86× | 0.99× |

Takeaways:
- On Linux with a good UAS drive, the page cache already batches writes, so reordering in userspace buys at most ~15%.
  The cost is per-file work inside the kernel's exFAT driver.
- One sequential stream (pack) is ~5× faster for small files. Getting that speed **with loose files** means skipping
  the filesystem driver: next step is the one-pass exFAT image builder.
- Expect much bigger gaps on cheap BOT sticks and on Windows "quick removal" (no write cache). Not measured yet.

Known caveat: the probe's QD8 test uses 8 threads on one O_DIRECT file, so it can hit the filesystem's inode lock.
It may be understating how much the device gains from parallel writes.
