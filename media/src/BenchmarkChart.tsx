import React from 'react';
import {AbsoluteFill, useCurrentFrame, useVideoConfig} from 'remotion';
import {BENCH_PAGES, BenchPage, BenchPageId} from './data/benchmarks';
import {EASE_OUT, FONT, MONO, PALETTES, Theme, pop, ramp, scaleAt} from './shared';

// 1200 x 640, 30 fps, 240 frames (8 s). One drive per composition: paired bars (kernel driver vs
// flashcp) on a shared seconds axis, a range drawn as a fainter extension, and the speed-up pill.
// Every number and label comes from src/data/benchmarks.ts; the layout adapts to 3 or 4 rows.
const W = 1200;
const H = 640;
const X0 = 340; // x of 0 s
const X_END = 830; // x of the longest bar
const ROWS_TOP = 128;
const ROWS_BOTTOM = 512;
const FADE_A = 222;
const FADE_B = 239;
export const BENCH_FRAMES = 240;

export const BenchmarkChart: React.FC<{theme: Theme; page: BenchPageId}> = ({theme, page: pageId}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const page: BenchPage = BENCH_PAGES[pageId];
  const n = page.rows.length;
  const pitch = (ROWS_BOTTOM - ROWS_TOP) / n;
  const barH = n > 3 ? 32 : 36;
  const barGap = n > 3 ? 6 : 8;
  const rowH = 2 * barH + barGap;
  const px = (X_END - X0) / page.axisMax;
  const show = ramp(f, 0, 12) * (1 - ramp(f, FADE_A, FADE_B));
  const flashLegendX = 60 + 28 + page.baseLegend.length * 11.6 + 36;

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <g opacity={show}>
          <text x={60} y={56} fontSize={28} fontWeight={700} fill={P.text}>
            {page.title}
          </text>
          <rect x={60} y={82} width={18} height={18} rx={3} fill={P.coral} />
          <text x={88} y={98} fontSize={22} fill={P.text}>
            {page.baseLegend}
          </text>
          <rect x={flashLegendX} y={82} width={18} height={18} rx={3} fill={P.teal} />
          <text x={flashLegendX + 28} y={98} fontSize={22} fill={P.text}>
            {page.flashLegend}
          </text>

          {page.ticks.map((t) => (
            <g key={t}>
              <line x1={X0 + t * px} y1={ROWS_TOP - 6} x2={X0 + t * px} y2={ROWS_BOTTOM + 6} stroke={P.grid} strokeWidth={1.5} />
              <text x={X0 + t * px} y={ROWS_BOTTOM + 34} textAnchor="middle" fontSize={22} fill={P.muted}>
                {`${t} s`}
              </text>
            </g>
          ))}

          {page.rows.map((row, r) => {
            const top = ROWS_TOP + r * pitch + (pitch - rowH) / 2;
            const s = 14 + r * 30;
            const baseP = ramp(f, s, s + 40, 0, 1, EASE_OUT);
            const flashP = ramp(f, s + 6, s + 46, 0, 1, EASE_OUT);
            const pillScale = pop(f, s + 38, fps);
            const pillW = Math.max(104, row.pill.length * 15 + 40);
            const pillCx = 1170 - pillW / 2;
            const pillCy = top + rowH / 2;
            const bars = [
              {bar: row.base, p: baseP, y: top, color: P.coral},
              {bar: row.flash, p: flashP, y: top + barH + barGap, color: P.teal},
            ];
            return (
              <g key={r}>
                <text x={60} y={top + barH - 8} fontSize={24} fontWeight={700} fill={P.text}>
                  {row.name}
                </text>
                <text x={60} y={top + barH + barGap + barH - 10} fontSize={22} fill={P.muted}>
                  {row.sub}
                </text>
                {bars.map(({bar, p, y, color}, i) => {
                  const len = Math.max(3, bar.value * px) * p;
                  const rangeLen = Math.max(3, (bar.max ?? bar.value) * px) * p;
                  return (
                    <g key={i}>
                      {bar.max ? <rect x={X0} y={y} width={rangeLen} height={barH} rx={4} fill={color} opacity={0.38} /> : null}
                      <rect x={X0} y={y} width={len} height={barH} rx={Math.min(4, len / 2)} fill={color} />
                      <text
                        x={X0 + rangeLen + 12}
                        y={y + barH / 2 + 8}
                        fontFamily={MONO}
                        fontSize={24}
                        fill={P.text}
                        stroke={P.bg}
                        strokeWidth={7}
                        strokeLinejoin="round"
                        paintOrder="stroke"
                        opacity={ramp(p, 0.9, 1)}
                        style={{fontVariantNumeric: 'tabular-nums'}}
                      >
                        {`${bar.label} s`}
                      </text>
                    </g>
                  );
                })}
                <g transform={scaleAt(pillCx, pillCy, pillScale)}>
                  <rect x={pillCx - pillW / 2} y={pillCy - 26} width={pillW} height={52} rx={26} fill={P.teal} />
                  <text x={pillCx} y={pillCy + 9} textAnchor="middle" fontSize={26} fontWeight={700} fill={P.pillText}>
                    {row.pill}
                  </text>
                </g>
              </g>
            );
          })}

          <text x={60} y={584} fontSize={22} fill={P.muted}>
            {page.notes[0]}
          </text>
          <text x={60} y={616} fontSize={22} fill={P.muted}>
            {page.notes[1]}
          </text>
        </g>
      </svg>
    </AbsoluteFill>
  );
};
