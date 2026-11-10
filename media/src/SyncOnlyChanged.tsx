import React from 'react';
import {AbsoluteFill, useCurrentFrame} from 'remotion';
import {DATASET, SYNC} from './data/benchmarks';
import {bump, clamp01, lerp} from './motion';
import {EASE_IO, EASE_OUT, FONT, MONO, PALETTES, Theme, fmtInt, mulberry32, ramp} from './shared';

// 1200 x 600, 30 fps, 348 frames (11.6 s).
//
// `flashcp sync`: a repeat copy moves only what changed. Two grids of 200 squares (one square =
// 100 of the 20,000 files): the folder on this computer and the copy on the stick. Three repeat
// runs: a scan compares them, the changed squares light up and only those cross over. Each run
// adds its measured time as a bar; the last bar is what rewriting every file costs.
const W = 1200;
const H = 600;
export const SYNC_FRAMES = 348;

const COLS = 25;
const ROWS = 8;
const TILE = 14;
const PITCH = 17;
const GRID_W = COLS * PITCH - 3; // 422
const GRID_H = ROWS * PITCH - 3; // 133
const LX = 60;
const RX = W - 60 - GRID_W; // 718
const GY = 84;
const FILES_PER_TILE = DATASET.files / (COLS * ROWS); // 100

// Which squares change in each run: a fixed shuffle, so every render picks the same ones.
const ORDER = (() => {
  const rnd = mulberry32(2026);
  const idx = Array.from({length: COLS * ROWS}, (_, i) => i);
  for (let i = idx.length - 1; i > 0; i--) {
    const j = Math.floor(rnd() * (i + 1));
    [idx[i], idx[j]] = [idx[j], idx[i]];
  }
  return idx;
})();

const ROUND_T0 = [18, 92, 166];
const SCAN = 16;
const FLY_T0 = 20;
const FLY = 18;
const FLY_STEP = 1.2;
const FULL_T0 = 240;
const FADE_A = 330;
const FADE_B = 347;

type Round = {t0: number; tiles: number[]; rowAt: number};
const ROUNDS: Round[] = SYNC.rows.map((row, r) => {
  const n = Math.round(row.changedFiles / FILES_PER_TILE);
  // earlier runs' squares are not reused, and squares are sent left to right
  const start = SYNC.rows.slice(0, r).reduce((a, x) => a + Math.round(x.changedFiles / FILES_PER_TILE), 0);
  const tiles = ORDER.slice(start, start + n).sort((a, b) => (a % COLS) - (b % COLS) || a - b);
  const rowAt = ROUND_T0[r] + (n === 0 ? SCAN + 6 : FLY_T0 + (n - 1) * FLY_STEP + FLY + 2);
  return {t0: ROUND_T0[r], tiles, rowAt};
});
const FULL_ROW_AT = FULL_T0 + SCAN + 6;

const tileX = (i: number) => (i % COLS) * PITCH;
const tileY = (i: number) => Math.floor(i / COLS) * PITCH;

const BAR_X = 340;
const BAR_MAX = 520;
const ROW_Y0 = 294;
const ROW_PITCH = 60;
const BAR_H = 30;
const PX_PER_S = BAR_MAX / SYNC.full.seconds;

const LABELS = [...SYNC.rows.map((r) => r.label), 'every file touched'];
const LABEL_T = [...ROUND_T0, FULL_T0];

