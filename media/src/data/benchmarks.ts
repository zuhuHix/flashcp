// Every measured number the animations show, in one place. Change a number here and re-render
// (`./render.sh`); no layout code needs touching. Times are seconds.
//
// Sources:
//   [K]   Kingston DataTraveler 3.0 128 GB (USB 3, usb-storage/BOT), 2026-11-10 16:33-17:07,
//         dataset `small` (20,000 files / 169 MB), SINGLE runs, fsck clean after every write.
//   [K2]  the same stick at 18:09, kernel driver only, ending with unmount, phases timed:
//         4 KB clusters:   mkfs 1.53 | mount 0.38 | cp + unmount 50.28 | all 52.20 s
//         128 KB clusters: mkfs 0.08 | mount 0.38 | cp + unmount 288.26 | all 288.72 s
//   [#18] PR 18 (Kingston, morning session), [#22] PR 22 (drive mode core), [#9] PR 9 (SanDisk),
//         [#7] PR 7 and [#6] PR 6 (MSI SSD and SanDisk build / add / pull).
// A number that is not in one of these does not belong in an animation.

export type Bar = {
  /** seconds; with `max`, the low end of a measured range */
  value: number;
  /** high end of a measured range (drawn as a fainter extension of the bar) */
  max?: number;
  /** the text next to the bar, without the unit */
  label: string;
};

export type BenchRow = {
  name: string;
  sub: string;
  /** the normal route: the Linux kernel exfat driver (mkfs / mount + cp) */
  base: Bar;
  flash: Bar;
  /** speed-up text, computed by hand from the two bars and checked in the README table */
  pill: string;
};

export type BenchPage = {
  title: string;
  baseLegend: string;
  flashLegend: string;
  /** right end of the seconds axis (the longest bar) */
  axisMax: number;
  ticks: number[];
  rows: BenchRow[];
  /** two footnote lines */
  notes: [string, string];
};

export const DATASET = {files: 20000, mb: 169};

export const BENCH_PAGES = {
  // [K] and [K2]. The kernel copied onto a 4 KB stick twice: 73.98 s in all at 16:5x (about 2 s
  // of it mkfs + mount) and 50.28 s for cp + unmount at 18:09 (52.20 s in all).
  // Drive mode (cp + unmount + commit, no formatting): 50.28 / 5.31 = 9.5 and 73.98 / 5.31 = 13.9.
  // Build (formats too, so against the kernel totals): 52.20 / 9.02 = 5.8 and 73.98 / 2.72 = 27.2.
  // 128 KB: 258.8 / 93.8 = 2.8, 288.7 / 93.8 = 3.1 (both unmount only), 370.7 / 93.8 = 4.0 (sync).
  usb3: {
    title: 'USB 3 stick · Kingston DataTraveler 3.0 · 20,000 small files',
    baseLegend: 'Linux kernel driver (mkfs + cp)',
    flashLegend: 'flashcp',
    axisMax: 370.7,
    ticks: [0, 100, 200, 300],
    rows: [
      {
        name: 'drive mode copy',
        sub: '4 KB-cluster stick',
        base: {value: 50.3, max: 74.0, label: '50–74'},
        flash: {value: 5.31, label: '5.31'},
        pill: '9–14×',
      },
      {
        name: 'build',
        sub: 'blank stick, 4 KB both',
        base: {value: 52.2, max: 74.0, label: '52–74'},
        // 2.72 s [K]; 9.02 s [#18]: the stick has a fast phase, so small builds vary
        flash: {value: 2.72, max: 9.02, label: '2.72–9.02'},
        pill: '6–27×',
      },
      {
        name: 'drive mode copy',
        sub: 'default 128 KB clusters',
        // whole runs (mkfs + mount are 0.46 s of them): unmount only 258.8 s [K] and 288.7 s [K2],
        // sync -f + unmount 315.5 s, sync + unmount 370.7 s
        base: {value: 258.8, max: 370.7, label: '259–371'},
        flash: {value: 93.8, label: '93.8'},
        pill: '2.8–4.0×',
      },
    ],
    notes: [
      'single runs · 128 KB kernel copies: 259–289 s ending with unmount, up to 371 s with a final sync',
      '4 KB kernel copies: 50–74 s over two runs · small builds vary with the stick’s state',
    ],
  },
  // [#6] [#7] medians of 3; drive mode row [#22] (durable copy, medians of 3).
  ssd: {
    title: 'USB SSD · MSI DATAMAG 20Gbps (UAS) · 20,000 small files',
    baseLegend: 'Linux kernel driver (mkfs / mount + cp)',
    flashLegend: 'flashcp',
    axisMax: 6.01,
    ticks: [0, 2, 4, 6],
    rows: [
      {
        name: 'build',
        sub: 'erase + format',
        base: {value: 2.8, label: '2.80'},
        flash: {value: 0.42, label: '0.42'},
        pill: '6.7×',
      },
      {
        name: 'add',
        sub: 'existing drive',
        base: {value: 1.72, label: '1.72'},
        flash: {value: 0.48, label: '0.48'},
        pill: '3.6×',
      },
      {
        name: 'pull',
        sub: 'copy off the drive',
        base: {value: 6.01, label: '6.01'},
        flash: {value: 0.66, label: '0.66'},
        pill: '9.1×',
      },
      {
        name: 'drive mode copy',
        sub: 'until safe to remove',
        base: {value: 1.8, label: '1.80'},
        flash: {value: 1.26, label: '1.26'},
        pill: '1.4×',
      },
    ],
    notes: [
      'medians of 3 · baselines include mkfs or mount and unmount, plus sync',
      'with 100,000 files: build 10.3×, pull from a flashcp-built drive 26.8×',
    ],
  },
  // [#9] [#7]. Build: 110.4 / 33.6 = 3.3 and 179.6 / 33.6 = 5.3. Add: 209.8 / 84.1 = 2.5 and
  // 209.8 / 114 = 1.8 (flashcp's add measured 84.1 s once and 96-114 s in other runs).
  usb2: {
    title: 'USB 2.0 stick · SanDisk Cruzer Blade 8 GB · 20,000 small files',
    baseLegend: 'Linux kernel driver (mkfs / mount + cp)',
    flashLegend: 'flashcp',
    axisMax: 209.8,
    ticks: [0, 50, 100, 150, 200],
    rows: [
      {
        name: 'build',
        sub: 'vs default format + cp',
        base: {value: 110.4, max: 179.6, label: '110.4–179.6'},
        flash: {value: 33.6, label: '33.6'},
        pill: '3.3–5.3×',
      },
      {
        name: 'add',
        sub: 'same 32 KB-cluster stick',
        base: {value: 209.8, label: '209.8'},
        flash: {value: 84.1, max: 114, label: '84.1–114'},
        pill: '1.8–2.5×',
      },
      {
        name: 'pull',
        sub: 'kernel-filled stick',
        base: {value: 30.86, label: '30.86'},
        flash: {value: 29.52, label: '29.52'},
        pill: '1.05×',
      },
      {
        name: 'pull',
        sub: 'flashcp-built stick',
        base: {value: 32.78, label: '32.78'},
        flash: {value: 16.38, label: '16.38'},
        pill: '2.0×',
      },
    ],
    notes: [
      'the flash chip is the ceiling here: 6.5 MB/s sequential write, 13.8 MB/s read (measured)',
      'build and pull: medians of 3 · add: single runs · this stick varies a lot from run to run',
    ],
  },
} satisfies Record<string, BenchPage>;

