# README animations

The animations in the README and on the Pages site are [Remotion](https://www.remotion.dev)
compositions: React components that draw one frame at a time. Nothing here is screen-recorded.

## Re-render everything

```sh
cd media
npm ci          # once
./render.sh     # needs Node and ffmpeg
```

That writes:

- `assets/<name>-light.gif` and `assets/<name>-dark.gif` for every scene (the README picks one with
  `<picture>` and `prefers-color-scheme`),
- `docs/media/<name>-<theme>.mp4` for the scenes the site plays as video,
- `docs/flashcp.mp4`, the site's dark showreel.

To render only some scenes, name them: `./render.sh DriveMode BenchmarkChart-usb3`. `Showreel`
is a name too. Intermediate files go to `media/out/` (not tracked).

`npm run studio` opens Remotion Studio for scrubbing through a scene, and `npm run typecheck`
checks the sources.

## Scenes

| scene | file | what it shows |
|---|---|---|
| `DriveMode` | `drive-mode` | the hero: any app → flashcp's driver → the stick, Eject, and the kernel driver on the same time scale |
| `ClusterSlack` | `cluster-slack` | why 128 KB clusters make small files slow, and the same copy on 4 KB clusters |
| `SyncOnlyChanged` | `sync-only-changed` | `flashcp sync`: three repeat runs, only the changed files cross over |
| `BenchmarkChart-usb3`, `-ssd`, `-usb2` | `benchmark-usb3`, … | one bar chart per measured drive |
| `CopyVsBuild` | `copy-vs-build` | per-file round trips against one pass (`build`) |
| `VolumeLayout` | `volume-layout` | the exFAT volume `build` writes, front to back |
| `AddNoErase` | `add-no-erase` | `add`: new files into free space, then the tables |
| `PullSweep` | `pull-sweep` | `pull`: one sweep in physical order |
| `TwoDrives` | `two-drives` | why a cheap stick gains less than an SSD |

## Changing a number

Every measured number the newer scenes show is in [`src/data/benchmarks.ts`](src/data/benchmarks.ts),
with its source next to it. Change it there and re-render; the layouts take their numbers, bar
lengths and time scales from that file. A chart page can have three or four rows, and a bar can
be a range (`value` and `max`). To add a drive, add a page to `BENCH_PAGES` and a line to `NAME`
and `ALL` in `render.sh`.

Rules for a number: it comes from a merged PR body or a logged hardware run, it says which drive,
dataset and baseline it belongs to, and single runs are called single runs. Update the image's
alt text in `README.md` when a number changes: the alt text states every number shown.

## Conventions

- 1200 px wide, 30 fps, 8–13 s loops. A README image is 800 px wide, so no text is below 22 px
  in the newer scenes.
- Light and dark come from one component with a `theme` prop; colours are in `src/shared.ts`.
  Coral is the kernel driver, teal is flashcp, amber is metadata or a changed file.
- Motion is a pure function of the frame (no CSS transitions, no `Math.random()`), so frames can
  be rendered in any order. `src/motion.ts` has the curve helpers.
- GIFs are 900 px wide at 20 fps with a 128-colour palette. `render.sh` steps down (15 fps,
  800 px, 64 colours) until a GIF is under 3 MB.
- After rendering, look at a few frames of each GIF in both themes before committing:
  `ffmpeg -i assets/drive-mode-dark.gif -vf "select='eq(n,100)'" -frames:v 1 frame.png`.