export const SyncOnlyChanged: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const intro = ramp(f, 0, 10);

  // per-square highlight state
  const leftAmber = new Float32Array(COLS * ROWS);
  const rightTeal = new Float32Array(COLS * ROWS);
  const flying: {x: number; y: number; a: number}[] = [];
  const scans: {x: number; a: number}[] = [];

  ROUNDS.forEach((round) => {
    const lf = f - round.t0;
    if (lf < 0) return;
    const scanP = ramp(lf, 0, SCAN, 0, 1, EASE_IO);
    if (lf <= SCAN + 4) scans.push({x: scanP * GRID_W, a: ramp(lf, 0, 3) * (1 - ramp(lf, SCAN, SCAN + 4))});
    round.tiles.forEach((t, j) => {
      const go = FLY_T0 + j * FLY_STEP;
      const found = ramp(scanP * GRID_W, tileX(t) - 4, tileX(t) + TILE);
      // amber from the moment the scan finds it until its new version has landed
      leftAmber[t] = Math.max(leftAmber[t], found * (1 - ramp(lf, go + FLY, go + FLY + 8)));
      if (lf >= go && lf < go + FLY) {
        const p = EASE_IO(clamp01((lf - go) / FLY));
        flying.push({
          x: lerp(LX + tileX(t), RX + tileX(t), p),
          y: GY + tileY(t) - Math.min(18, tileY(t) + 4) * bump(p),
          a: 1,
        });
      }
      if (lf >= go + FLY) rightTeal[t] = Math.max(rightTeal[t], 1 - ramp(lf, go + FLY, go + FLY + 22) * 0.999);
    });
  });

  // the reference run: every file is rewritten (a wave, no flights)
  const fullF = f - FULL_T0;
  if (fullF >= 0) {
    const scanP = ramp(fullF, 0, SCAN, 0, 1, EASE_IO);
    if (fullF <= SCAN + 4) scans.push({x: scanP * GRID_W, a: ramp(fullF, 0, 3) * (1 - ramp(fullF, SCAN, SCAN + 4))});
    for (let t = 0; t < COLS * ROWS; t++) {
      const found = ramp(scanP * GRID_W, tileX(t) - 4, tileX(t) + TILE);
      const wave = SCAN + 2 + (tileX(t) / GRID_W) * 34;
      leftAmber[t] = Math.max(leftAmber[t], found * (1 - ramp(fullF, wave, wave + 10)));
      rightTeal[t] = Math.max(rightTeal[t], ramp(fullF, wave, wave + 4) * (1 - ramp(fullF, wave + 6, wave + 28)));
    }
  }

  // case label under the grids
  const labelAt = LABEL_T.map((t, i) => {
    const next = LABEL_T[i + 1] ?? FADE_A;
    return ramp(f, t, t + 6) * (1 - ramp(f, next - 6, next));
  });

  const rows = [
    ...SYNC.rows.map((r, i) => ({...r, at: ROUNDS[i].rowAt, color: P.teal, grow: 14})),
    {...SYNC.full, at: FULL_ROW_AT, color: P.coral, grow: 44},
  ];

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <g opacity={intro}>
          <text x={LX} y={62} fontSize={24} fontWeight={700} fill={P.text}>
            on this computer
          </text>
          <text x={W / 2} y={62} textAnchor="middle" fontSize={26} fontWeight={700} fill={P.teal}>
            flashcp sync
          </text>
          <text x={RX} y={62} fontSize={24} fontWeight={700} fill={P.text}>
            on the stick
          </text>

          {/* the two grids */}
          {[LX, RX].map((gx, side) => (
            <g key={side}>
              {Array.from({length: COLS * ROWS}, (_, t) => {
                const hi = (side === 0 ? leftAmber[t] : rightTeal[t]) * keep;
                return (
                  <g key={t}>
                    <rect x={gx + tileX(t)} y={GY + tileY(t)} width={TILE} height={TILE} rx={3} fill={P.tile} opacity={0.55} />
                    {hi > 0.004 ? (
                      <rect x={gx + tileX(t)} y={GY + tileY(t)} width={TILE} height={TILE} rx={3} fill={side === 0 ? P.amber : P.teal} opacity={hi} />
                    ) : null}
                  </g>
                );
              })}
            </g>
          ))}
          {scans.map((s, i) =>
            [LX, RX].map((gx) => (
              <g key={`${i}-${gx}`} opacity={s.a * keep}>
                <rect x={gx + s.x - 9} y={GY - 8} width={18} height={GRID_H + 16} rx={9} fill={P.teal} opacity={0.2} />
                <rect x={gx + s.x - 1.5} y={GY - 8} width={3} height={GRID_H + 16} rx={1.5} fill={P.teal} />
              </g>
            )),
          )}
          {/* a faint guide between the grids, then the changed squares on their way over */}
          <path
            d={`M${LX + GRID_W + 22} ${GY + GRID_H / 2} H${RX - 30} m-12 -9 l12 9 l-12 9`}
            fill="none"
            stroke={P.grid}
            strokeWidth={3}
            strokeLinecap="round"
            strokeLinejoin="round"
          />
          {flying.map((t, i) => (
            <rect key={i} x={t.x} y={t.y} width={TILE} height={TILE} rx={3} fill={P.amber} opacity={t.a * keep} />
          ))}

          <text x={LX} y={254} fontSize={22} fill={P.muted}>
            {`one square = ${FILES_PER_TILE} files`}
          </text>
          {LABELS.map((label, i) => (
            <text key={label} x={W / 2} y={254} textAnchor="middle" fontSize={24} fontWeight={700} fill={P.text} opacity={labelAt[i] * keep}>
              {label}
            </text>
          ))}
          <text x={W - 60} y={254} textAnchor="end" fontSize={22} fill={P.muted}>
            {`${fmtInt(DATASET.files)} files, ${DATASET.mb} MB`}
          </text>

          {/* one bar per run: time on the stick, and what was written */}
          {rows.map((row, r) => {
            const y = ROW_Y0 + r * ROW_PITCH;
            const show = ramp(f, row.at, row.at + 6) * keep;
            const p = ramp(f, row.at, row.at + row.grow, 0, 1, EASE_OUT);
            const len = Math.max(4, row.seconds * PX_PER_S) * p;
            return (
              <g key={r} opacity={show}>
                <text x={LX} y={y + 22} fontSize={24} fill={P.text}>
                  {row.label}
                </text>
                <rect x={BAR_X} y={y} width={len} height={BAR_H} rx={Math.min(4, len / 2)} fill={row.color} />
                <text x={BAR_X + len + 12} y={y + 23} fontSize={22} fill={P.muted} opacity={ramp(p, 0.85, 1)}>
                  <tspan fontFamily={MONO} fontSize={24} fill={P.text} style={{fontVariantNumeric: 'tabular-nums'}}>
                    {`${row.seconds.toFixed(2)} s`}
                  </tspan>
                  {` · ${row.written} written`}
                </text>
              </g>
            );
          })}

          <text x={LX} y={566} fontSize={22} fill={P.muted}>
            {SYNC.footnote}
          </text>
        </g>
      </svg>
    </AbsoluteFill>
  );
};
