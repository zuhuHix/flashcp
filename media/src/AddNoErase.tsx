import React from 'react';
import {AbsoluteFill, Easing, useCurrentFrame} from 'remotion';
import {FONT, PALETTES, Palette, Theme, mulberry32, ramp} from './shared';

// "add without erasing": 1200 x 600, 30 fps, 270 frames (9 s).
// Existing files are drawn at a constant colour for the whole loop. New files are written in one
// ascending pass into free cells only. Then the metadata updates flash in order: data, bitmap + FAT,
// folder entries.
const W = 1200;
const H = 600;
const DATA_COLS = 30;
const ROWS = 4;
const DATA_X = 252;
const PITCH_X = 30;
const CELL_W = 24;
const GY = 172;
const PITCH_Y = 56;
const CELL_H = 46;
const DATA_END = DATA_X + (DATA_COLS - 1) * PITCH_X + CELL_W;
const META_X = [60, 122, 184]; // bitmap, FAT, folder entries: 2 columns x 4 rows each
const META_NAMES = ['bitmap', 'FAT', 'folder'];
const META_PX = 24;
const META_CW = 20;
const WRITE_A = 22;
const WRITE_B = 140;
const META_START = [146, 146, 178]; // bitmap + FAT together (phase 2), then folder entries (phase 3)
const STEP_START = [WRITE_A, META_START[0], META_START[2]];
const DONE_A = 200;
const FADE_A = 240;
const FADE_B = 269;

const N_CELLS = DATA_COLS * ROWS;
const cellCol = (p: number) => Math.floor(p / ROWS);
const cellRow = (p: number) => p % ROWS;
const cellX = (p: number) => DATA_X + cellCol(p) * PITCH_X;
const cellY = (p: number) => GY + cellRow(p) * PITCH_Y;

// Existing files: runs of occupied cells (column-major). Constant colour, never touched.
const existing: boolean[] = [];
{
  const rnd = mulberry32(7);
  let occ = rnd() < 0.5;
  for (let p = 0; p < N_CELLS; p++) {
    if (rnd() < 0.18) occ = !occ;
    existing.push(occ);
  }
}
// New files: about 60% of the free cells, in ascending order.
const written: boolean[] = [];
{
  const rnd = mulberry32(99);
  for (let p = 0; p < N_CELLS; p++) written.push(!existing[p] && rnd() < 0.6);
}

export const AddNoErase: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const P: Palette = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);

  // one ascending write pass: the head moves left to right, cells fill only when it reaches free space
  const hx = ramp(f, WRITE_A, WRITE_B, DATA_X - 10, DATA_END + 10, Easing.bezier(0.3, 0, 0.7, 1));
  const headA = ramp(f, WRITE_A - 2, WRITE_A) * (1 - ramp(f, WRITE_B, WRITE_B + 3)) * keep;

  const metaOp = (start: number) =>
    ramp(f, start, start + 4) * (1 - 0.6 * ramp(f, start + 4, start + 16)) * keep;

  const nodeOn = (start: number) => ramp(f, start, start + 6) * keep;
  const doneA = ramp(f, DONE_A, DONE_A + 14) * keep;

  const cells: React.ReactNode[] = [];
  for (let p = 0; p < N_CELLS; p++) {
    cells.push(
      <rect key={`b${p}`} x={cellX(p)} y={cellY(p)} width={CELL_W} height={CELL_H} rx={3} fill={P.empty} />,
    );
  }
  for (let p = 0; p < N_CELLS; p++) {
    if (existing[p]) {
      cells.push(
        <rect
          key={`e${p}`}
          x={cellX(p)}
          y={cellY(p)}
          width={CELL_W}
          height={CELL_H}
          rx={3}
          fill={P.muted}
          fillOpacity={0.4}
        />,
      );
    }
  }
  for (let p = 0; p < N_CELLS; p++) {
    if (written[p]) {
      cells.push(
        <rect
          key={`w${p}`}
          x={cellX(p)}
          y={cellY(p)}
          width={CELL_W}
          height={CELL_H}
          rx={3}
          fill={P.teal}
          opacity={ramp(hx, cellX(p) - 2, cellX(p) + 4) * keep}
        />,
      );
    }
  }

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <text x={60} y={44} fontSize={28} fontWeight={700} fill={P.text}>
          add without erasing
        </text>

        {/* step indicator */}
        <line x1={170} y1={96} x2={402} y2={96} stroke={P.grid} strokeWidth={2} />
        <line x1={620} y1={96} x2={766} y2={96} stroke={P.grid} strokeWidth={2} />
        {[
          {x: 60, label: '1 data'},
          {x: 420, label: '2 bitmap + FAT'},
          {x: 780, label: '3 folder entries'},
        ].map((s, i) => (
          <g key={s.label}>
            <circle cx={s.x + 12} cy={96} r={12} fill={P.bg} stroke={P.muted} strokeWidth={2} />
            <circle cx={s.x + 12} cy={96} r={12} fill={P.teal} opacity={nodeOn(STEP_START[i])} />
            <text x={s.x + 34} y={102} fontSize={22} fontWeight={600} fill={P.muted}>
              {s.label}
            </text>
          </g>
        ))}

        <text x={DATA_X} y={GY - 14} fontSize={20} fill={P.muted}>
          existing files stay untouched
        </text>

        {/* metadata zone (left): amber cells that flash in order */}
        {META_X.map((mx, g) => (
          <g key={`m${g}`}>
            {Array.from({length: 8}, (_, k) => {
              const col = k % 2;
              const row = Math.floor(k / 2);
              return (
                <g key={k}>
                  <rect
                    x={mx + col * META_PX}
                    y={GY + row * PITCH_Y}
                    width={META_CW}
                    height={CELL_H}
                    rx={3}
                    fill={P.amber}
                    fillOpacity={0.2}
                    stroke={P.amber}
                    strokeWidth={1.5}
                  />
                  <rect
                    x={mx + col * META_PX}
                    y={GY + row * PITCH_Y}
                    width={META_CW}
                    height={CELL_H}
                    rx={3}
                    fill={P.amber}
                    opacity={metaOp(META_START[g])}
                  />
                </g>
              );
            })}
            <text x={mx + 22} y={GY + 3 * PITCH_Y + CELL_H + 22} textAnchor="middle" fontSize={18} fill={P.muted}>
              {META_NAMES[g]}
            </text>
          </g>
        ))}

        {/* data zone */}
        {cells}
        <g opacity={headA}>
          <line x1={hx} y1={GY - 8} x2={hx} y2={GY + 3 * PITCH_Y + CELL_H + 8} stroke={P.text} strokeWidth={3} strokeLinecap="round" />
        </g>

        {/* legend */}
        <rect x={60} y={460} width={18} height={18} rx={3} fill={P.muted} fillOpacity={0.4} />
        <text x={88} y={475} fontSize={20} fill={P.text}>
          existing files (never changed)
        </text>
        <rect x={380} y={460} width={18} height={18} rx={3} fill={P.teal} />
        <text x={408} y={475} fontSize={20} fill={P.text}>
          new files, written into free space only
        </text>
        <rect x={760} y={460} width={18} height={18} rx={3} fill={P.amber} />
        <text x={788} y={475} fontSize={20} fill={P.text}>
          metadata cells
        </text>

        <text x={600} y={534} textAnchor="middle" fontSize={26} fontWeight={700} fill={P.teal} opacity={doneA}>
          done · 3.6× faster than mount + cp (USB SSD, 20k files)
        </text>
      </svg>
    </AbsoluteFill>
  );
};
