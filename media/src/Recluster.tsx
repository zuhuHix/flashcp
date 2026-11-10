import React from 'react';
import {AbsoluteFill, useCurrentFrame, useVideoConfig} from 'remotion';
import {DATASET, RECLUSTER} from './data/benchmarks';
import {clamp01, lerp, smoothstep} from './motion';
import {EASE_IO, EASE_OUT, FONT, MONO, Palette, PALETTES, Theme, fmt, fmtInt, pop, ramp, scaleAt} from './shared';

// 1200 x 600, 30 fps, 360 frames (12 s).
//
// `flashcp recluster`: a stick formatted the usual way gets 4 KB clusters in place.
//   1. Ten 128 KB clusters behind the stick's index. Eight small files are added, each owning a
//      whole cluster, while the first bar grows to the measured time of that add.
//   2. Re-cluster. The new index is bigger, so the two files where it has to go are carried to
//      free clusters first (still as 128 KB clusters). The index grows into the freed room, then
//      every cluster splits into 32 squares of 4 KB: the file data keeps its place and the padding
//      becomes free space. The clock runs to the measured time.
//   3. The same add again: eight more files land side by side in the freed squares, and the second
//      bar grows at the same pace as the first (it is done almost at once).
// The grid is an illustration (ten clusters, sixteen files). The times are the measured ones.
const W = 1200;
const H = 600;
export const RECLUSTER_FRAMES = 360;

// ---- layout ----
const SQ = 14; // a 4 KB square once the clusters are split
const PITCH = 16;
const GX = 60;
const CL_COLS = 8; // a 128 KB cluster = 8 x 4 squares of 4 KB
const CL_ROWS = 4;
const CL_CELLS = CL_COLS * CL_ROWS;
const CL_W = CL_COLS * PITCH; // 128: drawn as one solid block until it is split
const CL_H = CL_ROWS * PITCH;
const CL_PITCH = 144;
const CX0 = 102;
const ROW_Y = [88, 186];
const BLOCKS = 10; // 5 columns x 2 rows, in the stick's order: down a column, then the next one
const FRAME = 6; // the cluster outline sits this far outside the block
const LANE = 25; // a carried file rises this far, into the clear lane above its row
// the index: a narrow bar before the first cluster; the new one also takes the first column
const IDX = {x: GX, y: ROW_Y[0] - FRAME, w0: 24, w1: CX0 + CL_W + FRAME - GX, h: ROW_Y[1] + CL_H + FRAME - (ROW_Y[0] - FRAME)};
const NUM_X = 926;
const STATUS_Y = 296;
const BAR_X = 250;
const BAR_W = 656;
const BAR_A_Y = 366;
const BAR_B_Y = 418;
const PILL_Y = 476;
const BAR_H = 36;
const PAD_OPACITY = 0.62;

// Eight small files like the dataset's (169 MB / 20,000 = 8.4 KB on average).
const FILE_KB = [8, 5, 12, 6, 9, 11, 7, 10];
const FILE_SQ = FILE_KB.map((kb) => Math.ceil(kb / 4));
const NEW_SQ = FILE_SQ.reduce((a, b) => a + b, 0); // the second add: the same eight files, 20 squares
const MOVED = 2; // the files in the first column, where the new index goes
const NEW_BLOCK = MOVED; // the second add fills the free squares behind this cluster's file

// ---- timeline (frames) ----
const FILE_T0 = 16;
const FILE_STEP = 9;
const DROP = 6; // a file's own squares pop in, then its padding follows
const PAD = 12;
const ADD1_T0 = 16;
const ADD1_FRAMES = 84; // the add onto 128 KB clusters
const ADD2_FRAMES = (ADD1_FRAMES * RECLUSTER.after.seconds) / RECLUSTER.before.seconds; // same pace
const RECL_T0 = 112; // the re-cluster clock starts
const RECL_END = 232; // and stops at RECLUSTER.recluster.seconds
const HOP_T0 = 120;
const HOP_STAGGER = 6;
const HOP_FRAMES = 30;
const INDEX_A = 160;
const INDEX_B = 182;
const SPLIT_T0 = 186;
const SPLIT_COL = 8; // the split sweeps along the stick, one column of clusters after the other
const SPLIT_SQ = 0.6;
const SPLIT_FRAMES = 14;
const ADD2_T0 = 246;
const PILL = 264;
const FADE_A = 342;
const FADE_B = 359;

