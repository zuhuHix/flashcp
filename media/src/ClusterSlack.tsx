import React from 'react';
import {AbsoluteFill, useCurrentFrame, useVideoConfig} from 'remotion';
import {CLUSTER_SLACK, DATASET} from './data/benchmarks';
import {clamp01} from './motion';
import {EASE_OUT, FONT, MONO, Palette, PALETTES, Theme, fmt, fmtInt, pop, ramp, scaleAt} from './shared';

// 1200 x 600, 30 fps, 340 frames (11.3 s).
//
// Why a stick formatted the usual way is slow for small files. One square is 4 KB. On the default
// format a cluster is 128 KB (32 squares), and every file owns a whole one, so the driver writes
// mostly padding. With 4 KB clusters the same files sit side by side. Then the totals for the real
// copy, both bars growing at their measured pace (the short one is done almost at once).
const W = 1200;
const H = 600;
export const CLUSTER_SLACK_FRAMES = 340;

const SQ = 14;
const PITCH = 16;
const GX = 60;
const A_Y = 88;
const B_Y = 226;
const CL_COLS = 8; // a 128 KB cluster = 8 x 4 squares of 4 KB
const CL_ROWS = 4;
const CL_W = CL_COLS * PITCH - 2; // 126
const CL_GAP = 18;
const B_COLS = 53;
const GRID_W = B_COLS * PITCH - 2; // 846
const BAR_X = GX;
const BAR_W = GRID_W;
const BAR_A_Y = 356;
const BAR_B_Y = 408;
const BAR_H = 36;
const NUM_X = 926;

// Six small files like the dataset's (169 MB / 20,000 = 8.4 KB on average).
const FILE_KB = [8, 5, 12, 6, 9, 11];
const FILE_SQ = FILE_KB.map((kb) => Math.ceil(kb / 4));
const B_OFF = FILE_SQ.map((_, i) => FILE_SQ.slice(0, i).reduce((a, b) => a + b, 0));

// timeline
const FILE_T0 = 16;
const FILE_STEP = 14;
const DROP = 6; // the file's own squares pop in, then the padding follows
const PAD = 14;
const TOTALS_IN = 116;
const WRITE_T0 = 134;
const WRITE_A = 120; // frames for the 128 KB copy
const WRITE_B = (WRITE_A * CLUSTER_SLACK.small.seconds) / CLUSTER_SLACK.big.seconds;
const PILL = WRITE_T0 + WRITE_A + 6;
const FADE_A = 322;
const FADE_B = 339;

const Square: React.FC<{x: number; y: number; fill: string; opacity?: number; w?: number}> = ({x, y, fill, opacity = 1, w = SQ}) => (
  <rect x={x} y={y} width={w} height={SQ} rx={2.5} fill={fill} opacity={opacity} />
);

const Legend: React.FC<{x: number; y: number; color: string; text: string; P: Palette; opacity?: number}> = ({x, y, color, text, P, opacity = 1}) => (
  <g>
    <rect x={x} y={y - 15} width={18} height={18} rx={3} fill={color} opacity={opacity} />
    <text x={x + 28} y={y} fontSize={22} fill={P.text}>
      {text}
    </text>
  </g>
);

