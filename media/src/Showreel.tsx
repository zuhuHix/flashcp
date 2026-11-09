import React from 'react';
import {AbsoluteFill, Sequence} from 'remotion';
import {AddNoErase} from './AddNoErase';
import {BenchmarkChart} from './BenchmarkChart';
import {CopyVsBuild} from './CopyVsBuild';
import {PullSweep} from './PullSweep';
import {PALETTES} from './shared';
import {TwoDrives} from './TwoDrives';

// 1200 x 640 reel, dark theme only, about 54 s. Scenes are centred vertically in the frame.
// Order: copy -> add -> pull -> two drives -> benchmark. Each scene keeps its own duration.
const Scene: React.FC<{h: number; children: React.ReactNode}> = ({h, children}) => (
  <AbsoluteFill>
    <div style={{position: 'absolute', left: 0, top: (640 - h) / 2, width: 1200, height: h, overflow: 'hidden'}}>
      {children}
    </div>
  </AbsoluteFill>
);

export const Showreel: React.FC = () => (
  <AbsoluteFill style={{background: PALETTES.dark.bg}}>
    <Sequence from={0} durationInFrames={330}>
      <Scene h={600}>
        <CopyVsBuild theme="dark" />
      </Scene>
    </Sequence>
    <Sequence from={330} durationInFrames={270}>
      <Scene h={600}>
        <AddNoErase theme="dark" />
      </Scene>
    </Sequence>
    <Sequence from={600} durationInFrames={330}>
      <Scene h={600}>
        <PullSweep theme="dark" />
      </Scene>
    </Sequence>
    <Sequence from={930} durationInFrames={300}>
      <Scene h={600}>
        <TwoDrives theme="dark" />
      </Scene>
    </Sequence>
    <Sequence from={1230} durationInFrames={390}>
      <Scene h={640}>
        <BenchmarkChart theme="dark" />
      </Scene>
    </Sequence>
  </AbsoluteFill>
);
