import React from 'react';
import {AbsoluteFill, Easing, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {FONT, MONO, Palette, PALETTES, Theme, fmtInt, mulberry32, ramp} from './shared';

// "copy off the drive": 1200 x 600, 30 fps, 330 frames (11 s).
// Timers share one time scale: the mount + cp lane spans NORMAL_DUR frames for 6.01 s, so the
// flashcp lane (plan + sweep) spans FLASH_DUR = NORMAL_DUR * 0.66 / 6.01 frames for 0.66 s.
const W = 1200;
const H = 600;
const N = 30; // files drawn (schematic)
const COLS = 26;
const ROWS = 3;
const GRID_X = 60;
const PITCH_X = 34;
const CELL_W = 26;
const CELL_H = 24;
const PITCH_Y = 30;
const GRID_W = (COLS - 1) * PITCH_X + CELL_W; // 876
const NORMAL_GY = 112;
const FLASH_GY = 332;
const SLOT_W = GRID_W / N;
const TILE_W = 20;
const TILE_H = 24;
const NORMAL_DEST_Y = 242;
const FLASH_DEST_Y = 462;
const TIMER_X = 960;

const START = 12;
const OUT = 3; // travel to the file (eased)
const READ = 2; // head moves across the file's cells
const BACK = 3; // travel back to the file's slot in the destination
const PAUSE = 1;
const PER = OUT + READ + BACK + PAUSE; // 9 frames per file
const NORMAL_DUR = N * PER; // 270 frames = 6.01 s
const FLASH_DUR = Math.round((NORMAL_DUR * 0.66) / 6.01); // 30 frames = 0.66 s
const PLAN = 7;
const SWEEP_A = START + PLAN;
const SWEEP_B = START + FLASH_DUR;
const FADE_A = 306;
const FADE_B = 329;

type Pt = {x: number; y: number};
type Ext = {p0: number; size: number};

// Physical layout: files sit in cells in column-major order, with a few gaps.
const EXTS: Ext[] = [];
{
  const rnd = mulberry32(2024);
  let p = 0;
  for (let k = 0; k < N; k++) {
    p += rnd() < 0.3 ? 1 : 0;
    const size = 1 + (rnd() < 0.45 ? 1 : 0) + (rnd() < 0.15 ? 1 : 0);
    EXTS.push({p0: p, size});
    p += size;
  }
  if (p > COLS * ROWS) throw new Error('PullSweep: files do not fit the grid');
}

// Name order: name index i reads physical rank NAME_TO_RANK[i]. Shuffled, so the reads hop around.
const NAME_TO_RANK: number[] = [...Array(N).keys()];
{
  const rnd = mulberry32(77);
  for (let i = N - 1; i > 0; i--) {
    const j = Math.floor(rnd() * (i + 1));
    [NAME_TO_RANK[i], NAME_TO_RANK[j]] = [NAME_TO_RANK[j], NAME_TO_RANK[i]];
  }
}
const RANK_TO_NAME: number[] = [];
NAME_TO_RANK.forEach((r, i) => (RANK_TO_NAME[r] = i));

const isFileCell: boolean[] = Array(COLS * ROWS).fill(false);
EXTS.forEach((e) => {
  for (let k = 0; k < e.size; k++) isFileCell[e.p0 + k] = true;
});

const cellCol = (p: number) => Math.floor(p / ROWS);
const cellRow = (p: number) => p % ROWS;
const cellLeft = (p: number) => GRID_X + cellCol(p) * PITCH_X;
const cellCenter = (p: number, gy: number): Pt => ({
  x: cellLeft(p) + CELL_W / 2,
  y: gy + cellRow(p) * PITCH_Y + CELL_H / 2,
});
const slotX = (i: number) => GRID_X + i * SLOT_W + (SLOT_W - TILE_W) / 2;
const slotCenter = (i: number, y: number): Pt => ({x: slotX(i) + TILE_W / 2, y: y + TILE_H / 2});
const HOME: Pt = {x: GRID_X - 6, y: NORMAL_GY + 12};

const lerpPt = (a: Pt, b: Pt, p: number): Pt => ({x: a.x + (b.x - a.x) * p, y: a.y + (b.y - a.y) * p});
const HOP = Easing.inOut(Easing.cubic);

// Normal lane: per file, travel out, read its cells, travel back and drop it in its slot.
type Leg = {t0: number; t1: number; from: Pt; to: Pt; travel: boolean};
const NORMAL_LEGS: Leg[] = [];
const NORMAL_CELLS: {p: number; a: number; b: number}[] = [];
const NORMAL_LAND: number[] = [];
{
  let prev: Pt = HOME;
  for (let i = 0; i < N; i++) {
    const ext = EXTS[NAME_TO_RANK[i]];
    const t = START + i * PER;
    const a = cellCenter(ext.p0, NORMAL_GY);
    const b = cellCenter(ext.p0 + ext.size - 1, NORMAL_GY);
    const slot = slotCenter(i, NORMAL_DEST_Y);
    NORMAL_LEGS.push({t0: t, t1: t + OUT, from: prev, to: a, travel: true});
    NORMAL_LEGS.push({t0: t + OUT, t1: t + OUT + READ, from: a, to: b, travel: false});
    NORMAL_LEGS.push({t0: t + OUT + READ, t1: t + OUT + READ + BACK, from: b, to: slot, travel: true});
    for (let k = 0; k < ext.size; k++) {
      NORMAL_CELLS.push({
        p: ext.p0 + k,
        a: t + OUT + (k * READ) / ext.size,
        b: t + OUT + ((k + 1) * READ) / ext.size,
      });
    }
    NORMAL_LAND[i] = t + OUT + READ + BACK;
    prev = slot;
  }
}

function normalHead(f: number): Pt {
  let pos = HOME;
  for (const leg of NORMAL_LEGS) {
    if (f >= leg.t1) {
      pos = leg.to;
      continue;
    }
    if (f >= leg.t0) {
      const p = leg.travel ? ramp(f, leg.t0, leg.t1, 0, 1, HOP) : ramp(f, leg.t0, leg.t1);
      return lerpPt(leg.from, leg.to, p);
    }
    break;
  }
  return pos;
}

// Flashcp lane: one eased sweep left to right over the physical columns.
const sweepX = (f: number) => GRID_X - 10 + ramp(f, SWEEP_A, SWEEP_B, 0, GRID_W + 20, Easing.inOut(Easing.cubic));
const FLASH_LAND: number[] = []; // by rank: frame the file lands
{
  for (let r = 0; r < N; r++) {
    const ext = EXTS[r];
    const xr = cellLeft(ext.p0 + ext.size - 1) + CELL_W;
    let land = SWEEP_B;
    for (let f = SWEEP_A; f <= SWEEP_B; f++) {
      if (sweepX(f) >= xr) {
        land = f;
        break;
      }
    }
    FLASH_LAND[r] = land;
  }
}

const Grid: React.FC<{gy: number; P: Palette; children?: React.ReactNode}> = ({gy, P, children}) => {
  const cells: React.ReactNode[] = [];
  for (let p = 0; p < COLS * ROWS; p++) {
    cells.push(
      <rect
        key={`c${p}`}
        x={cellLeft(p)}
        y={gy + cellRow(p) * PITCH_Y}
        width={CELL_W}
        height={CELL_H}
        rx={3}
        fill={isFileCell[p] ? P.muted : P.empty}
        fillOpacity={isFileCell[p] ? 0.45 : 1}
      />,
    );
  }
  return (
    <g>
      {cells}
      {children}
    </g>
  );
};

export const PullSweep: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  // zero state: shown at the loop start, and again once the live values have faded out
  const zeroA = (f < START ? 1 : 0) + ramp(f, FADE_B - 1, FADE_B);
  const live = f >= START ? keep : 0; // live values only once the run starts (no double draw at frame 0)

  // normal lane
  const nhPos = normalHead(f);
  const normalProg = ramp(f, START, START + NORMAL_DUR);
  const headA = ramp(f, START - 1, START) * (1 - ramp(f, START + NORMAL_DUR, START + NORMAL_DUR + 4)) * keep;

  // flashcp lane
  const flashProg = ramp(f, START, START + FLASH_DUR);
  const hx = sweepX(f);
  const planA = ramp(f, START, START + PLAN - 2) * keep;
  const beamA = ramp(f, SWEEP_A - 1, SWEEP_A) * (1 - ramp(f, SWEEP_B, SWEEP_B + 3)) * keep;
  const chipScale =
    f < SWEEP_B ? 0 : spring({frame: f - SWEEP_B, fps, config: {damping: 12, stiffness: 200, mass: 0.7}});

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <text x={60} y={44} fontSize={28} fontWeight={700} fill={P.text}>
          copy off the drive
        </text>

        {/* normal lane */}
        <text x={60} y={92} fontSize={26} fontWeight={700} fill={P.coral}>
          mount + cp
        </text>
        <text x={210} y={92} fontSize={20} fill={P.muted}>
          one round trip per file
        </text>
        <Grid gy={NORMAL_GY} P={P}>
          {NORMAL_CELLS.map((c) => (
            <rect
              key={`n${c.p}-${c.a}`}
              x={cellLeft(c.p)}
              y={NORMAL_GY + cellRow(c.p) * PITCH_Y}
              width={CELL_W}
              height={CELL_H}
              rx={3}
              fill={P.coral}
              opacity={ramp(f, c.a, c.b) * keep}
            />
          ))}
        </Grid>
        <g opacity={headA}>
          <rect x={nhPos.x - 8} y={nhPos.y - 8} width={CELL_W + 4} height={CELL_H + 4} rx={6} fill={P.coral} fillOpacity={0.16} />
          <rect x={nhPos.x - 8} y={nhPos.y - 8} width={CELL_W + 4} height={CELL_H + 4} rx={6} fill="none" stroke={P.coral} strokeWidth={2.5} />
        </g>
        <text x={GRID_X} y={NORMAL_DEST_Y - 10} fontSize={20} fill={P.muted}>
          destination, name order
        </text>
        {Array.from({length: N}, (_, i) => (
          <rect
            key={`ns${i}`}
            x={slotX(i)}
            y={NORMAL_DEST_Y}
            width={TILE_W}
            height={TILE_H}
            rx={3}
            fill={P.empty}
          />
        ))}
        {Array.from({length: N}, (_, i) => (
          <rect
            key={`nt${i}`}
            x={slotX(i)}
            y={NORMAL_DEST_Y}
            width={TILE_W}
            height={TILE_H}
            rx={3}
            fill={P.coral}
            opacity={ramp(f, NORMAL_LAND[i], NORMAL_LAND[i] + 2) * keep}
          />
        ))}

        {/* flashcp lane */}
        <text x={60} y={312} fontSize={26} fontWeight={700} fill={P.teal}>
          flashcp pull
        </text>
        <text x={230} y={312} fontSize={20} fill={P.muted}>
          plan, then one sweep in physical order
        </text>
        <Grid gy={FLASH_GY} P={P}>
          {EXTS.flatMap((e) =>
            Array.from({length: e.size}, (_, k) => {
              const p = e.p0 + k;
              const cl = cellLeft(p);
              return (
                <g key={`fx${p}`}>
                  <rect
                    x={cl}
                    y={FLASH_GY + cellRow(p) * PITCH_Y}
                    width={CELL_W}
                    height={CELL_H}
                    rx={3}
                    fill={P.teal}
                    opacity={ramp(hx, cl, cl + CELL_W) * keep}
                  />
                  <rect
                    x={cl - 1.5}
                    y={FLASH_GY + cellRow(p) * PITCH_Y - 1.5}
                    width={CELL_W + 3}
                    height={CELL_H + 3}
                    rx={4}
                    fill="none"
                    stroke={P.teal}
                    strokeWidth={2}
                    opacity={planA * (1 - ramp(hx, cl, cl + CELL_W))}
                  />
                </g>
              );
            }),
          )}
        </Grid>
        <g opacity={beamA}>
          <rect x={hx - 9} y={FLASH_GY - 8} width={18} height={3 * PITCH_Y + CELL_H + 16} rx={9} fill={P.teal} fillOpacity={0.2} />
          <rect x={hx - 1.5} y={FLASH_GY - 8} width={3} height={3 * PITCH_Y + CELL_H + 16} rx={1.5} fill={P.teal} />
        </g>
        <text x={GRID_X} y={FLASH_DEST_Y - 10} fontSize={20} fill={P.muted}>
          destination, name order (filled as read)
        </text>
        {Array.from({length: N}, (_, i) => (
          <rect key={`fs${i}`} x={slotX(i)} y={FLASH_DEST_Y} width={TILE_W} height={TILE_H} rx={3} fill={P.empty} />
        ))}
        {Array.from({length: N}, (_, r) => (
          <rect
            key={`ft${r}`}
            x={slotX(RANK_TO_NAME[r])}
            y={FLASH_DEST_Y}
            width={TILE_W}
            height={TILE_H}
            rx={3}
            fill={P.teal}
            opacity={ramp(f, FLASH_LAND[r], FLASH_LAND[r] + 2) * keep}
          />
        ))}

        {/* timers (cross-fade back to zero state at the loop point) */}
        <text
          x={TIMER_X}
          y={NORMAL_GY + 44}
          fontFamily={MONO}
          fontSize={40}
          fontWeight={600}
          fill={P.coral}
          opacity={live * ramp(normalProg, 0.98, 1)}
          style={{fontVariantNumeric: 'tabular-nums'}}
        >
          {'6.01 s'}
        </text>
        <text
          x={TIMER_X}
          y={NORMAL_GY + 44}
          fontFamily={MONO}
          fontSize={40}
          fontWeight={600}
          fill={P.coral}
          opacity={zeroA}
          style={{fontVariantNumeric: 'tabular-nums'}}
        >
        </text>
        <text x={TIMER_X} y={NORMAL_GY + 80} fontSize={22} fill={P.muted} opacity={live * ramp(normalProg, 0.98, 1)} style={{fontVariantNumeric: 'tabular-nums'}}>
          {`${fmtInt(20000)} files`}
        </text>
        <text x={TIMER_X} y={NORMAL_GY + 80} fontSize={22} fill={P.muted} opacity={zeroA} style={{fontVariantNumeric: 'tabular-nums'}}>
        </text>

        <text
          x={TIMER_X}
          y={FLASH_GY + 44}
          fontFamily={MONO}
          fontSize={40}
          fontWeight={600}
          fill={P.teal}
          opacity={live * ramp(flashProg, 0.98, 1)}
          style={{fontVariantNumeric: 'tabular-nums'}}
        >
          {'0.66 s'}
        </text>
        <text
          x={TIMER_X}
          y={FLASH_GY + 44}
          fontFamily={MONO}
          fontSize={40}
          fontWeight={600}
          fill={P.teal}
          opacity={zeroA}
          style={{fontVariantNumeric: 'tabular-nums'}}
        >
        </text>
        <text x={TIMER_X} y={FLASH_GY + 80} fontSize={22} fill={P.muted} opacity={live * ramp(flashProg, 0.98, 1)} style={{fontVariantNumeric: 'tabular-nums'}}>
          {`${fmtInt(20000)} files`}
        </text>
        <text x={TIMER_X} y={FLASH_GY + 80} fontSize={22} fill={P.muted} opacity={zeroA} style={{fontVariantNumeric: 'tabular-nums'}}>
        </text>

        {/* speed-up pill at the end of the flashcp run */}
        <g opacity={keep} transform={`translate(1010 451) scale(${chipScale}) translate(-1010 -451)`}>
          <rect x={960} y={428} width={100} height={46} rx={23} fill={P.teal} />
          <text x={1010} y={458} textAnchor="middle" fontSize={26} fontWeight={700} fill={P.pillText}>
            9.1×
          </text>
        </g>

        <text x={60} y={540} fontSize={22} fill={P.muted}>
          measured on a USB SSD, 20k files
        </text>
        <text x={60} y={566} fontSize={18} fill={P.muted}>
          grid and file count are schematic; the motion is scaled to the measured times
        </text>
      </svg>
    </AbsoluteFill>
  );
};