export const ClusterSlack: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const intro = ramp(f, 0, 10);
  const PAD_OPACITY = 0.62;

  const totals = ramp(f, TOTALS_IN, TOTALS_IN + 12) * keep;
  const aP = ramp(f, WRITE_T0, WRITE_T0 + WRITE_A);
  const bP = ramp(f, WRITE_T0, WRITE_T0 + WRITE_B);
  const aLen = BAR_W * aP;
  const payloadLen = BAR_W * (DATASET.mb / CLUSTER_SLACK.big.writtenMb);
  const bLen = BAR_W * (CLUSTER_SLACK.small.writtenMb / CLUSTER_SLACK.big.writtenMb) * bP;
  const aMb = CLUSTER_SLACK.big.writtenMb * aP;
  const aDone = f >= WRITE_T0 + WRITE_A;
  const bDone = f >= WRITE_T0 + WRITE_B;

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <g opacity={intro}>
          {/* lane A: default format */}
          <text x={GX} y={46} fontSize={26} fontWeight={700} fill={P.coral}>
            default format: 128 KB clusters
          </text>
          <text x={478} y={46} fontSize={22} fill={P.muted}>
            every file owns a whole cluster
          </text>
          {FILE_KB.map((_kb, i) => {
            const cx = GX + i * (CL_W + CL_GAP);
            const t0 = FILE_T0 + i * FILE_STEP;
            const n = FILE_SQ[i];
            const padP = ramp(f, t0 + DROP, t0 + DROP + PAD);
            const squares: React.ReactNode[] = [];
            for (let r = 0; r < CL_ROWS; r++) {
              for (let c = 0; c < CL_COLS; c++) {
                const idx = r * CL_COLS + c;
                const x = cx + c * PITCH;
                const y = A_Y + r * PITCH;
                squares.push(<Square key={`e${idx}`} x={x} y={y} fill={P.empty} />);
                if (idx >= n) {
                  const a = clamp01(padP * (CL_COLS * CL_ROWS - n) - (idx - n));
                  if (a > 0) squares.push(<Square key={`p${idx}`} x={x} y={y} fill={P.coral} opacity={a * PAD_OPACITY * keep} />);
                }
              }
            }
            return (
              <g key={i}>
                <rect x={cx - 6} y={A_Y - 6} width={CL_W + 12} height={CL_ROWS * PITCH + 10} rx={7} fill="none" stroke={P.border} strokeWidth={2} />
                {squares}
                {Array.from({length: n}, (_, k) => (
                  <g key={k} transform={scaleAt(cx + k * PITCH + SQ / 2, A_Y + SQ / 2, pop(f, t0 + k * 1.5, fps))} opacity={keep}>
                    <Square x={cx + k * PITCH} y={A_Y} fill={P.teal} />
                  </g>
                ))}
              </g>
            );
          })}
          <Legend x={NUM_X} y={112} color={P.teal} text="file data" P={P} />
          <Legend x={NUM_X} y={144} color={P.coral} text="padding (zeros)" P={P} opacity={PAD_OPACITY} />

          {/* lane B: 4 KB clusters */}
          <text x={GX} y={208} fontSize={26} fontWeight={700} fill={P.teal}>
            flashcp format: 4 KB clusters
          </text>
          <text x={452} y={208} fontSize={22} fill={P.muted}>
            the same files, side by side
          </text>
          {Array.from({length: B_COLS * CL_ROWS}, (_, s) => (
            <Square key={s} x={GX + Math.floor(s / CL_ROWS) * PITCH} y={B_Y + (s % CL_ROWS) * PITCH} fill={P.empty} />
          ))}
          {FILE_SQ.map((n, i) => {
            const t0 = FILE_T0 + i * FILE_STEP;
            return (
              <g key={i} opacity={keep}>
                {Array.from({length: n}, (_, k) => {
                  const s = B_OFF[i] + k;
                  const x = GX + Math.floor(s / CL_ROWS) * PITCH;
                  const y = B_Y + (s % CL_ROWS) * PITCH;
                  return (
                    <g key={k} transform={scaleAt(x + SQ / 2, y + SQ / 2, pop(f, t0 + k * 1.5, fps))}>
                      <Square x={x} y={y} fill={P.teal} />
                    </g>
                  );
                })}
              </g>
            );
          })}
          <text x={NUM_X} y={258} fontSize={22} fill={P.muted}>
            one square = 4 KB
          </text>

          {/* totals for the real copy */}
          <g opacity={totals}>
            <text x={GX} y={338} fontSize={24} fontWeight={700} fill={P.text}>
              {`× ${fmtInt(DATASET.files)} files (${DATASET.mb} MB): what the stick has to write`}
            </text>
            <rect x={BAR_X} y={BAR_A_Y} width={BAR_W} height={BAR_H} rx={4} fill={P.empty} />
            <rect x={BAR_X} y={BAR_B_Y} width={BAR_W} height={BAR_H} rx={4} fill={P.empty} />
            {/* 128 KB: the payload first, then padding */}
            <rect x={BAR_X} y={BAR_A_Y} width={aLen} height={BAR_H} rx={4} fill={P.coral} opacity={PAD_OPACITY} />
            <rect x={BAR_X} y={BAR_A_Y} width={Math.min(aLen, payloadLen)} height={BAR_H} rx={4} fill={P.teal} />
            <rect x={BAR_X} y={BAR_B_Y} width={bLen} height={BAR_H} rx={4} fill={P.teal} />

            <text
              x={NUM_X}
              y={BAR_A_Y + 26}
              fontFamily={MONO}
              fontSize={24}
              fontWeight={700}
              fill={P.coral}
              opacity={f >= WRITE_T0 ? 1 : 0}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {aDone ? CLUSTER_SLACK.big.writtenLabel : `${fmt(aMb / 1000)} GB`}
            </text>
            <text
              x={1140}
              y={BAR_A_Y + 26}
              textAnchor="end"
              fontFamily={MONO}
              fontSize={24}
              fill={P.text}
              opacity={f >= WRITE_T0 ? 1 : 0}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {`${fmt(CLUSTER_SLACK.big.seconds * aP)} s`}
            </text>
            <g opacity={bDone ? 1 : 0} transform={scaleAt(NUM_X + 100, BAR_B_Y + 18, 0.6 + 0.4 * pop(f, WRITE_T0 + WRITE_B, fps))}>
              <text x={NUM_X} y={BAR_B_Y + 26} fontFamily={MONO} fontSize={24} fontWeight={700} fill={P.teal}>
                {CLUSTER_SLACK.small.writtenLabel}
              </text>
              <text x={1140} y={BAR_B_Y + 26} textAnchor="end" fontFamily={MONO} fontSize={24} fill={P.text}>
                {`${fmt(CLUSTER_SLACK.small.seconds)} s`}
              </text>
            </g>
          </g>

          <text x={GX} y={500} fontSize={22} fill={P.text} opacity={ramp(f, PILL + 4, PILL + 16, 0, 1, EASE_OUT) * keep}>
            same stick, same files, same driver: only the cluster size differs
          </text>
          <g opacity={keep} transform={scaleAt(1033, 492, pop(f, PILL, fps))}>
            <rect x={NUM_X} y={468} width={214} height={48} rx={24} fill={P.teal} />
            <text x={1033} y={501} textAnchor="middle" fontSize={24} fontWeight={700} fill={P.pillText}>
              {`${CLUSTER_SLACK.speedup} faster`}
            </text>
          </g>

          <text x={GX} y={566} fontSize={22} fill={P.muted}>
            {CLUSTER_SLACK.footnote}
          </text>
        </g>
      </svg>
    </AbsoluteFill>
  );
};
