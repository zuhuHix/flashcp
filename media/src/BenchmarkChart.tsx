import React from 'react';
import {AbsoluteFill, Easing, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {FONT, MONO, Palette, PALETTES, Theme, ramp} from './shared';

// 1200 x 640, 30 fps, 390 frames (13 s). Two pages in sequence with an eased slide:
// the USB SSD, then the cheap USB 2.0 stick. Each page has its own seconds axis.
// Every number comes from the measured table (never the PR #9 provisional ones).
const W = 1200;
const H = 640;
const X0 = 300; // x of 0 s
const X_END = 930; // x of the longest bar on a page
const ROW_TOPS = [150, 290, 430];
const ROW_DELAY = [14, 50, 86]; // frames after the page's own start
const SLIDE_A = 196;
const SLIDE_B = 222;
const PAGE2_T0 = 222;
const FADE_A = 368;
const FADE_B = 389;

type Row = {name: string; sub: string; base: number; flash: number; baseLabel: string; flashLabel: string; pill: string};
type Page = {title: string; axisMax: number; ticks: number[]; rows: Row[]; note: string; t0: number};

const PAGES: Page[] = [
  {
    title: 'USB SSD (MSI DATAMAG 20Gbps, UAS) · 20k small files',
    axisMax: 6.01,
    ticks: [0, 2, 4, 6],
    rows: [
      {name: 'build', sub: 'erase + format', base: 2.8, flash: 0.42, baseLabel: '2.80', flashLabel: '0.42', pill: '6.7×'},
      {name: 'add', sub: 'existing drive', base: 1.72, flash: 0.48, baseLabel: '1.72', flashLabel: '0.48', pill: '3.6×'},
      {name: 'pull', sub: 'copy off the drive', base: 6.01, flash: 0.66, baseLabel: '6.01', flashLabel: '0.66', pill: '9.1×'},
    ],
    note: 'up to 10.3× (build, 100k files) and 26.8× (pull, 100k files, flashcp-built)',
    t0: 0,
  },
  {
    title: 'Cheap USB 2.0 stick (SanDisk Cruzer Blade, BOT) · 20k small files',
    axisMax: 86.3,
    ticks: [0, 20, 40, 60, 80],
    rows: [
      {name: 'build', sub: 'erase + format', base: 86.3, flash: 34.6, baseLabel: '86.3', flashLabel: '34.6', pill: '2.5×'},
      {name: 'pull', sub: 'kernel-filled stick', base: 30.86, flash: 29.52, baseLabel: '30.86', flashLabel: '29.52', pill: '1.05× on par'},
      {name: 'pull', sub: 'flashcp-built stick', base: 32.78, flash: 16.38, baseLabel: '32.78', flashLabel: '16.38', pill: '2.0×'},
    ],
    note: "the stick's flash chip is the ceiling (measured: 6.5 MB/s write, ≈11 MB/s read)",
    t0: PAGE2_T0,
  },
];

const decimalsOf = (label: string) => (label.includes('.') ? label.split('.')[1].length : 0);

const PageView: React.FC<{page: Page; f: number; P: Palette; fps: number}> = ({page, f: fAbs, P, fps}) => {
  const f = fAbs - page.t0;
  const px = (X_END - X0) / page.axisMax;
  return (
    <g>
      <text x={60} y={58} fontSize={28} fontWeight={700} fill={P.text}>
        {page.title}
      </text>
      <rect x={60} y={84} width={18} height={18} rx={3} fill={P.coral} />
      <text x={88} y={99} fontSize={22} fill={P.text}>
        normal copy (mount/mkfs + cp)
      </text>
      <rect x={470} y={84} width={18} height={18} rx={3} fill={P.teal} />
      <text x={498} y={99} fontSize={22} fill={P.text}>
        flashcp
      </text>

      {page.ticks.map((t) => (
        <g key={t}>
          <line x1={X0 + t * px} y1={124} x2={X0 + t * px} y2={516} stroke={P.grid} strokeWidth={1.5} />
          <text x={X0 + t * px} y={544} textAnchor="middle" fontSize={22} fill={P.muted}>
            {`${t} s`}
          </text>
        </g>
      ))}

      {page.rows.map((row, r) => {
        const top = ROW_TOPS[r];
        const s = ROW_DELAY[r];
        const coralP = ramp(f, s, s + 40, 0, 1, Easing.out(Easing.cubic));
        const tealP = ramp(f, s + 6, s + 46, 0, 1, Easing.out(Easing.cubic));
        const pillStart = s + 40;
        const pillScale =
          f < pillStart ? 0 : spring({frame: f - pillStart, fps, config: {damping: 12, stiffness: 200, mass: 0.7}});
        const pillW = Math.max(100, row.pill.length * 15 + 36);
        const pillCx = 1170 - pillW / 2;
        const pillCy = top + 40;
        const cDec = decimalsOf(row.baseLabel);
        const tDec = decimalsOf(row.flashLabel);
        const cLen = row.base * px * coralP;
        const tLen = row.flash * px * tealP;
        return (
          <g key={r}>
            <text x={60} y={top + 26} fontSize={26} fontWeight={700} fill={P.text}>
              {row.name}
            </text>
            <text x={60} y={top + 52} fontSize={18} fill={P.muted}>
              {row.sub}
            </text>

            <rect x={X0} y={top} width={cLen} height={36} rx={4} fill={P.coral} />
            <text
              x={X0 + cLen + 12}
              y={top + 26}
              fontFamily={MONO}
              fontSize={24}
              fill={P.text}
              opacity={ramp(f, s, s + 6)}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {`${(row.base * coralP).toFixed(cDec)} s`}
            </text>

            <rect x={X0} y={top + 44} width={tLen} height={36} rx={4} fill={P.teal} />
            <text
              x={X0 + tLen + 12}
              y={top + 70}
              fontFamily={MONO}
              fontSize={24}
              fill={P.text}
              opacity={ramp(f, s + 6, s + 12)}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {`${(row.flash * tealP).toFixed(tDec)} s`}
            </text>

            <g transform={`translate(${pillCx} ${pillCy}) scale(${pillScale}) translate(${-pillCx} ${-pillCy})`}>
              <rect x={pillCx - pillW / 2} y={pillCy - 26} width={pillW} height={52} rx={26} fill={P.teal} />
              <text x={pillCx} y={pillCy + 9} textAnchor="middle" fontSize={26} fontWeight={700} fill={P.pillText}>
                {row.pill}
              </text>
            </g>
          </g>
        );
      })}

      <text x={60} y={572} fontSize={22} fill={P.muted}>
        {page.note}
      </text>
      <text x={60} y={614} fontSize={22} fill={P.muted}>
        baselines include mkfs or mount/unmount · medians of 3 · Linux 7.0
      </text>
    </g>
  );
};

export const BenchmarkChart: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const fadeIn = ramp(f, 0, 12);
  const e = ramp(f, SLIDE_A, SLIDE_B, 0, 1, Easing.inOut(Easing.cubic));

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <g transform={`translate(${-W * e} 0)`} opacity={fadeIn * keep}>
          <PageView page={PAGES[0]} f={f} P={P} fps={fps} />
        </g>
        <g transform={`translate(${W * (1 - e)} 0)`} opacity={fadeIn * keep}>
          <PageView page={PAGES[1]} f={f} P={P} fps={fps} />
        </g>
      </svg>
    </AbsoluteFill>
  );
};
