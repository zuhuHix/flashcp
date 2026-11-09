import React from 'react';
import {AbsoluteFill, Easing, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {FONT, MONO, Palette, PALETTES, Theme, ramp} from './shared';

// "two kinds of drives": 1200 x 600, 30 fps, 300 frames (10 s).
// The bars are schematic (not to scale): only the measured pills carry numbers.
const W = 1200;
const H = 600;
const BAR_X = 200;
const BAR_FULL = 700;
const FADE_A = 252;
const FADE_B = 299;

type Lane = {
  title: string;
  link: string;
  neckH: number;
  body: string;
  chipLabel: string;
  mult: string;
  times: string;
  oy: number;
  normalOv: number; // overhead segment, fraction of the full bar
  flashOv: number;
  chip: number; // chip time: the same in both bars
  normalStart: number;
  flashStart: number;
  pillStart: number;
};

const LANES: Lane[] = [
  {
    title: 'Cheap USB 2.0 stick (BOT)',
    link: 'USB 2.0 · BOT',
    neckH: 8,
    body: 'stick',
    chipLabel: 'measured: 6.5 MB/s sequential write',
    mult: '2.5×',
    times: '86.3 → 34.6 s',
    oy: 10,
    normalOv: 0.6,
    flashOv: 0.12,
    chip: 0.4,
    normalStart: 50,
    flashStart: 100,
    pillStart: 150,
  },
  {
    title: 'Fast USB SSD (UAS)',
    link: '10 Gb/s · UAS',
    neckH: 34,
    body: 'SSD',
    chipLabel: 'drive: 999 MB/s sequential writes (probe)',
    mult: '6.7×',
    times: '2.80 → 0.42 s',
    oy: 288,
    normalOv: 0.85,
    flashOv: 0.17,
    chip: 0.15,
    normalStart: 56,
    flashStart: 106,
    pillStart: 156,
  },
];

const LaneView: React.FC<{lane: Lane; f: number; P: Palette; fps: number; keep: number; intro: number}> = ({
  lane,
  f,
  P,
  fps,
  keep,
  intro,
}) => {
  const oy = lane.oy;
  const cy = oy + 87; // centre line of the link
  const neckX = 700;
  const normalP = ramp(f, lane.normalStart, lane.normalStart + 40, 0, 1, Easing.out(Easing.cubic));
  const flashP = ramp(f, lane.flashStart, lane.flashStart + 40, 0, 1, Easing.out(Easing.cubic));
  const pillScale =
    f < lane.pillStart
      ? 0
      : spring({frame: f - lane.pillStart, fps, config: {damping: 12, stiffness: 200, mass: 0.7}});
  const labelA = ramp(f, lane.normalStart + 30, lane.normalStart + 40) * keep;
  const flashLabelA = ramp(f, lane.flashStart + 30, lane.flashStart + 40) * keep;
  const subA = ramp(f, lane.pillStart + 6, lane.pillStart + 16) * keep;
  const ovN = lane.normalOv * BAR_FULL;
  const ovF = lane.flashOv * BAR_FULL;
  const chipW = lane.chip * BAR_FULL;
  const pillW = 110;
  const pillCx = 1085;

  return (
    <g>
      <g opacity={intro * keep}>
        <text x={60} y={oy + 34} fontSize={26} fontWeight={700} fill={P.text}>
          {lane.title}
        </text>

        {/* computer */}
        <rect x={60} y={oy + 60} width={140} height={52} rx={8} fill="none" stroke={P.muted} strokeWidth={2} />
        <text x={130} y={oy + 92} textAnchor="middle" fontSize={22} fill={P.text}>
          computer
        </text>

        {/* USB link: a pipe with a dashed flow line */}
        <rect x={200} y={oy + 70} width={500} height={34} rx={6} fill={P.muted} fillOpacity={0.18} />
        <line
          x1={200}
          y1={cy}
          x2={700}
          y2={cy}
          stroke={P.muted}
          strokeWidth={3}
          strokeDasharray="10 14"
          strokeDashoffset={-f * 1.2}
        />
        <text x={450} y={oy + 62} textAnchor="middle" fontSize={20} fill={P.muted}>
          {lane.link}
        </text>

        {/* flash chip: a neck on the stick, the full width on the SSD */}
        <rect x={neckX} y={cy - lane.neckH / 2} width={36} height={lane.neckH} fill={P.text} fillOpacity={0.8} />
        <rect x={736} y={oy + 56} width={124} height={62} rx={8} fill="none" stroke={P.muted} strokeWidth={2} />
        <text x={798} y={oy + 93} textAnchor="middle" fontSize={22} fill={P.text}>
          {lane.body}
        </text>
        <text x={718} y={oy + 140} textAnchor="middle" fontSize={20} fill={P.text}>
          {lane.chipLabel}
        </text>
      </g>

      {/* stacked schematic bars */}
      <g opacity={intro * keep}>
        <text x={60} y={oy + 188} fontSize={20} fontWeight={600} fill={P.coral}>
          normal copy
        </text>
        <text x={60} y={oy + 234} fontSize={20} fontWeight={600} fill={P.teal}>
          flashcp
        </text>
      </g>
      <g>
        <rect x={BAR_X} y={oy + 160} width={ovN * normalP} height={36} rx={4} fill={P.coral} opacity={keep} />
        <rect x={BAR_X + ovN * normalP} y={oy + 160} width={chipW * normalP} height={36} rx={4} fill={P.muted} opacity={keep} />
        <rect x={BAR_X} y={oy + 206} width={ovF * flashP} height={36} rx={4} fill={P.teal} opacity={keep} />
        <rect x={BAR_X + ovF * flashP} y={oy + 206} width={chipW * flashP} height={36} rx={4} fill={P.muted} opacity={keep} />
        <text
          x={BAR_X + 12}
          y={oy + 183}
          fontSize={18}
          fontWeight={600}
          fill={P.pillText}
          opacity={labelA}
        >
          per-file overhead
        </text>
        <text
          x={BAR_X + ovN + 12}
          y={oy + 183}
          fontSize={18}
          fontWeight={600}
          fill={P.bg}
          opacity={labelA}
        >
          chip time
        </text>
        <text
          x={BAR_X + ovF + 12}
          y={oy + 229}
          fontSize={18}
          fontWeight={600}
          fill={P.bg}
          opacity={flashLabelA}
        >
          chip time
        </text>
        <text x={BAR_X} y={oy + 256} fontSize={16} fill={P.muted} opacity={ramp(f, lane.normalStart, lane.normalStart + 10) * keep}>
          schematic, not to scale
        </text>
      </g>

      {/* measured result */}
      <g opacity={keep} transform={`translate(${pillCx} ${oy + 83}) scale(${pillScale}) translate(${-pillCx} ${-(oy + 83)})`}>
        <rect x={pillCx - pillW / 2} y={oy + 60} width={pillW} height={46} rx={23} fill={P.teal} />
        <text x={pillCx} y={oy + 90} textAnchor="middle" fontSize={26} fontWeight={700} fill={P.pillText}>
          {lane.mult}
        </text>
      </g>
      <text x={pillCx} y={oy + 132} textAnchor="middle" fontFamily={MONO} fontSize={20} fill={P.text} opacity={subA}>
        {lane.times}
      </text>
    </g>
  );
};

export const TwoDrives: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const intro = ramp(f, 0, 20);
  const captionA = ramp(f, 176, 192) * keep;

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        {LANES.map((lane) => (
          <LaneView key={lane.title} lane={lane} f={f} P={P} fps={fps} keep={keep} intro={intro} />
        ))}
        <text x={600} y={580} textAnchor="middle" fontSize={21} fill={P.text} opacity={captionA}>
          On a cheap stick the chip is the ceiling; on a fast drive the overhead is most of the time.
        </text>
      </svg>
    </AbsoluteFill>
  );
};
