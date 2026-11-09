# Simulating flashcp on USB drives

Tuning flashcp on real cheap sticks is slow and noisy. A 20k-file run on a USB 2 stick takes 30–200 s, the same run varies by ±40 %, and only a couple of physical drives exist. `flashcp sim` replays what flashcp *would* send to a drive against a device model, so two strategies can be compared in seconds. Real drives stay the final judge. Everything here runs on image files and never touches a block device.

The pipeline has four steps:

1. **Record.** Run flashcp on an image file with `FLASHCP_TRACE=<file>` (or the hidden `--trace <file>`). It logs every op it issues to the target, as JSON lines.
2. **Pretend.** Add `FLASHCP_SIM_DEVICE=<file>` and every strategy input flashcp reads from a real drive comes from that file instead (transport, max request size, logical block, removable, the cached device profile and its measured gap policies). An image on NVMe then gets *exactly* the op sequence the SanDisk would get.
3. **Replay.** `flashcp sim run TRACE --profile P` turns the op sequence into seconds on a device model.
4. **Sweep and calibrate.** These steps record and replay many variants at once.

```sh
flashcp sim profiles                                   # built-in device models
flashcp sim sweep --profile sandisk --workload small --op pull \
    --vary gap_kb=0,4,16,32,64 --vary inflight=1,2,4   # ranked table, ~10 s
flashcp sim sweep --profile sandisk --workload smallB --op add \
    --vary gap=skip,fill,pad16384 --vary qd=1,8 --seeds 8
flashcp sim calibrate                                  # real vs simulated, cross-validated
FLASHCP_SIM_DEVICE=src/sim/devices/sandisk.dev flashcp --trace pull.jsonl pull stick.img out/
flashcp sim run pull.jsonl --profile sandisk --profile msi --seeds 16
```

`sweep` and `calibrate` keep their datasets, images and traces under `$XDG_CACHE_HOME/flashcp/sim` (or `--work DIR`). A trace is cached by a hash of everything that affects the run, including the flashcp binary's size and mtime, so a rebuilt binary re-records. Host time is part of the trace (see below). Re-record on an idle machine before trusting numbers for host-bound drives (UAS SSDs): delete `traces/` under the work directory. `sweep` records UAS profiles one at a time for this reason.

## The trace (`src/sim/trace.rs`)

The trace is one JSON object per line: a `start` header (command, simulated device), then `io` (`op` r/w, `off`, `len`, `qd`, `grp`, `seq`, `q`, `t`, `d`), `sync`, `direct` (O_DIRECT toggles), `phase` markers (add: dirty/probe/data/alloc/fat+dir/clean, build: boot/stream/boot-final, pull: read, optimize: its phases), `note` (pull's strategy, add's gap policy) and `end`.

Which ops can overlap is part of the format:

- An op with `qd` 1, or in a different `grp` than the op before it, starts only after everything before it has finished.
- Within a group, op k waits for op k − qd of the same group.
- `seq` is the issuing code's own order. Racing threads may write lines of one group out of order, so the replay sorts by `seq`.

The hooks sit in flashcp's own I/O layer:

- SeqWriter's solo writes and its scattered-span pool (qd = write_qd);
- build's boot-region writes and syncs;
- add's metadata commits, VolumeDirty flips and in-situ probe trials;
- pull's reads (qd = inflight, seq = chunk index), its micro-probe and its tail;
- the validating volume reader;
- optimize's hash pass and raw reads.

When tracing is off, every hook costs one relaxed atomic load before the syscall it wraps. The volume bytes are identical, and `tests/sim_trace.rs` checks that traced and untraced builds write the same volume. Interleaved medians against `main` (image on NVMe, 21 pulls each): pull 0.366 s vs 0.366 s. Build and add were within ±6 % noise in both directions over 7 runs.

One property of flashcp shows up in traces: pull's metadata walk reads directories in an order that changes from process to process. The data reads are the same sequence every time.

### Simulated devices (`src/sim/device.rs`, `src/sim/devices/*.dev`)

