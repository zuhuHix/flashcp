import React from 'react';
import {AbsoluteFill, Easing, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {FONT, MONO, Palette, PALETTES, Theme, fmtInt, mulberry32, ramp} from './shared';

// 1200 x 600, 30 fps, 330 frames (11 s).
const W = 1200;
const H = 600;
const COLS = 60;
const ROWS = 3;
const META_COLS = 7;
const META_CELLS = META_COLS * ROWS; // 21
const DATA_CELLS = (COLS - META_COLS) * ROWS; // 159
const GRID_X = 60;
const PITCH_X = 15;
const CELL_W = 12;
const CELL_H = 26;
const PITCH_Y = 32;
const GRID_H = 2 * PITCH_Y + CELL_H; // 90
const NORMAL_GY = 130;
const FLASH_GY = 370;
const TIMER_X = 1000;

// Timeline (frames at 30 fps). Animation time maps to real time at 0.35 s per animation second:
// 2.80 s / 8.0 s == 0.42 s / 1.2 s == 0.35.
const START = 12; // 0.40 s
const NORMAL_DUR = 240; // 8.0 s
const FLASH_DUR = 36; // 1.2 s
const NORMAL_END = START + NORMAL_DUR; // 252
const FLASH_END = START + FLASH_DUR; // 48
const FADE_A = 294; // 9.8 s
const FADE_B = 315; // 10.5 s
const NORMAL_REAL = 2.8;
const FLASH_REAL = 0.42;
const NCHUNK = 32;
const JUMP = 3;
const FLASH_LEN = 10;
const TRAIL_TAIL = 8;

type Pt = {x: number; y: number}; // lane-relative (y measured from grid top)
type Jump = {t0: number; t1: number; from: Pt; to: Pt};
type Write = {t0: number; t1: number; first: number; size: number};
type Flash = {t: number; m: number};

const cellPt = (col: number, row: number): Pt => ({x: GRID_X + col * PITCH_X, y: row * PITCH_Y});
const metaPt = (m: number): Pt => cellPt(Math.floor(m / ROWS), m % ROWS);
const dataCell = (d: number) => ({col: META_COLS + Math.floor(d / ROWS), row: d % ROWS});
const dataPt = (d: number): Pt => {
  const c = dataCell(d);
  return cellPt(c.col, c.row);
};

// Normal copy: 32 small "files" written in ascending order.
// Each group: head jumps (eased) to the group, writes it cell by cell, jumps back to a metadata cell
// (which flashes). Writes share the remaining frames proportionally to size.
const SCHED = (() => {
  const rnd = mulberry32(1307);
  const sizes: number[] = [];
  const offs: number[] = [];
  let acc = 0;
  for (let c = 0; c < NCHUNK; c++) {
    const s = Math.floor(DATA_CELLS / NCHUNK) + (c < DATA_CELLS % NCHUNK ? 1 : 0);
    sizes.push(s);
    offs.push(acc);
    acc += s;
  }
  const order = [...Array(NCHUNK).keys()];
  void rnd; // files land in ascending order, like a real allocator; the cost is the metadata round trip
  const perCell = (NORMAL_DUR - NCHUNK * 2 * JUMP) / DATA_CELLS;
  const jumps: Jump[] = [];
  const writes: Write[] = [];
  const flashes: Flash[] = [];
  let t = START;
  let head: Pt = metaPt(0);
  order.forEach((c, i) => {
    const first = offs[c];
    const size = sizes[c];
    const to = dataPt(first);
    jumps.push({t0: t, t1: t + JUMP, from: head, to});
    t += JUMP;
    const wd = size * perCell;
    writes.push({t0: t, t1: t + wd, first, size});
    t += wd;
    const m = (i * 5) % META_CELLS;
    const mp = metaPt(m);
    jumps.push({t0: t, t1: t + JUMP, from: dataPt(first + size - 1), to: mp});
    t += JUMP;
    flashes.push({t, m});
    head = mp;
  });
  return {jumps, writes, flashes, end: t};
})();

const writeCount = (w: Write, f: number) =>
  Math.min(w.size, Math.floor(((f - w.t0) / (w.t1 - w.t0)) * w.size) + 1);

type Overlay = {col: number; row: number; frac: number; op: number; color: string};

function normalState(f: number) {
  const filled = new Uint8Array(DATA_CELLS);
  for (const w of SCHED.writes) {
    if (f >= w.t1) {
      for (let k = 0; k < w.size; k++) filled[w.first + k] = 1;
    } else if (f >= w.t0) {
      const n = writeCount(w, f);
      for (let k = 0; k < n; k++) filled[w.first + k] = 1;
    }
  }

  let head: Pt = metaPt(0);
  for (const w of SCHED.writes) {
    if (f >= w.t0 && f < w.t1) head = dataPt(w.first + writeCount(w, f) - 1);
  }
  for (const j of SCHED.jumps) {
    if (f >= j.t0 && f < j.t1) {
      const p = ramp(f, j.t0, j.t1, 0, 1, Easing.inOut(Easing.cubic));
      head = {x: j.from.x + (j.to.x - j.from.x) * p, y: j.from.y + (j.to.y - j.from.y) * p};
    }
  }
  if (f >= NORMAL_END) head = SCHED.jumps[SCHED.jumps.length - 1].to;
  const headAlpha = ramp(f, START - 1, START, 0, 1) * ramp(f, NORMAL_END, NORMAL_END + 4, 1, 0);

  const flashes: {m: number; a: number}[] = [];
  for (const e of SCHED.flashes) {
    if (f >= e.t) flashes.push({m: e.m, a: Math.max(0.5, ramp(f, e.t, e.t + FLASH_LEN, 1, 0))});
  }

  // Motion trail: a faint line along the jump in progress, then it fades out.
  const trails: {from: Pt; to: Pt; a: number}[] = [];
  for (const j of SCHED.jumps) {
    if (f >= j.t0 && f < j.t1) trails.push({from: j.from, to: head, a: 0.5});
    else if (f >= j.t1 && f < j.t1 + TRAIL_TAIL) {
      trails.push({from: j.from, to: j.to, a: 0.5 * ramp(f, j.t1, j.t1 + TRAIL_TAIL, 1, 0)});
    }
  }
  return {filled, head, headAlpha, flashes, trails};
}

const Grid: React.FC<{gy: number; overlays: Overlay[]; P: Palette}> = ({gy, overlays, P}) => {
  const base: React.ReactNode[] = [];
  for (let col = 0; col < COLS; col++) {
    for (let row = 0; row < ROWS; row++) {
      const isMeta = col < META_COLS;
      base.push(
        <rect
          key={`b${col}-${row}`}
          x={GRID_X + col * PITCH_X}
          y={gy + row * PITCH_Y}
          width={CELL_W}
          height={CELL_H}
          rx={3}
          fill={isMeta ? P.amber : P.empty}
          fillOpacity={isMeta ? 0.2 : 1}
          stroke={isMeta ? P.amber : 'none'}
          strokeWidth={isMeta ? 1.5 : 0}
        />,
      );
    }
  }
  return (
    <g>
      {base}
      {overlays
        .filter((o) => o.op > 0.001 && o.frac > 0.001)
        .map((o, i) => (
          <rect
            key={`o${i}`}
            x={GRID_X + o.col * PITCH_X}
            y={gy + o.row * PITCH_Y}
            width={CELL_W * o.frac}
            height={CELL_H}
            rx={3}
            fill={o.color}
            opacity={o.op}
          />
        ))}
    </g>
  );
};

export const CopyVsBuild: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  // zero-state text: shown at the loop start, and only again once the live values have faded out
  const zeroA = (f < START ? 1 : 0) + ramp(f, FADE_B - 1, FADE_B);

  // Normal copy lane
  const n = normalState(f);
  const normalProg = ramp(f, START, NORMAL_END);
  const normalOverlays: Overlay[] = [];
  for (let d = 0; d < DATA_CELLS; d++) {
    if (n.filled[d]) {
      const c = dataCell(d);
      normalOverlays.push({col: c.col, row: c.row, frac: 1, op: keep, color: P.coral});
    }
  }
  for (const fl of n.flashes) {
    normalOverlays.push({
      col: Math.floor(fl.m / ROWS),
      row: fl.m % ROWS,
      frac: 1,
      op: fl.a * keep,
      color: P.amber,
    });
  }
  const badgeScale =
    f < NORMAL_END ? 0 : spring({frame: f - NORMAL_END, fps, config: {damping: 11, stiffness: 170, mass: 0.6}});

  // flashcp lane: one beam sweeps column by column, metadata columns first.
  const flashProg = ramp(f, START, FLASH_END);
  const headCol = ramp(f, START, FLASH_END, 0, COLS);
  const flashOverlays: Overlay[] = [];
  for (let col = 0; col < COLS; col++) {
    const frac = Math.min(1, Math.max(0, headCol - col));
    if (frac <= 0) continue;
    for (let row = 0; row < ROWS; row++) {
      flashOverlays.push({col, row, frac, op: keep, color: P.teal});
    }
  }
  const beamA = ramp(f, START, START + 2) * (1 - ramp(f, FLASH_END, FLASH_END + 3)) * keep;
  const beamX = GRID_X + headCol * PITCH_X;
  const chipScale =
    f < FLASH_END ? 0 : spring({frame: f - FLASH_END, fps, config: {damping: 14, stiffness: 190}});

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        {/* lane titles and sub-labels */}
        <text x={GRID_X} y={82} fontSize={28} fontWeight={700} fill={P.coral}>
          normal copy (mkfs + cp)
        </text>
        <text x={GRID_X} y={NORMAL_GY - 12} fontSize={22} fill={P.muted}>
          metadata
        </text>
        <text x={GRID_X + META_COLS * PITCH_X + 6} y={NORMAL_GY - 12} fontSize={22} fill={P.muted}>
          file data
        </text>

        <text x={GRID_X} y={322} fontSize={28} fontWeight={700} fill={P.teal}>
          flashcp build
        </text>
        <text x={GRID_X} y={FLASH_GY - 12} fontSize={22} fill={P.muted}>
          metadata
        </text>
        <text x={GRID_X + META_COLS * PITCH_X + 6} y={FLASH_GY - 12} fontSize={22} fill={P.muted}>
          file data
        </text>

        {/* normal lane: trails, grid, head */}
        {n.trails.map((t, i) => (
          <line
            key={`tr${i}`}
            x1={GRID_X + t.from.x + CELL_W / 2}
            y1={NORMAL_GY + t.from.y + CELL_H / 2}
            x2={GRID_X + t.to.x + CELL_W / 2}
            y2={NORMAL_GY + t.to.y + CELL_H / 2}
            stroke={P.coral}
            strokeWidth={2}
            strokeLinecap="round"
            opacity={t.a * keep}
          />
        ))}
        <Grid gy={NORMAL_GY} overlays={normalOverlays} P={P} />
        <g opacity={n.headAlpha * keep}>
          <rect
            x={GRID_X + n.head.x - 7}
            y={NORMAL_GY + n.head.y - 7}
            width={CELL_W + 14}
            height={CELL_H + 14}
            rx={6}
            fill={P.coral}
            fillOpacity={0.16}
          />
          <rect
            x={GRID_X + n.head.x - 4}
            y={NORMAL_GY + n.head.y - 4}
            width={CELL_W + 8}
            height={CELL_H + 8}
            rx={5}
            fill="none"
            stroke={P.coral}
            strokeWidth={3}
          />
        </g>

        {/* flashcp lane */}
        <Grid gy={FLASH_GY} overlays={flashOverlays} P={P} />
        <g opacity={beamA}>
          <rect x={beamX - 9} y={FLASH_GY - 8} width={18} height={GRID_H + 16} rx={9} fill={P.teal} fillOpacity={0.2} />
          <rect x={beamX - 1.5} y={FLASH_GY - 8} width={3} height={GRID_H + 16} rx={1.5} fill={P.teal} />
        </g>

        {/* timers and counters (cross-fade back to zero state at the loop point) */}
        <g>
          <text
            x={TIMER_X}
            y={NORMAL_GY + 44}
            fontFamily={MONO}
            fontSize={44}
            fontWeight={600}
            fill={P.coral}
            opacity={keep * ramp(f, NORMAL_END, NORMAL_END + 6)}
            style={{fontVariantNumeric: 'tabular-nums'}}
          >
            {`${NORMAL_REAL.toFixed(2)} s`}
          </text>
          <text
            x={TIMER_X}
            y={NORMAL_GY + 44}
            fontFamily={MONO}
            fontSize={44}
            fontWeight={600}
            fill={P.coral}
            opacity={zeroA}
            style={{fontVariantNumeric: 'tabular-nums'}}
          >
          </text>
          <text x={TIMER_X} y={NORMAL_GY + 80} fontSize={22} fill={P.muted} opacity={keep * ramp(f, NORMAL_END, NORMAL_END + 6)} style={{fontVariantNumeric: 'tabular-nums'}}>
            {`${fmtInt(20000)} files`}
          </text>
          <text x={TIMER_X} y={NORMAL_GY + 80} fontSize={22} fill={P.muted} opacity={zeroA} style={{fontVariantNumeric: 'tabular-nums'}}>
          </text>
        </g>
        <g>
          <text
            x={TIMER_X}
            y={FLASH_GY + 44}
            fontFamily={MONO}
            fontSize={44}
            fontWeight={600}
            fill={P.teal}
            opacity={keep * ramp(f, FLASH_END, FLASH_END + 6)}
            style={{fontVariantNumeric: 'tabular-nums'}}
          >
            {`${FLASH_REAL.toFixed(2)} s`}
          </text>
          <text
            x={TIMER_X}
            y={FLASH_GY + 44}
            fontFamily={MONO}
            fontSize={44}
            fontWeight={600}
            fill={P.teal}
            opacity={zeroA}
            style={{fontVariantNumeric: 'tabular-nums'}}
          >
          </text>
          <text x={TIMER_X} y={FLASH_GY + 80} fontSize={22} fill={P.muted} opacity={keep * ramp(f, FLASH_END, FLASH_END + 6)} style={{fontVariantNumeric: 'tabular-nums'}}>
            {`${fmtInt(20000)} files`}
          </text>
          <text x={TIMER_X} y={FLASH_GY + 80} fontSize={22} fill={P.muted} opacity={zeroA} style={{fontVariantNumeric: 'tabular-nums'}}>
          </text>
        </g>

        {/* done chip for flashcp */}
        <g opacity={keep} transform={`translate(1050 316) scale(${chipScale}) translate(-1050 -316)`}>
          <rect x={990} y={299} width={120} height={34} rx={17} fill={P.teal} />
          <text x={1050} y={323} textAnchor="middle" fontSize={22} fontWeight={700} fill={P.pillText}>
            done ✓
          </text>
        </g>

        {/* speed-up badge, centred between the lanes */}
        <g opacity={keep} transform={`translate(600 262) scale(${badgeScale}) translate(-600 -262)`}>
          <rect x={475} y={234} width={250} height={56} rx={28} fill={P.teal} />
          <text x={600} y={272} textAnchor="middle" fontSize={30} fontWeight={700} fill={P.pillText}>
            6.7× faster
          </text>
        </g>

        <text x={600} y={540} textAnchor="middle" fontSize={22} fill={P.muted}>
          20,000 files · 169 MB · measured on a USB SSD, flashcp time includes formatting
        </text>
      </svg>
    </AbsoluteFill>
  );
};