const blockPos = (b: number) => ({cx: CX0 + Math.floor(b / 2) * CL_PITCH, cy: ROW_Y[b % 2]});
const hopStart = (m: number) => HOP_T0 + m * HOP_STAGGER;

/** Everything one cluster needs for a frame. */
function blockAt(b: number, f: number) {
  const col = Math.floor(b / 2);
  if (b < MOVED) {
    // first column: the file leaves, the cluster is free, then the index takes the room
    const t0 = FILE_T0 + b * FILE_STEP;
    const gone = 1 - ramp(f, INDEX_A - 6, INDEX_A + 6);
    return {
      files: FILE_SQ[b],
      popT: t0 as number | null,
      fileShown: f < hopStart(b),
      padFill: ramp(f, t0 + DROP, t0 + DROP + PAD),
      padKeep: (_c: number) => 1 - ramp(f, hopStart(b) + 2, hopStart(b) + 16),
      split: (_c: number) => 0,
      frame: gone,
      base: gone,
    };
  }
  const ts = (c: number) => SPLIT_T0 + (col - 1) * SPLIT_COL + c * SPLIT_SQ;
  const common = {
    padKeep: (c: number) => 1 - ramp(f, ts(c) + 4, ts(c) + 16),
    split: (c: number) => ramp(f, ts(c), ts(c) + SPLIT_FRAMES, 0, 1, EASE_IO),
    frame: 1 - ramp(f, ts(0), ts(0) + 10),
    base: 1,
  };
  if (b < FILE_SQ.length) {
    const t0 = FILE_T0 + b * FILE_STEP;
    return {...common, files: FILE_SQ[b], popT: t0 as number | null, fileShown: true, padFill: ramp(f, t0 + DROP, t0 + DROP + PAD)};
  }
  // last column: free until a carried file lands in it (as a 128 KB cluster, padding included)
  const m = b - FILE_SQ.length;
  const land = hopStart(m) + HOP_FRAMES;
  return {...common, files: FILE_SQ[m], popT: null as number | null, fileShown: f >= land, padFill: ramp(f, land, land + 10)};
}

const Swatch: React.FC<{x: number; y: number; fill: string; fillOpacity?: number; stroke?: string}> = ({x, y, fill, fillOpacity = 1, stroke}) => (
  <rect x={x} y={y - 15} width={18} height={18} rx={3} fill={fill} fillOpacity={fillOpacity} stroke={stroke ?? 'none'} strokeWidth={stroke ? 1.5 : 0} />
);

const LegendText: React.FC<{x: number; y: number; text: string; P: Palette}> = ({x, y, text, P}) => (
  <text x={x + 28} y={y} fontSize={22} fill={P.text}>
    {text}
  </text>
);

