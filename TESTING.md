# Testing flashcp

flashcp writes an exFAT volume itself, so the tests have to check two things: that the bytes
match the spec, and that real operating systems accept the volume and read the files back
correctly. The layers below go from the cheapest check to the most independent one.

## Overview

| Layer | What it proves | What it does not prove | Where it runs | Run locally |
|---|---|---|---|---|
| Unit tests (`src/exfat.rs`, 30 tests) | Boot checksum matches the mkfs reference, the up-case table checksum, the entry-set layout against kernel-made files and directories, timestamp encoding and clamping (1980 to 2107), name rules, geometry invariants over a sweep of sizes. | That a whole volume is valid. Nothing is mounted or read back. | `ci.yml` `test` job (Ubuntu, `FLASHCP_REQUIRE_FSCK=1`). | `cargo test --locked` |
| Integration tests (`tests/build.rs`) | The real binary builds images that are read back by `tests/common/exfat_reader.rs`, a parser written from the Microsoft exFAT spec that shares no code with the writer. Names, types, bytes and mtimes (10 ms resolution) must match the source. `fsck.exfat -n` must be clean when it is installed. Also covers the error paths (missing `--yes-erase`, names that differ only by case, a colon in a name, a label over 11 characters, a non-block target) and that `--dry-run` leaves the image untouched. | Anything about other operating systems. The reader is only as good as our reading of the spec. | `ci.yml` `test` job. | `cargo test --locked` (install `exfatprogs` for `fsck.exfat`) |
| Linux mount (`ci.yml` `image` job, and the root-only `mount` tests) | The Linux kernel exFAT driver mounts the volume read-only and `diff -r` finds the same content, for `--fat prefix` and `--fat full`. The mount tests build a volume and mount it through a loop device (as root) or `udisksctl loop-setup` (not as root), and check that a busy device is refused. | Windows and macOS. `diff -r` compares content, not mtimes. | `ci.yml` `image` job. The mount tests run only with `FLASHCP_MOUNT_TESTS=1` and are root-gated. | See the Linux section below. |
| Windows chkdsk (`xos.yml` `windows` job) | The Windows exFAT driver attaches the VHD, `chkdsk` (read-only) reports no problems before and after a write, every file matches the manifest, and Windows can write to the volume. | Behaviour of a particular USB drive or controller. Removable-media policy. | `xos.yml`, `windows-latest`, one job per volume. | Section 3, Windows. |
| macOS fsck_exfat (`xos.yml` `macos` job) | macOS `fsck_exfat -n` is clean before and after a write, the volume mounts, every file matches, and macOS can write to it. | Behaviour of a particular USB drive. Macs with other macOS versions. | `xos.yml`, `macos-latest`, one job per volume. | Section 3, macOS. |

`FLASHCP_REQUIRE_FSCK=1` makes a missing `fsck.exfat` a failure instead of a SKIP. The `test`
job sets it, and so does the mount step in `image`. That step also fails if its log contains
`SKIP`, so a gated mount test cannot pass silently.

## The cross-OS workflow

`xos.yml` builds the volumes once on Linux (`scripts/xos/build_images.sh`) and uploads them as
artifacts. The Windows and macOS jobs download one volume each and check it.

### The source tree

`scripts/xos/gen_tree.py` writes a deterministic tree (seed 20240517), deliberately awkward:

- Directory and file names in Greek, Japanese, French and German, including names with an
  emoji. Every 53rd small file gets an emoji or CJK stem.
- A 200-character file name, written with BMP characters only.
- Eight levels of nesting (`d1/.../d8/leaf.txt`).
- About 2000 small files (0 to 8 KiB, random content) spread over 20 directories.
- Empty files and empty directories, including nested empty directories.
- Four names that differ only by accents (`cafe.txt`, `café.txt`, `cafè.txt`, `cafê.txt`), all NFC.
- A 50 MiB file, `big.bin`.
- Modification times in 1980, 2000 and 2100 (in `timestamps/`), plus a fixed 2024 time for
  everything else. The sub-second parts are multiples of 10 ms, so exFAT's truncation does not
  change them.