export type BenchPageId = keyof typeof BENCH_PAGES;

// Drive mode hero: one copy of `small` onto the Kingston with 4 KB clusters. [K] [K2]
export const DRIVE_MODE = {
  label: 'KINGSTON',
  /** one `cp -r` of the 20,000 files returned after 0.93 s */
  appDoneS: 0.93,
  /** cp + unmount + the final commit: the stick can be removed */
  durableS: 5.31,
  /** kernel exfat driver, same 4 KB format, cp + unmount: the faster of its two runs.
   *  The strip is drawn at this pace. */
  kernelS: 50.28,
  /** both kernel runs: 50.28 s, and 73.98 s that includes about 2 s of mkfs + mount */
  kernelLabel: '50–74',
  /** 50.28 / 5.31 = 9.5 to 73.98 / 5.31 = 13.9 */
  speedup: '9–14×',
  footnote: 'Kingston DataTraveler 3.0 (USB 3) · 20,000 files, 169 MB · 4 KB clusters · single runs',
};

// Cluster slack: the same copy in drive mode onto the same stick, two formats. [K]
// MB written are the device's own counters (2,525 MiB and 201 MiB).
export const CLUSTER_SLACK = {
  big: {clusterKb: 128, writtenMb: 2648, seconds: 93.8, writtenLabel: '2.6 GB'},
  small: {clusterKb: 4, writtenMb: 211, seconds: 5.31, writtenLabel: '211 MB'},
  /** 93.8 / 5.31 */
  speedup: '17.7×',
  footnote: 'Kingston DataTraveler 3.0 · drive mode, copy until safe to remove · single runs',
};

// flashcp sync on the Kingston with 4 KB clusters, 20,000 files already on it. [K]
export const SYNC = {
  rows: [
    {label: 'nothing changed', changedFiles: 0, seconds: 0.21, written: '0 MB'},
    {label: '200 files changed', changedFiles: 200, seconds: 1.17, written: '2 MB'},
    {label: '1,800 files changed', changedFiles: 1800, seconds: 2.67, written: '20 MB'},
  ],
  /** every modification time touched, so sync rewrote every file */
  full: {label: 'every file rewritten', seconds: 8.89, written: '224 MB'},
  footnote: 'Kingston DataTraveler 3.0 (USB 3) · 20,000 files, 169 MB · 4 KB clusters · single runs',
};
