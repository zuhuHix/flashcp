import React from 'react';
import {AbsoluteFill, Easing, useCurrentFrame} from 'remotion';
import {FONT, PALETTES, Theme, mulberry32, ramp} from './shared';

// 1200 x 600, 30 fps, 240 frames (8 s).
const W = 1200;
const H = 600;
const X0 = 60;
const BAR_W = 1080;
const BAR_Y = 250;
const BAR_H = 90;
const GAP = 4;
const SWEEP_A = 20;
const SWEEP_B = 170;
const FADE_A = 214;
const FADE_B = 238;

const META = [
  {name: 'boot', w: 50},
  {name: 'FAT', w: 90},
  {name: 'bitmap', w: 110},
  {name: 'up-case', w: 110},
  {name: 'directories', w: 130},
];
const metaTotal = META.reduce((s, m) => s + m.w, 0);
const DATA_W = BAR_W - metaTotal - GAP * META.length;

type Seg = {name: string; x: number; w: number; meta: boolean};
const SEGS: Seg[] = [];
{
  let x = X0;
  for (const m of META) {
    SEGS.push({name: m.name, x, w: m.w, meta: true});
    x += m.w + GAP;
  }
  SEGS.push({name: 'file data', x, w: DATA_W, meta: false});
}

// File blocks inside the file-data segment: seeded widths, alternating teal shades.
const BLOCKS: {x: number; w: number; i: number}[] = [];
{
  const rnd = mulberry32(42);
  let x = SEGS[SEGS.length - 1].x;
  const end = x + DATA_W;
  let i = 0;
  while (x < end - 4) {
    const w = Math.min(end - x, 8 + Math.floor(rnd() * 14));
    BLOCKS.push({x, w, i});
    x += w;
    i++;
  }
}

export const VolumeLayout: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const hx = X0 + ramp(f, SWEEP_A, SWEEP_B, 0, BAR_W, Easing.bezier(0.3, 0, 0.7, 1));
  const headA = ramp(f, SWEEP_A - 4, SWEEP_A, 0, 1) * ramp(f, SWEEP_B + 2, SWEEP_B + 8, 1, 0) * keep;
  const arrowProg = ramp(f, SWEEP_A, SWEEP_B);
  const arrowHeadA = ramp(f, SWEEP_B - 4, SWEEP_B + 2) * keep;
  const arrowLabelA = ramp(f, SWEEP_B + 2, SWEEP_B + 10) * keep;
  const endTextA = ramp(f, SWEEP_B + 14, SWEEP_B + 24) * keep;
  const arrowY = BAR_Y + BAR_H + 42;

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <text x={X0} y={110} fontSize={30} fontWeight={700} fill={P.text}>
          exFAT volume, written in one pass
        </text>

        {/* empty volume */}
        {SEGS.map((s) => (
          <rect key={`e${s.name}`} x={s.x} y={BAR_Y} width={s.w} height={BAR_H} rx={4} fill={P.empty} />
        ))}

        {/* metadata segments fill (amber) as the head passes */}
        {SEGS.filter((s) => s.meta).map((s) => (
          <rect
            key={`m${s.name}`}
            x={s.x}
            y={BAR_Y}
            width={s.w * ramp(hx, s.x, s.x + s.w)}
            height={BAR_H}
            rx={4}
            fill={P.amber}
            opacity={keep}
          />
        ))}

        {/* file data: alternating teal blocks, each filled as the head passes */}
        {BLOCKS.map((b) => (
          <rect
            key={`b${b.x}`}
            x={b.x}
            y={BAR_Y}
            width={Math.max(0, Math.min(b.w - 2, hx - b.x))}
            height={BAR_H}
            fill={b.i % 2 === 0 ? P.teal : P.tealAlt}
            opacity={keep}
          />
        ))}

        {/* segment labels slide and fade in as they are written */}
        {SEGS.map((s) => {
          const a = ramp(hx, s.x, s.x + 30) * keep;
          const dy = ramp(hx, s.x, s.x + 30, 12, 0);
          return (
            <text
              key={`l${s.name}`}
              x={s.x + s.w / 2}
              y={BAR_Y - 16 + dy}
              textAnchor="middle"
              fontSize={22}
              fontWeight={600}
              fill={s.meta ? P.text : P.teal}
              opacity={a}
            >
              {s.name}
            </text>
          );
        })}

        {/* write head */}
        <g opacity={headA}>
          <rect x={hx - 10} y={BAR_Y - 12} width={20} height={BAR_H + 24} rx={6} fill={P.text} fillOpacity={0.12} />
          <rect x={hx - 2} y={BAR_Y - 12} width={4} height={BAR_H + 24} rx={2} fill={P.text} />
        </g>

        {/* one ascending stream of writes */}
        <line
          x1={X0}
          y1={arrowY}
          x2={X0 + BAR_W - 16}
          y2={arrowY}
          stroke={P.teal}
          strokeWidth={4}
          strokeLinecap="round"
          pathLength={1}
          strokeDasharray="1 1"
          strokeDashoffset={1 - arrowProg}
          opacity={keep}
        />
        <polygon
          points={`${X0 + BAR_W},${arrowY} ${X0 + BAR_W - 16},${arrowY - 9} ${X0 + BAR_W - 16},${arrowY + 9}`}
          fill={P.teal}
          opacity={arrowHeadA}
        />
        <text x={600} y={arrowY + 42} textAnchor="middle" fontSize={28} fontWeight={700} fill={P.teal} opacity={arrowLabelA}>
          one ascending stream of 8 MB writes
        </text>

        <text x={600} y={520} textAnchor="middle" fontSize={26} fill={P.muted} opacity={endTextA}>
          a normal exFAT drive with normal loose files
        </text>
      </svg>
    </AbsoluteFill>
  );
};
