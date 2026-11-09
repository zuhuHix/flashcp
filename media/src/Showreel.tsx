import React from 'react';
import {AbsoluteFill, Sequence} from 'remotion';
import {BenchmarkChart} from './BenchmarkChart';
import {CopyVsBuild} from './CopyVsBuild';
import {PALETTES} from './shared';
import {VolumeLayout} from './VolumeLayout';

// 1200 x 640 reel, dark theme only. Scenes are centred vertically in the frame.
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
      <Scene h={640}>
        <BenchmarkChart theme="dark" />
      </Scene>
    </Sequence>
    <Sequence from={600} durationInFrames={240}>
      <Scene h={600}>
        <VolumeLayout theme="dark" />
      </Scene>
    </Sequence>
  </AbsoluteFill>
);
