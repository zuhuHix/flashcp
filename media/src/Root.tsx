import React from 'react';
import {Composition} from 'remotion';
import {BenchmarkChart} from './BenchmarkChart';
import {CopyVsBuild} from './CopyVsBuild';
import {Showreel} from './Showreel';
import {VolumeLayout} from './VolumeLayout';

export const RemotionRoot: React.FC = () => (
  <>
    <Composition
      id="CopyVsBuild-light"
      component={CopyVsBuild}
      durationInFrames={330}
      fps={30}
      width={1200}
      height={600}
      defaultProps={{theme: 'light' as const}}
    />
    <Composition
      id="CopyVsBuild-dark"
      component={CopyVsBuild}
      durationInFrames={330}
      fps={30}
      width={1200}
      height={600}
      defaultProps={{theme: 'dark' as const}}
    />
    <Composition
      id="BenchmarkChart-light"
      component={BenchmarkChart}
      durationInFrames={270}
      fps={30}
      width={1200}
      height={640}
      defaultProps={{theme: 'light' as const}}
    />
    <Composition
      id="BenchmarkChart-dark"
      component={BenchmarkChart}
      durationInFrames={270}
      fps={30}
      width={1200}
      height={640}
      defaultProps={{theme: 'dark' as const}}
    />
    <Composition
      id="VolumeLayout-light"
      component={VolumeLayout}
      durationInFrames={240}
      fps={30}
      width={1200}
      height={600}
      defaultProps={{theme: 'light' as const}}
    />
    <Composition
      id="VolumeLayout-dark"
      component={VolumeLayout}
      durationInFrames={240}
      fps={30}
      width={1200}
      height={600}
      defaultProps={{theme: 'dark' as const}}
    />
    <Composition
      id="Showreel-dark"
      component={Showreel}
      durationInFrames={840}
      fps={30}
      width={1200}
      height={640}
    />
  </>
);