`scripts/xos/manifest.py` then records every entry: NFC relative path, type, size, mtime in
nanoseconds, and SHA-256 for files. It skips top-level junk such as `System Volume Information`,
`.Spotlight-V100` and `._*` files. It refuses symlinks and NFC collisions.

### The volume matrix

Every volume is labelled `XOSTEST`. The matrix comes from `VOLUMES` in `build_images.sh`, plus
two more volumes:

| Name | Built with | Notes |
|---|---|---|
| `prefix-auto` | 512 MiB, `--fat prefix` | Cluster size chosen by the optimizer. Also the source of `superfloppy`. |
| `full-auto` | 512 MiB, `--fat full` | Whole FAT zeroed, like `mkfs.exfat`. |
| `prefix-4k` | 512 MiB, `--fat prefix --cluster 4096` | |
| `prefix-128k` | 512 MiB, `--fat prefix --cluster 131072` | |
| `prefix-1g` | 1 GiB, `--fat prefix` | |
| `superfloppy` | The `prefix-auto` volume with no partition table | Raw volume as `.img` and `.vhd`. Report-only. |
| `control-mkfs` | `mkfs.exfat -L XOSTEST`, then `sudo cp -r --preserve=timestamps` on Linux | Not written by flashcp. Report-only. |

Before any volume is wrapped, `build_images.sh` runs `fsck.exfat -n` on it. If passwordless sudo
is available it also loop-mounts it read-only and runs `verify.py` against the manifest. This
checks the verifier against the Linux kernel driver before Windows and macOS are asked to agree
with it.

### Wrapping for Windows and macOS

- `mbr_wrap.py` puts the volume in an MBR disk. There is one primary partition, type `0x07`, at
  LBA 2048 (1 MiB), and its length is the volume length. Both exFAT boot regions have
  `PartitionOffset` set to 2048, and each boot checksum (sector 11 and sector 23) is recomputed
  per the spec. The script checks the signature, the sector size and the existing checksums
  before it patches anything, then reads the output back and verifies it. `build_images.sh`
  runs `fsck.exfat -n` on the partition inside the MBR image.
- `mkvhd.py` appends the 512-byte footer of a fixed VHD (disk type 2) so Windows can attach the
  image with `Mount-DiskImage`. The raw image is padded to a multiple of 512 bytes first.

The `superfloppy` entry has no MBR. It is report-only because support for partitionless disks
differs between OSes and disk types, but in practice both Windows and macOS accept it (see the
results below).

### The manifest and the verify rules

`verify.py` compares a mounted volume with `manifest.json` and reports each difference as a
problem or a warning:

- A missing, extra or wrong-type entry is a problem. So is a size or SHA-256 mismatch.
- mtime is compared after truncating the expected value to exFAT's 10 ms tick. A difference
  within 10 ms passes.
- A difference that is a whole number of 15-minute steps (within 2 s) is a TIMEZONE failure.
  Runners are set to a UTC+5:30 zone, so a driver that mishandles the UTC-offset field shows
  up as an error of 5.5 hours (22 steps), which is caught here.
- Any other difference within 2 s is a warning. A larger difference is an MTIME failure.
- A name on disk that is not NFC is a warning. A name that collides after NFC normalization
  is a failure.

Entries with `mtime_ns: null` skip the mtime check. The write test uses this for new files and
for directories, whose mtimes change when their contents change.

### The write test

`write_test.py` runs against the mounted volume:

1. It verifies the volume against the manifest, and stops if the baseline is already wrong.
2. It deletes 50 files chosen with a fixed seed (never `big.bin` or the timestamp files).
3. It renames one top-level directory that holds files, to `<name>-renamed`.
4. It creates `xos-new/` with 300 files of 0 to 64 KiB, each fsynced.
5. It writes a new manifest for the resulting state and verifies the volume against it.

