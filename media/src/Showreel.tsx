import React from 'react';
import {AbsoluteFill, Sequence} from 'remotion';
import {BENCH_FRAMES, BenchmarkChart} from './BenchmarkChart';
import {CLUSTER_SLACK_FRAMES, ClusterSlack} from './ClusterSlack';
import {CopyVsBuild} from './CopyVsBuild';
import {DRIVE_MODE_FRAMES, DriveMode} from './DriveMode';
import {PullSweep} from './PullSweep';
import {PALETTES} from './shared';
import {SYNC_FRAMES, SyncOnlyChanged} from './SyncOnlyChanged';

// 1200 x 640 reel for the Pages site, dark theme only, no audio. Scenes are centred vertically
// and each keeps its own duration:
// drive mode -> cluster size -> sync -> build -> pull -> the USB 3 stick's chart.
const SCENES: {h: number; frames: number; node: React.ReactNode}[] = [
  {h: 600, frames: DRIVE_MODE_FRAMES, node: <DriveMode theme="dark" />},
  {h: 600, frames: CLUSTER_SLACK_FRAMES, node: <ClusterSlack theme="dark" />},
  {h: 600, frames: SYNC_FRAMES, node: <SyncOnlyChanged theme="dark" />},
  {h: 600, frames: 330, node: <CopyVsBuild theme="dark" />},
  {h: 600, frames: 330, node: <PullSweep theme="dark" />},
  {h: 640, frames: BENCH_FRAMES, node: <BenchmarkChart theme="dark" page="usb3" />},
];

export const SHOWREEL_FRAMES = SCENES.reduce((a, s) => a + s.frames, 0);

export const Showreel: React.FC = () => {
  let from = 0;
  return (
    <AbsoluteFill style={{background: PALETTES.dark.bg}}>
      {SCENES.map((s, i) => {
        const start = from;
        from += s.frames;
        return (
          <Sequence key={i} from={start} durationInFrames={s.frames}>
            <AbsoluteFill>
              <div style={{position: 'absolute', left: 0, top: (640 - s.h) / 2, width: 1200, height: s.h, overflow: 'hidden'}}>
                {s.node}
              </div>
            </AbsoluteFill>
          </Sequence>
        );
      })}
    </AbsoluteFill>
  );
};