A sim-device file is a devprofile cache file (the format of `~/.cache/flashcp/devices/*`) plus `sim.*` keys: `name`, `max_sectors_kb`, `logical_block`, `removable`, `probe_choice`, `part_start_sectors` and `size_bytes`.

`sim.probe_choice` is what add's in-situ gap probe decides when the profile has no pattern for this add. The probe's writes still run and are traced; only its decision, which would be timed on the image, is replaced. While the variable is set, nothing is written to the real profile cache.

The `size_bytes` of `sandisk.dev` (8,004,304,896) and `part_start_sectors` (2048) are guesses; see "Hardware measurements wanted".

## The model (`src/sim/model.rs`, `src/sim/ftl.rs`)

The replay preserves host time. Op i is issued at `max(issue[i−1], done(dep)) + gap[i] × host_scale`, where `gap[i]` is the host time the recording shows between the moment op i could have been issued and the moment it was (plus the recorded head and tail). The simulated total is the time on the simulated device plus the host work the recording shows, overlapped the way the dependency rule allows.

The device is modelled as follows:

- **Requests:** every syscall is split into requests of at most `max_req` bytes (the kernel's `max_sectors_kb`, 120 KB on the SanDisk).
- **Queue:** syscalls in flight are served round-robin, so their requests interleave.
- **Lanes:** a request takes one of `lanes` command slots (BOT 1, UAS 32) for `cmd + position penalty + qpen`, then the shared bus for `ser + len/bw + FTL cost`.
- **Position:** a request contiguous with the previous one in its direction costs nothing. A read resuming a recent stream costs `rd.switch` if the stick still tracks that stream (`rd.streams`), else `rd.thrash`. Anything else is a forward (`fwd`) or backward (`back`) jump.
- **`ser`:** a per-request cost on the shared controller that deep queues cannot hide, i.e. the IOPS ceiling of UAS drives.
- **`qpen`:** an empirical per-request penalty while other syscalls wait (BOT add with 8 writes in flight vs 1).
- **Sync:** a barrier plus `flush`.

**FTL (optional, SanDisk only).** The medium is cut into allocation units (`au`, 4 MiB), of which `open` (2) stay open, LRU.

- **Sequential writes** are free.
- **Random writes:** a write to an AU that is neither open nor continued, or a backward write, costs `open_cost` (this is the 48 random-4K IOPS).
- **Gapped writes:** an AU written with gaps pays a deferred merge, `merge + unwritten/copy_bw`, when it is evicted.
- **GC:** every block close triggers a GC stall with probability `gc_p`, lognormally distributed (`gc_mean`, `gc_sigma`) and seeded. `--seeds N` reports the mean and p10/p90. Calibration charges stalls at their mean.

### Profiles (`src/sim/profiles.rs`, `flashcp sim profiles NAME`)

| profile | basis |
|---|---|
| `sandisk-cruzer-usb2` | **Fitted.** Writes from the probe (4K 1.64 ms, 8M 6.5 MB/s → `wr.cmd` 1.05 ms, `wr.bw` 6.88 MB/s; reproduces 64K 6.2 and 1M 6.45 MB/s). `ftl.open_cost` 19.2 ms from random 4K QD1 at 48 IOPS. `rd.bw` 13.8 MB/s from the cached read probe. `max_req` 120 KB from sysfs. Read jump/stream costs, `wr.qpen`, `ftl.merge`/`copy_bw` and GC were tuned on the end-to-end scenarios below (so calibration does **not** start from them, see below). |
| `msi-datamag-uas` | **Fitted to the probe:** 4K QD1 97 MB/s and 8M 999 MB/s → `cmd` 18 µs, `ser` 20 µs, `bw` 1.02 GB/s. 32 lanes. Reads assumed equal to writes, and `ser`, `max_req` 1 MiB and `flush` are GUESSES. End-to-end MSI runs are 0.3–2.3 s and mostly host time. |
| `usb2-cheap`, `usb3-midrange`, `nvme-usb` | **GUESSES** at typical drives of their class, for what-if comparisons only. Nothing of these classes was measured. |

`--profile` also accepts a model file (`flashcp-simmodel 1`, `key=value`, as printed by `flashcp sim profiles NAME`) or a devprofile cache file. The latter turns any device flashcp has measured (`flashcp probe`, add's write probe, pull's read probe) into a model, with the unmeasured parameters taken from its transport's generic preset.

**Adding a profile:**

1. Write a model file starting from `flashcp sim profiles usb2-cheap`, or the cached devprofile of the drive.
2. Fill in what `flashcp probe` measured.
3. Add the drive's real end-to-end runs to `src/sim/scenarios.txt` (with a `drive=` of its own, a `devices/<drive>.dev` recording file and an entry in `workload.rs`/`profiles.rs`).
4. Run `flashcp sim calibrate --drive <drive>` and paste the table here.

## Calibration and honest validation

`src/sim/scenarios.txt` lists every real measurement from PRs #6, #7 and #9, with its source:

- 29 SanDisk scenarios and 15 MSI scenarios: build, add and pull on `flashcp gen` small/smallB/many/mixed or one 512 MB file, flashcp vs kernel-like layouts, the strategy variants that were measured, and the `flashcp probe` micro numbers;
- 14 hardware rankings ("pair" lines).

`flashcp sim calibrate` records every scenario on images (≈2 min with a warm cache) and fits the free parameters by least squares in log time. The fit is deterministic Nelder–Mead in log-parameter space, with a weak prior toward the starting model.

The error reported is held out. Each point's "held-out" prediction comes from a fit that did not see it (10-fold cross-validation), and ranking accuracy is computed on those held-out predictions. By default (`--prior probe`) the fit starts from a **probe-only prior**: every SanDisk parameter that was hand-tuned on these same scenarios is reset to the generic USB 2 guess, so no held-out point benefits from a value derived from it.

Kernel layouts are approximated by `mkfs.exfat -c <C>` plus `flashcp add` (no root or loop mount is needed). Real kernel `cp -r` layouts may differ in directory placement.

### Results (flashcp at this commit; held-out = cross-validated)

**SanDisk Cruzer Blade 8 GB (USB 2, BOT):** median |error| 3.7 % fitted, **4.9 % held out**, **rankings 11/11** correct (held out and fitted).

| scenario | real s | range | held-out s | error | flag |
|---|---:|---|---:|---:|---|
| sd-build-small | 33.6 | – | 33.63 | +0.1 % | |
| sd-build-small-512 | 28.5 | – | 28.67 | +0.6 % | |
| sd-build-small-1k | 29.6 | – | 29.20 | −1.4 % | |
| sd-build-512mb | 87.5 | – | 85.56 | −2.2 % | |
| sd-build-512mb-512 | 86.7 | – | 86.53 | −0.2 % | |
| sd-pull-flashcp | 16.0 | 15.6–16.6 | 16.74 | +4.7 % | |
| sd-pull-flashcp-8m | 17.6 | 17.6–22.1 | 19.16 | +8.9 % | |
| sd-pull-flashcp-qd4 | 27.9 | – | 21.40 | −23.3 % | |
| sd-pull-flashcp-1k | 15.2 | – | 16.05 | +5.6 % | |
| sd-pull-flashcp-512 | 14.8 | – | 17.88 | +20.8 % | |
| sd-pull-kernel | 27.0 | – | 27.15 | +0.5 % | |
| sd-pull-kernel-tail4k | 28.7 | – | 30.11 | +4.9 % | |
| sd-pull-kernel-v1 | 62.9 | – | 81.77 | +30.0 % | > 25 % |
| sd-add-32k-skip1 | 105 | 96–114 | 73.45 | −30.0 % | > 25 %, gapped writes |
| sd-add-32k-skip8 | 138 | 128–148 | 89.37 | −35.2 % | > 25 %, gapped writes |
| sd-add-32k-fill | 146 | 120–173 | 104.94 | −28.1 % | > 25 %, gapped writes |
| sd-add-32k-pad16k | 163 | 119–207 | 90.74 | −44.3 % | > 25 %, gapped writes |
| sd-add-4k | 36.5 | – | 38.80 | +6.3 % | |
| sd-add-1k | 29.7 | – | 30.27 | +1.9 % | |
| sd-add-512 | 28.8 | – | 29.69 | +3.1 % | |
| sd-add-empty32k | 34.9 | – | 33.79 | −3.2 % | |
| sd-add-empty32k-nobuild | 64 | – | 120.07 | +87.6 % | > 25 %, gapped writes |
| sd-micro-seqw4k | 26.84 | – | 21.11 | −21.3 % | |
| sd-micro-seqw64k | 10.82 | – | 10.91 | +0.8 % | |
| sd-micro-seqw1m | 10.49 | – | 10.54 | +0.5 % | |
| sd-micro-seqw8m | 10.32 | – | 10.46 | +1.4 % | |
| sd-micro-randw4k | 10.67 | – | 10.92 | +2.3 % | |
| sd-micro-flush | 0.135 | – | 0.170 | +26.0 % | > 25 % |
| sd-micro-seqr1m | 6.10 | – | 4.11 | −32.6 % | > 25 % |

**MSI DATAMAG 20Gbps (UAS):** median |error| 7.9 % fitted, **11.6 % held out**, **rankings 3/3** correct.

| scenario | real s | held-out s | error | flag |
|---|---:|---:|---:|---|
| msi-build-small | 0.31 | 0.301 | −2.8 % | |
| msi-build-512mb | 0.57 | 0.686 | +20.3 % | |
| msi-pull-flashcp | 0.37 | 0.330 | −10.9 % | |
| msi-pull-kernel | 0.66 | 0.562 | −14.8 % | |
| msi-pull-many-flashcp | 1.17 | 1.220 | +4.2 % | |
| msi-pull-many-kernel | 2.27 | 2.721 | +19.8 % | |
| msi-pull-mixed-flashcp | 1.68 | 1.591 | −5.3 % | |
| msi-pull-mixed-kernel | 1.82 | 2.197 | +20.7 % | |
| msi-add-onto-flashcp | 0.41 | 0.381 | −7.1 % | |
| msi-add-small-preload | 0.48 | 0.530 | +10.5 % | |
| msi-add-many-preload | 1.75 | 1.912 | +9.2 % | |
| msi-add-mixed-preload | 1.57 | 1.803 | +14.9 % | |
| msi-add-empty128k | 0.41 | 0.285 | −30.5 % | > 25 % |
| msi-micro-seqw8m | 1.075 | 1.199 | +11.6 % | |
| msi-micro-seqw4k | 2.767 | 1.822 | −34.1 % | > 25 % |

### Where the model is wrong, and why

- **add's gapped writes on 32 KB clusters (SanDisk, −28 to −44 %), and add into an empty 32 KB volume with build disabled (+88 %).** These have the same write pattern (one ascending stream skipping cluster slack), yet measured 105 s onto a used stick and 64 s onto a fresh mkfs. A trace cannot tell them apart; the stick's history can (which AUs hold old data that a merge must copy, and how much the FTL's spare pool has been used). The FTL layer models merge costs, but not that state.
  - Zero-filling is purely sequential (662 MB), yet it measured 4.5 MB/s against the 512 MB build's 6.1 MB/s. That also points at state (GC under a fuller medium) rather than at the op pattern.
  - The rankings among these strategies are right (skip:1 < skip:8 < fill and pad), and that is what the decisions rest on.
  - To fix: an FTL model with persistent medium state (prior occupancy and spare blocks), calibrated by measurements that record the stick's history (see below).
- **Reads with 4 and with 32 requests in flight (SanDisk, −23 % and +30 %).** One `rd.thrash` value cannot satisfy both: the stick's read-ahead behaves differently when 4 streams compete and when 32 do. The ranking holds.
- **512 B-cluster pull (+21 %), 4K writes and 1 MB reads in the probe:** these are small effects of request alignment that the model does not have.
- **MSI** is host-bound. Its error depends on how closely the recording machine matches the measuring one, and on recording one at a time. The fit absorbs part of the difference into `host_scale` (≈0.5). `msi-add-empty128k` and the 4K probe are the outliers.

## Sweeps that reproduce known decisions

```text
$ flashcp sim sweep --profile sandisk --workload small --op pull --layout kernel32k:small \
      --vary gap_kb=0,4,16,32,64 --vary inflight=1,2,4 --vary chunk_kb=120,8192      (9.6 s)
rank  gap_kb  inflight  chunk_kb  sim s  vs best  reads
1     4       2         8192      25.76  +0.0%    20205
2     0       2         8192      25.76  +0.0%    20205
4     0       2         120       25.80  +0.1%    20205
5     0       1         8192      26.54  +3.0%    20205
13    16      2         120       29.38  +14.1%   15066
19    32      1         120       42.90  +66.5%   5205
30    32      4         8192      72.33  +180.8%  284
```

The measured BOT decision was gap 0, 2 in flight, and reads at the 120 KB hardware request. The sweep reproduces it: gap ≤ 4 KB with 2 in flight is best, and reading through gaps ≥ 16 KB loses 14–70 %. 8 MB reads tie with 120 KB reads on this layout, but on the flashcp layout they lose (the sim gives 17.6 s → 19.2 s, matching the measured 17.6–22.1 s vs 16.0 s), and with 4 in flight they are the worst (+180 %).

```text
$ flashcp sim sweep --profile sandisk --workload smallB --op add \
      --vary gap=skip,fill,pad16384,pad8192 --vary qd=1,2,8 --seeds 8                (9.5 s)
rank  gap       qd  mean s  p10-p90 s      vs best
1     skip      1   94.59   93.70-95.38    +0.0%
2     pad8192   1   97.06   96.17-97.84    +2.6%
5     pad16384  1   102.05  101.16-102.84  +7.9%
9     fill      1   106.10  105.21-106.88  +12.2%
10    skip      8   126.11  125.22-126.90  +33.3%
```

This reproduces the measured order: skip-all with 1 in flight (96–114 s) beats 8 in flight (128–148 s), zero-fill (120–173 s) and 16 KB padding (119–207 s).

**New suggestion (UNVERIFIED, needs a hardware run):** on the 8 GB SanDisk, `many` (100k files, 413 MB) with 512 B clusters is predicted to build in **71 s instead of 100 s** (auto picks 4 KB, because the data fills < 50 % of the stick) and to pull in **36.5 s instead of 48.8 s**. That would argue for relaxing the sub-4K gate's 50 % fill rule when tiny files dominate. To verify: `flashcp build many /dev/sdX1 --cluster 512` vs auto, then `flashcp pull` of each.

## Hardware measurements wanted

These are not needed for the rankings above, but they would tighten the model:

1. The SanDisk's partition start and size (`cat /sys/class/block/sdX1/{start,size}`) to replace the guesses in `sandisk.dev`.
2. The MSI's `max_sectors_kb`, and a QD32 4K read IOPS figure to pin `ser`.
3. Two adds onto 32 KB clusters with the same strategy, one right after `mkfs` + full-stick TRIM/erase and one after the stick was filled once, to separate stick history from the op pattern.
4. The new suggestion above (`many` at 512 B vs 4 KB clusters).

## Tests

- `tests/sim_trace.rs`: traced vs untraced builds are identical; recordings are deterministic and follow the simulated device (BOT strategy, 4 KB cluster cap, partition offset, no profile caching); `sim run` never touches its input.
- Unit tests in `src/sim/*`:
  - parsers round-trip;
  - the dependency rule, BOT interleaving and request splitting;
  - host-gap preservation;
  - FTL merges on random vs sequential writes;
  - per-seed determinism;
  - calibration recovers known parameters, and cross-validation honesty;
  - the sweep's vary/combination mapping;
  - scenario parsing and the recording cache.