This tests that the OS can write to a flashcp volume and keep it consistent, not just read it.

### The NFC probe

`nfc_probe.py` is report-only and always exits 0. It runs after the write test because it
changes the volume. For names the volume's writer stored (in `accents/`) it tries stat, read,
rename and unlink, addressing each name once in NFC and once in NFD. For names the OS creates
itself, in either form, it records which form `readdir` returns and whether stat and unlink
work in each form. The write test only deletes and renames ASCII names, so these Unicode cases
are measured here and do not fail the job. Running the same probe on `control-mkfs` shows
whether a difference comes from flashcp or from the OS driver.

### Report-only entries

`superfloppy` and `control-mkfs` use `continue-on-error` in both the Windows and macOS jobs.
A superfloppy disk has no partition table, and OS support for those varies by OS and disk type, so
failure there does not necessarily mean the volume is bad. `control-mkfs` is a reference: the
same tree written by a different tool. If `control-mkfs` fails the same way a flashcp volume
does, the cause is the OS driver, not flashcp. If only the flashcp volume fails, the cause is
flashcp.

### Logs

- `logs-windows-<volume>` (14 days): `chkdsk-<label>-fresh.txt`, `chkdsk-<label>-after-write.txt`,
  `verify-<label>.txt`, `nfcprobe-<label>.txt`, `writetest-<label>.txt`, `after-<label>.json`.
- `logs-macos-<volume>` (14 days): the same set, with `fsck_exfat-<label>-*.txt` in place of chkdsk.
- The `xos-*` volume artifacts are kept for 3 days.
- Both scripts write one line per step to the job summary. A failed check adds the last 25 lines
  of its output.

## Running the cross-OS checks yourself

On Linux, with `exfatprogs`, Python 3 and `sudo` available:

```sh
cargo build --release --locked
scripts/xos/build_images.sh target/release/flashcp out
```

This writes `out/<volume>/<volume>.img`, `out/<volume>/<volume>.vhd` and
`out/<volume>/manifest.json`, plus `out/superfloppy/` and `out/control-mkfs/`. Without
passwordless sudo the script skips the Linux mount check and the control volume, and says so.

On Windows, in an administrator PowerShell with `python` on the PATH (the script calls it):

```powershell
pwsh scripts/xos/win_check.ps1 -Vhd out/prefix-auto/prefix-auto.vhd -Manifest out/prefix-auto/manifest.json -Label prefix-auto -LogDir logs
```

Add `-Superfloppy` for `out/superfloppy/superfloppy.vhd`. The write test changes the attached
VHD, so copy the file first if you want to reuse it.

On macOS:

```sh
scripts/xos/mac_check.sh out/prefix-auto/prefix-auto.img out/prefix-auto/manifest.json prefix-auto logs
scripts/xos/mac_check.sh out/superfloppy/superfloppy.img out/superfloppy/manifest.json superfloppy logs superfloppy
```

The script attaches the image with `hdiutil` and runs `sudo fsck_exfat`. The write test changes
the attached image, so copy it first if you want to keep it. Use the `.img` on macOS, not the
`.vhd`.

## Checking a real drive by hand

Use an image or a drive you can erase. `flashcp build` erases the whole target device.

Always unmount or eject the drive before you unplug it.

### Windows

1. Plug the drive in and find its drive letter (File Explorer, or `Get-Volume` in PowerShell).
   Use `E:` as the letter in the next step.
2. In an administrator terminal, run `chkdsk E:`. Do not add `/f`. Without it chkdsk is
   read-only. Expect `Windows has scanned the file system and found no problems.`
