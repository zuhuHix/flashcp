import React from 'react';
import {Composition} from 'remotion';
import {AddNoErase} from './AddNoErase';
import {BENCH_FRAMES, BenchmarkChart} from './BenchmarkChart';
import {CLUSTER_SLACK_FRAMES, ClusterSlack} from './ClusterSlack';
import {CopyVsBuild} from './CopyVsBuild';
import {BENCH_PAGES, BenchPageId} from './data/benchmarks';
import {DRIVE_MODE_FRAMES, DriveMode} from './DriveMode';
import {PullSweep} from './PullSweep';
import {Theme} from './shared';
import {SHOWREEL_FRAMES, Showreel} from './Showreel';
import {SYNC_FRAMES, SyncOnlyChanged} from './SyncOnlyChanged';
import {TwoDrives} from './TwoDrives';
import {VolumeLayout} from './VolumeLayout';

// Every animation is registered twice, as `<Name>-light` and `<Name>-dark`. render.sh renders
// them all and turns them into assets/<name>-<theme>.gif.
const THEMES: Theme[] = ['light', 'dark'];

const SCENES: {id: string; component: React.FC<{theme: Theme}>; frames: number; height?: number}[] = [
  {id: 'DriveMode', component: DriveMode, frames: DRIVE_MODE_FRAMES},
  {id: 'ClusterSlack', component: ClusterSlack, frames: CLUSTER_SLACK_FRAMES},
  {id: 'SyncOnlyChanged', component: SyncOnlyChanged, frames: SYNC_FRAMES},
  {id: 'CopyVsBuild', component: CopyVsBuild, frames: 330},
  {id: 'VolumeLayout', component: VolumeLayout, frames: 240},
  {id: 'AddNoErase', component: AddNoErase, frames: 270},
  {id: 'PullSweep', component: PullSweep, frames: 330},
  {id: 'TwoDrives', component: TwoDrives, frames: 300},
];

// One chart per drive: BenchmarkChart-usb3, -ssd, -usb2 (the keys of BENCH_PAGES).
const benchId = (page: string) => `BenchmarkChart-${page}`;

export const RemotionRoot: React.FC = () => (
  <>
    {THEMES.flatMap((theme) => [
      ...SCENES.map((s) => (
        <Composition
          key={`${s.id}-${theme}`}
          id={`${s.id}-${theme}`}
          component={s.component}
          durationInFrames={s.frames}
          fps={30}
          width={1200}
          height={s.height ?? 600}
          defaultProps={{theme}}
        />
      )),
      ...(Object.keys(BENCH_PAGES) as BenchPageId[]).map((page) => (
        <Composition
          key={`${benchId(page)}-${theme}`}
          id={`${benchId(page)}-${theme}`}
          component={BenchmarkChart}
          durationInFrames={BENCH_FRAMES}
          fps={30}
          width={1200}
          height={640}
          defaultProps={{theme, page}}
        />
      )),
    ])}
    <Composition id="Showreel-dark" component={Showreel} durationInFrames={SHOWREEL_FRAMES} fps={30} width={1200} height={640} />
  </>
);