export const Recluster: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const intro = ramp(f, 0, 10);

  // headline and legend follow the stick's state
  // (one text leaves before the next arrives, so two never overlap)
  const oldHead = 1 - ramp(f, RECL_T0 - 8, RECL_T0 - 2);
  const newHead = ramp(f, RECL_T0 - 2, RECL_T0 + 6, 0, 1, EASE_OUT) * keep;
  const padLegend = 1 - ramp(f, SPLIT_T0 + 4, SPLIT_T0 + 10) + ramp(f, FADE_A + 9, FADE_B);
  const freeLegend = ramp(f, SPLIT_T0 + 10, SPLIT_T0 + 18, 0, 1, EASE_OUT) * (1 - ramp(f, FADE_A, FADE_A + 8));

  // the index
  const grow = ramp(f, INDEX_A, INDEX_B, 0, 1, EASE_IO);
  const zone = ramp(f, RECL_T0, RECL_T0 + 10) * (1 - ramp(f, INDEX_B - 4, INDEX_B + 4));
  const indexLabel = ramp(f, INDEX_B - 4, INDEX_B + 8);

  // the re-cluster clock
  const reclP = ramp(f, RECL_T0, RECL_END);
  const reclOn = ramp(f, RECL_T0 - 2, RECL_T0 + 6) * keep;
  const reclDone = ramp(f, RECL_END, RECL_END + 8);
  const hopNote = ramp(f, RECL_T0 + 4, RECL_T0 + 12) * (1 - ramp(f, INDEX_B, INDEX_B + 8));

  // the two adds, on one time scale
  const aP = ramp(f, ADD1_T0, ADD1_T0 + ADD1_FRAMES);
  const bP = ramp(f, ADD2_T0, ADD2_T0 + ADD2_FRAMES);
  const bDone = f >= ADD2_T0 + ADD2_FRAMES;
  const afterOn = ramp(f, RECL_END, RECL_END + 10) * keep;
  const speedup = `${fmt(RECLUSTER.before.seconds / RECLUSTER.after.seconds)}×`;

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <defs>
          {Array.from({length: BLOCKS}, (_, b) => {
            const {cx, cy} = blockPos(b);
            return (
              <clipPath key={b} id={`recluster-block-${b}`}>
                <rect x={cx} y={cy} width={CL_W} height={CL_H} rx={5 * (1 - blockAt(b, f).split(0))} />
              </clipPath>
            );
          })}
        </defs>
        <g opacity={intro}>
          {/* headline: what the stick is right now */}
          <text x={GX} y={46} opacity={oldHead}>
            <tspan fontSize={26} fontWeight={700} fill={P.coral}>
              {`default format: ${RECLUSTER.before.clusterKb} KB clusters`}
            </tspan>
            <tspan dx={22} fontSize={22} fill={P.muted}>
              every file owns a whole cluster
            </tspan>
          </text>
          <text x={GX} y={46} opacity={newHead}>
            <tspan fontSize={26} fontWeight={700} fill={P.teal}>
              {`re-cluster in place: ${RECLUSTER.before.clusterKb} KB → ${RECLUSTER.after.clusterKb} KB`}
            </tspan>
            <tspan dx={22} fontSize={22} fill={P.muted}>
              the files stay on the stick
            </tspan>
          </text>

          {/* the stick: index, then clusters */}
          <g opacity={keep}>
            {Array.from({length: BLOCKS}, (_, b) => {
              const {cx, cy} = blockPos(b);
              const s = blockAt(b, f);
              const cells: React.ReactNode[] = [];
              for (let r = 0; r < CL_ROWS; r++) {
                for (let c = 0; c < CL_COLS; c++) {
                  const idx = r * CL_COLS + c;
                  const x = cx + c * PITCH;
                  const y = cy + r * PITCH;
                  const sp = s.split(c);
                  const size = lerp(PITCH, SQ, sp);
                  const rx = 2.5 * sp;
                  cells.push(<rect key={`e${idx}`} x={x} y={y} width={size} height={size} rx={rx} fill={P.empty} />);
                  if (idx >= s.files) {
                    const a = clamp01(s.padFill * (CL_CELLS - s.files) - (idx - s.files)) * s.padKeep(c);
                    if (a > 0) cells.push(<rect key={`p${idx}`} x={x} y={y} width={size} height={size} rx={rx} fill={P.coral} opacity={a * PAD_OPACITY} />);
                  } else if (s.fileShown) {
                    const k = s.popT === null ? 1 : pop(f, s.popT + idx * 1.5, fps);
                    cells.push(
                      <g key={`f${idx}`} transform={scaleAt(x + PITCH / 2, y + PITCH / 2, k)}>
                        <rect x={x} y={y} width={size} height={size} rx={rx} fill={P.teal} />
                      </g>,
                    );
                  }
                  // the second add: more files into the free squares right behind this one
                  const n = idx - s.files;
                  if (b === NEW_BLOCK && n >= 0 && n < NEW_SQ) {
                    cells.push(
                      <g key={`n${idx}`} transform={scaleAt(x + SQ / 2, y + SQ / 2, pop(f, ADD2_T0 + n * 0.6, fps))}>
                        <rect x={x} y={y} width={SQ} height={SQ} rx={2.5} fill={P.teal} />
                      </g>,
                    );
                  }
                }
              }
              return (
                <g key={b}>
                  <rect
                    x={cx - FRAME}
                    y={cy - FRAME}
                    width={CL_W + 2 * FRAME}
                    height={CL_H + 2 * FRAME}
                    rx={8}
                    fill="none"
                    stroke={P.border}
                    strokeWidth={2}
                    opacity={s.frame}
                  />
                  <g opacity={s.base} clipPath={`url(#recluster-block-${b})`}>
                    {cells}
                  </g>
                </g>
              );
            })}

            {/* the room the new index needs, then the index itself */}
            <rect x={IDX.x} y={IDX.y} width={IDX.w1} height={IDX.h} rx={8} fill="none" stroke={P.amber} strokeWidth={2.5} strokeDasharray="7 7" opacity={zone} />
            <rect x={IDX.x} y={IDX.y} width={lerp(IDX.w0, IDX.w1, grow)} height={IDX.h} rx={8} fill={P.amber} fillOpacity={0.28} stroke={P.amber} strokeWidth={2.5} />
            <text x={IDX.x + IDX.w1 / 2} y={IDX.y + IDX.h / 2 + 8} textAnchor="middle" fontSize={22} fontWeight={700} fill={P.text} opacity={indexLabel}>
              new index
            </text>

            {/* the files in the index's way are carried to free clusters */}
            {Array.from({length: MOVED}, (_, m) => {
              const t = f - hopStart(m);
              if (t < 0 || t >= HOP_FRAMES) return null;
              const p = t / HOP_FRAMES;
              const from = blockPos(m);
              const to = blockPos(FILE_SQ.length + m);
              const lift = smoothstep(p / 0.3) * (1 - smoothstep((p - 0.7) / 0.3));
              const x = lerp(from.cx, to.cx, EASE_IO(p));
              const y = from.cy - LANE * lift;
              const w = FILE_SQ[m] * PITCH;
              return (
                <g key={m} transform={scaleAt(x + w / 2, y + PITCH / 2, 1 + 0.08 * lift)}>
                  <rect x={x - 2} y={y - 2} width={w + 4} height={PITCH + 4} rx={3} fill={P.bg} opacity={lift} />
                  <rect x={x} y={y} width={w} height={PITCH} rx={2.5 * lift} fill={P.teal} />
                </g>
              );
            })}
          </g>

          {/* legend */}
          <Swatch x={NUM_X} y={112} fill={P.teal} />
          <LegendText x={NUM_X} y={112} text="file data" P={P} />
          <g opacity={padLegend}>
            <Swatch x={NUM_X} y={144} fill={P.coral} fillOpacity={PAD_OPACITY} />
            <LegendText x={NUM_X} y={144} text="padding" P={P} />
          </g>
          <g opacity={freeLegend}>
            <Swatch x={NUM_X} y={144} fill={P.empty} stroke={P.border} />
            <LegendText x={NUM_X} y={144} text="free space" P={P} />
          </g>
          <Swatch x={NUM_X} y={176} fill={P.amber} fillOpacity={0.28} stroke={P.amber} />
          <LegendText x={NUM_X} y={176} text="the stick's index" P={P} />
          <text x={NUM_X} y={232} fontSize={22} fill={P.muted} opacity={padLegend}>
            {`one block = ${RECLUSTER.before.clusterKb} KB`}
          </text>
          <text x={NUM_X} y={232} fontSize={22} fill={P.muted} opacity={freeLegend}>
            {`one square = ${RECLUSTER.after.clusterKb} KB`}
          </text>

          {/* the re-cluster clock */}
          <g opacity={reclOn}>
            <text x={GX} y={STATUS_Y} fontFamily={MONO} fontSize={32} fontWeight={600} fill={P.teal} style={{fontVariantNumeric: 'tabular-nums'}}>
              {`${fmt(RECLUSTER.recluster.seconds * reclP, 2)} s`}
            </text>
            {/* working: a turning arc; done: a tick */}
            <g opacity={1 - reclDone}>
              <circle cx={212} cy={STATUS_Y - 10} r={12} fill="none" stroke={P.grid} strokeWidth={4} />
              <path
                d={`M212 ${STATUS_Y - 22} a12 12 0 0 1 12 12`}
                fill="none"
                stroke={P.teal}
                strokeWidth={4}
                strokeLinecap="round"
                transform={`rotate(${(f - RECL_T0) * 14} 212 ${STATUS_Y - 10})`}
              />
            </g>
            <g opacity={reclDone} transform={scaleAt(212, STATUS_Y - 10, pop(f, RECL_END, fps))}>
              <circle cx={212} cy={STATUS_Y - 10} r={14} fill={P.teal} />
              <path
                d={`M205 ${STATUS_Y - 10} l5 6 l9 -11`}
                fill="none"
                stroke={P.pillText}
                strokeWidth={3.5}
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </g>
            <text x={238} y={STATUS_Y - 2}>
              <tspan fontSize={24} fontWeight={700} fill={P.text}>
                nothing is erased
              </tspan>
              <tspan dx={18} fontSize={22} fill={P.muted} opacity={reclDone}>
                {`${fmt(RECLUSTER.recluster.writtenMb, 0)} MB written`}
              </tspan>
            </text>
            <text x={458} y={STATUS_Y - 2} fontSize={22} fill={P.muted} opacity={hopNote}>
              files in the new index's way move aside
            </text>
          </g>

          {/* the same add, before and after */}
          <text x={GX} y={348} fontSize={24} fontWeight={700} fill={P.text}>
            {`adding ${fmtInt(DATASET.files)} small files (${DATASET.mb} MB) to the stick`}
          </text>
          <text x={GX} y={BAR_A_Y + 26} fontSize={22} fontWeight={700} fill={P.coral}>
            {`${RECLUSTER.before.clusterKb} KB clusters`}
          </text>
          <text x={GX} y={BAR_B_Y + 26} fontSize={22} fontWeight={700} fill={P.teal} opacity={lerp(0.35, 1, afterOn)}>
            {`${RECLUSTER.after.clusterKb} KB clusters`}
          </text>
          <rect x={BAR_X} y={BAR_A_Y} width={BAR_W} height={BAR_H} rx={4} fill={P.empty} />
          <rect x={BAR_X} y={BAR_B_Y} width={BAR_W} height={BAR_H} rx={4} fill={P.empty} />
          <g opacity={keep}>
            <rect x={BAR_X} y={BAR_A_Y} width={BAR_W * aP} height={BAR_H} rx={4} fill={P.coral} />
            <rect x={BAR_X} y={BAR_B_Y} width={BAR_W * (RECLUSTER.after.seconds / RECLUSTER.before.seconds) * bP} height={BAR_H} rx={4} fill={P.teal} />
            <text
              x={NUM_X}
              y={BAR_A_Y + 26}
              fontFamily={MONO}
              fontSize={24}
              fontWeight={700}
              fill={P.coral}
              opacity={f >= ADD1_T0 ? 1 : 0}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {`${fmt(RECLUSTER.before.seconds * aP)} s`}
            </text>
            <g opacity={bDone ? 1 : 0} transform={scaleAt(NUM_X + 40, BAR_B_Y + 18, 0.6 + 0.4 * pop(f, ADD2_T0 + ADD2_FRAMES, fps))}>
              <text x={NUM_X} y={BAR_B_Y + 26} fontFamily={MONO} fontSize={24} fontWeight={700} fill={P.teal}>
                {`${fmt(RECLUSTER.after.seconds, 2)} s`}
              </text>
            </g>
          </g>

          <text x={GX} y={PILL_Y + 32} fontSize={22} fill={P.text} opacity={ramp(f, PILL + 4, PILL + 16, 0, 1, EASE_OUT) * keep}>
            same stick, files kept: only the cluster size changed
          </text>
          <g opacity={keep} transform={scaleAt(NUM_X + 107, PILL_Y + 24, pop(f, PILL, fps))}>
            <rect x={NUM_X} y={PILL_Y} width={214} height={48} rx={24} fill={P.teal} />
            <text x={NUM_X + 107} y={PILL_Y + 33} textAnchor="middle" fontSize={24} fontWeight={700} fill={P.pillText}>
              {`${speedup} faster`}
            </text>
          </g>

          <text x={GX} y={566} fontSize={22} fill={P.muted}>
            {RECLUSTER.footnote}
          </text>
        </g>
      </svg>
    </AbsoluteFill>
  );
};