3. Compare the files. Either run `verify.py` with a manifest made on the source machine
   (`python manifest.py <source> manifest.json`, then `python verify.py manifest.json E:\ --label usb`),
   or check single files with `Get-FileHash -Algorithm SHA256`. For a rough check,
   `robocopy <source> E:\ /L /E /NJH` lists what would be copied. Nothing is copied with `/L`.
4. Disk Management shows a flashcp drive without a partition table (a superfloppy). That is
   expected. In CI, Windows mounts the superfloppy VHD and chkdsk accepts it.
5. Eject the drive with File Explorer or the tray icon before you unplug it.

### macOS

1. Run `diskutil list` to find the disk, for example `/dev/disk4`.
2. Unmount it with `diskutil unmount /dev/disk4`. If the drive has a partition table, use
   `/dev/disk4s1` instead.
3. Check it with `sudo fsck_exfat -n /dev/rdisk4` (or `/dev/rdisk4s1`). Expect the output to say
   the volume `appears to be OK`.
4. Mount it with `diskutil mount /dev/disk4` (or `/dev/disk4s1`).
5. Compare the files with `verify.py` and a manifest made on the source machine, or with
   `shasum -a 256` on individual files. macOS lists exFAT names in NFD, so a byte-for-byte
   `diff -r` against an NFC source made on another OS can report false differences. `verify.py`
   normalizes names and reports NFD names as warnings only.
6. Run `diskutil eject /dev/disk4` before you unplug it.

### Linux

1. Check the unmounted volume with `sudo fsck.exfat -n /dev/sdX`. Use `/dev/sdXN` if the drive
   has a partition table.
2. Mount it read-only: `sudo mount -o ro /dev/sdX /mnt/usb`.
3. Compare with `sudo diff -r <source> /mnt/usb`. `diff -r` compares contents only. Use
   `verify.py` if you also want mtimes checked.
4. Run `sudo umount /mnt/usb`, then `sync` before you unplug the drive.

## Cross-OS results

Run [42616790178](https://github.com/zuhuHix/flashcp/actions/runs/42616790178)
(2026-11-09, `windows-latest` and `macos-latest`, runner time zone UTC+5:30):

| Volume | Linux fsck.exfat + mount | Windows chkdsk, before / after write | Windows files | macOS fsck_exfat, before / after write | macOS files |
|---|---|---|---|---|---|
| `prefix-auto` (512 MiB, MBR) | OK | OK / OK | OK | OK / OK | OK |
| `full-auto` (512 MiB, MBR) | OK | OK / OK | OK | OK / OK | OK |
| `prefix-4k` (512 MiB, 4 KiB clusters, MBR) | OK | OK / OK | OK | OK / OK | OK |
| `prefix-128k` (512 MiB, 128 KiB clusters, MBR) | OK | OK / OK | OK | OK / OK | OK |
| `prefix-1g` (1 GiB, MBR) | OK | OK / OK | OK | OK / OK | OK |
| `superfloppy` (512 MiB, no partition table) | OK | OK / OK | OK | OK / OK | OK |
| `control-mkfs` (mkfs.exfat + Linux, MBR) | OK | OK / OK | OK | OK / OK | OK |

"Files" means all 2054 files and 36 directories matched on content, size and mtime (exact to
the 10 ms tick, including the 1980, 2000 and 2100 dates, with no time-zone shift), and the
write test passed.

Unicode names (from `nfc_probe.py`; identical on flashcp volumes and on `control-mkfs`):

- Windows returns names exactly as stored (NFC) and matches them exactly: an NFD spelling of an
  NFC name is a different name.
- macOS returns every non-ASCII name in NFD. Stat, open and rename find a name in either form.
  Unlink only works when the name is given in the form it was stored in on disk. Unlinking an
  NFC-stored name by its NFD spelling, which is the form `readdir` returns, fails with ENOENT.
  This happens on the mkfs.exfat control volume too, so it is a macOS driver behaviour, not a
  flashcp one. Not yet checked: whether Finder is affected. Scripts that delete files by the
  names `readdir` returns are.
