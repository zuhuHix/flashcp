import React from 'react';
import {AbsoluteFill, Easing, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {FONT, MONO, PALETTES, Theme, ramp} from './shared';

// 1200 x 640, 30 fps, 270 frames (9 s).
const W = 1200;
const H = 640;
const X0 = 290; // x of 0 s
const PX = 71; // pixels per second: 9 s -> x = 929

type Group = {name: string; size: string; normal: number; flash: number; mult: string};
const GROUPS: Group[] = [
  {name: '20k files', size: '169 MB', normal: 2.8, flash: 0.42, mult: '6.7×'},
  {name: '100k files', size: '413 MB', normal: 8.9, flash: 0.86, mult: '10.3×'},
  {name: '8.4k mixed files', size: '1.4 GB', normal: 3.71, flash: 1.72, mult: '2.2×'},
];
const TOPS = [150, 290, 430];
const BAR_H = 40;
const BAR_GAP = 50;
const HOLD_END = 236;
const FADE_END = 266;

export const BenchmarkChart: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, HOLD_END, FADE_END);
  const ticks = [0, 3, 6, 9];

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <text x={60} y={62} fontSize={30} fontWeight={700} fill={P.text}>
          Time to fill a freshly formatted USB SSD (lower is better)
        </text>
        <rect x={60} y={86} width={18} height={18} rx={3} fill={P.coral} />
        <text x={88} y={101} fontSize={22} fill={P.text}>
          mkfs + cp
        </text>
        <rect x={250} y={86} width={18} height={18} rx={3} fill={P.teal} />
        <text x={278} y={101} fontSize={22} fill={P.text}>
          flashcp build
        </text>

        {ticks.map((t) => (
          <g key={t}>
            <line x1={X0 + t * PX} y1={130} x2={X0 + t * PX} y2={528} stroke={P.grid} strokeWidth={1.5} />
            <text x={X0 + t * PX} y={558} textAnchor="middle" fontSize={22} fill={P.muted}>
              {`${t} s`}
            </text>
          </g>
        ))}

        {GROUPS.map((g, gi) => {
          const top = TOPS[gi];
          const pillStart = 20 + gi * 24 + 10 + 60;
          const pillScale =
            f < pillStart ? 0 : spring({frame: f - pillStart, fps, config: {damping: 12, stiffness: 200, mass: 0.7}});
          const series = [
            {label: g.normal, color: P.coral, st: 20 + gi * 24},
            {label: g.flash, color: P.teal, st: 30 + gi * 24},
          ];
          return (
            <g key={g.name}>
              <text x={60} y={top + 38} fontSize={26} fontWeight={700} fill={P.text}>
                {g.name}
              </text>
              <text x={60} y={top + 68} fontSize={22} fill={P.muted}>
                {g.size}
              </text>
              {series.map((s, si) => {
                const p = ramp(f, s.st, s.st + 60, 0, 1, Easing.out(Easing.cubic));
                const len = s.label * PX * p;
                const y = top + 8 + si * BAR_GAP;
                const labelA = keep * ramp(f, s.st, s.st + 6);
                return (
                  <g key={si}>
                    <rect x={X0} y={y} width={len} height={BAR_H} rx={4} fill={s.color} opacity={keep} />
                    <text
                      x={X0 + len + 12}
                      y={y + 28}
                      fontFamily={MONO}
                      fontSize={24}
                      fill={P.text}
                      opacity={labelA}
                      style={{fontVariantNumeric: 'tabular-nums'}}
                    >
                      {`${(s.label * p).toFixed(2)} s`}
                    </text>
                  </g>
                );
              })}
              <g opacity={keep} transform={`translate(1110 ${top + 53}) scale(${pillScale}) translate(-1110 -${top + 53})`}>
                <rect x={1050} y={top + 27} width={120} height={52} rx={26} fill={P.teal} />
                <text x={1110} y={top + 61} textAnchor="middle" fontSize={28} fontWeight={700} fill={P.pillText}>
                  {g.mult}
                </text>
              </g>
            </g>
          );
        })}

        <text x={60} y={606} fontSize={22} fill={P.muted}>
          flashcp time includes formatting · baseline = mkfs.exfat + mount + cp -r + sync
        </text>
      </svg>
    </AbsoluteFill>
  );
};
