import React from 'react';
import {AbsoluteFill, useCurrentFrame, useVideoConfig} from 'remotion';
import {DATASET, DRIVE_MODE} from './data/benchmarks';
import {bump, clamp01, lerp, track} from './motion';
import {EASE_IO, EASE_OUT, FONT, MONO, Palette, PALETTES, Theme, fmt, pop, ramp, scaleAt, settle} from './shared';

// 1200 x 600, 30 fps, 390 frames (13 s). The README hero.
//
// A stick is plugged in and flashcp's driver takes it over. Three apps drop files; they land in
// the driver's staging lane at once and leave for the stick as one ordered stream. Eject shows
// the real notification texts (src/daemon.rs). Below, the kernel driver does the same copy on the
// same stick with one metadata round trip per file. The three apps are an illustration: the
// measured copy was one `cp -r`, and appDoneS is when it returned.
//
// Both clocks run on ONE time scale: frame COPY_T0 is 0 s and frame DONE is DRIVE_MODE.durableS,
// so the kernel strip is exactly as far as kernelS (its faster run) allows when flashcp is done.
const W = 1200;
const H = 600;
export const DRIVE_MODE_FRAMES = 390;

// ---- timeline (frames) ----
const PLUG_A = 14;
const PLUG_B = 42;
const TAKE = 46; // the driver takes the stick over
const COPY_T0 = 78; // the apps start copying (0 s)
const DONE = 272; // "can be removed" (durableS)
const F_PER_S = (DONE - COPY_T0) / DRIVE_MODE.durableS;
const APP_DONE = COPY_T0 + DRIVE_MODE.appDoneS * F_PER_S; // the copy returns in the apps
const EJECT = 134;
const TOAST = 138;
const KERNEL_RESULT = 296;
const BADGE = 308;
const FADE_A = 372;
const FADE_B = 389;

// ---- layout ----
const APP_X = 60;
const APP_W = 240;
const APP_H = 70;
const APP_YS = [90, 176, 262];
const APPS = ['File manager', 'Browser', 'Terminal'];
const LANE = {x: 400, y: 84, w: 320, h: 248};
const TILE_W = 38;
const TILE_H = 30;
const SLOT_X0 = 421;
const SLOT_Y0 = 104;
const SLOT_PX = 48;
const SLOT_PY = 40;
const PIPE_Y = 222;
const STICK = {x: 860, y: 150, w: 280, h: 150};
const BAR = {x: 880, y: 202, w: 240, h: 40, cells: 24};
const TOAST_BOX = {x: 740, y: 22, w: 400, h: 108};

// ---- files: 24 tiles, 8 per app, launched one per frame so all three apps feed the lane at once ----
const N = 24;
const FLIGHT = 10;
const STREAM_T0 = 100; // the first tile leaves the lane for the stick
const STREAM_FLIGHT = 18;
const STREAM_STEP = (DONE - STREAM_FLIGHT - STREAM_T0) / (N - 1);
const LAUNCH_STEP = (APP_DONE - FLIGHT - COPY_T0) / (N - 1);

type Tile = {
  app: number;
  launch: number;
  land: number;
  leave: number;
  sx: number; // slot centre
  sy: number;
  outX: (f: number) => number;
  outY: (f: number) => number;
};

const TILES: Tile[] = Array.from({length: N}, (_, i) => {
  const launch = COPY_T0 + i * LAUNCH_STEP;
  const leave = STREAM_T0 + i * STREAM_STEP;
  const sx = SLOT_X0 + (i % 6) * SLOT_PX + TILE_W / 2;
  const sy = SLOT_Y0 + Math.floor(i / 6) * SLOT_PY + TILE_H / 2;
  // slot -> lane exit -> along the pipe into the stick; one curve, so speed never stops at the exit
  const outX = track([
    {f: leave, v: sx},
    {f: leave + 8, v: LANE.x + LANE.w - 6},
    {f: leave + STREAM_FLIGHT, v: BAR.x + 6, slope: 15},
  ]);
  const outY = track([
    {f: leave, v: sy},
    {f: leave + 8, v: PIPE_Y},
    {f: leave + STREAM_FLIGHT, v: PIPE_Y},
  ]);
  return {app: i % 3, launch, land: launch + FLIGHT, leave, sx, sy, outX, outY};
});

/** Fraction of the copy that is on the stick (0..1), in step with the tiles arriving. */
const savedAt = (f: number): number => clamp01((f - (STREAM_T0 + STREAM_FLIGHT)) / (DONE - STREAM_T0 - STREAM_FLIGHT));

// ---- kernel strip: 58 x 2 cells, 6 metadata columns, one data cell per file, to the same time scale ----
const K = {x: 60, y: 456, cols: 58, rows: 2, metaCols: 6, cw: 12, ch: 24, px: 15, py: 30};
const K_DATA = (K.cols - K.metaCols) * K.rows;
const K_STEP = (DRIVE_MODE.kernelS * F_PER_S) / K_DATA; // frames per file
const kMeta = (m: number) => ({x: K.x + Math.floor(m / K.rows) * K.px, y: K.y + (m % K.rows) * K.py});
const kData = (d: number) => ({x: K.x + (K.metaCols + Math.floor(d / K.rows)) * K.px, y: K.y + (d % K.rows) * K.py});
const kMetaOf = (k: number) => (k * 5 + 3) % (K.metaCols * K.rows);

function kernelAt(f: number) {
  const t = Math.max(0, f - COPY_T0);
  const k = Math.floor(t / K_STEP);
  const u = (t - k * K_STEP) / K_STEP; // 0..1 within this file
  const from = kMeta(kMetaOf(k - 1 < 0 ? 0 : k - 1));
  const data = kData(k);
  const back = kMeta(kMetaOf(k));
  // hop out (0-0.24), write (0.24-0.54), hop back (0.54-0.78), metadata write (0.78-1)
  const a = EASE_IO(clamp01(u / 0.24));
  const b = EASE_IO(clamp01((u - 0.54) / 0.24));
  const head =
    u < 0.54
      ? {x: lerp(from.x, data.x, a), y: lerp(from.y, data.y, a)}
      : {x: lerp(data.x, back.x, b), y: lerp(data.y, back.y, b)};
  return {
    files: k,
    writing: clamp01((u - 0.24) / 0.3),
    metaFlash: u >= 0.78 ? 1 - (u - 0.78) / 0.22 : 0,
    metaCell: kMetaOf(k),
    hop: u < 0.24 ? {from, to: head} : u >= 0.54 && u < 0.78 ? {from: data, to: head} : null,
    head,
  };
}

const AppIcon: React.FC<{kind: number; x: number; y: number; color: string}> = ({kind, x, y, color}) => {
  if (kind === 0) {
    // folder
    return (
      <path
        d={`M${x} ${y + 6} h10 l4 4 h16 v18 h-30 z`}
        fill="none"
        stroke={color}
        strokeWidth={2.5}
        strokeLinejoin="round"
      />
    );
  }
  if (kind === 1) {
    // download: arrow into a tray
    return (
      <g fill="none" stroke={color} strokeWidth={2.5} strokeLinecap="round" strokeLinejoin="round">
        <path d={`M${x + 15} ${y + 2} v16 M${x + 8} ${y + 12} l7 7 l7 -7`} />
        <path d={`M${x + 1} ${y + 22} v6 h28 v-6`} />
      </g>
    );
  }
  // terminal prompt
  return (
    <g fill="none" stroke={color} strokeWidth={2.5} strokeLinecap="round" strokeLinejoin="round">
      <path d={`M${x + 2} ${y + 6} l9 8 l-9 8`} />
      <path d={`M${x + 16} ${y + 24} h13`} />
    </g>
  );
};

const FileTile: React.FC<{cx: number; cy: number; s: number; opacity: number; P: Palette}> = ({cx, cy, s, opacity, P}) => (
  <g transform={scaleAt(cx, cy, s)} opacity={opacity}>
    <rect x={cx - TILE_W / 2} y={cy - TILE_H / 2} width={TILE_W} height={TILE_H} rx={5} fill={P.tile} />
    <rect x={cx - TILE_W / 2 + 7} y={cy - 6} width={TILE_W - 14} height={3} rx={1.5} fill={P.bg} opacity={0.7} />
    <rect x={cx - TILE_W / 2 + 7} y={cy + 2} width={TILE_W - 22} height={3} rx={1.5} fill={P.bg} opacity={0.7} />
  </g>
);

export const DriveMode: React.FC<{theme: Theme}> = ({theme}) => {
  const f = useCurrentFrame();
  const {fps} = useVideoConfig();
  const P = PALETTES[theme];
  const keep = 1 - ramp(f, FADE_A, FADE_B);
  const intro = ramp(f, 0, 10);

  // stick: slides into the port, then the driver takes it over
  const plug = ramp(f, PLUG_A, PLUG_B, 0, 1, EASE_OUT);
  const stickDx = (1 - plug) * 190;
  const stickA = ramp(f, PLUG_A, PLUG_A + 8) * keep;
  const take = settle(f, TAKE, fps) * keep;
  const saved = savedAt(f);
  const done = f >= DONE;
  const doneP = ramp(f, DONE, DONE + 8);

  // clocks (one time scale)
  const realS = Math.max(0, f - COPY_T0) / F_PER_S;
  const flashClock = Math.min(realS, DRIVE_MODE.durableS);
  const copying = f >= COPY_T0;
  const appsBusy = ramp(f, COPY_T0 - 14, COPY_T0 - 4) * (1 - ramp(f, APP_DONE, APP_DONE + 10));

  // eject + notification
  const ejectPress = bump(clamp01((f - EJECT) / 10));
  const ripple = clamp01((f - EJECT) / 16);
  const toastIn = settle(f, TOAST, fps);
  const pendingMb = Math.max(1, Math.round(DATASET.mb * (1 - saved)));

  const kn = kernelAt(f);
  const kernelDone = ramp(f, KERNEL_RESULT, KERNEL_RESULT + 10);
  const stickStroke = done ? P.teal : P.border;

  return (
    <AbsoluteFill style={{background: P.bg}}>
      <svg width={W} height={H} style={{display: 'block', fontFamily: FONT}}>
        <g opacity={intro}>
          {/* column headers */}
          <text x={APP_X} y={66} fontSize={24} fontWeight={700} fill={P.text}>
            any app
          </text>
          <text x={LANE.x} y={66} fontSize={24} fontWeight={700} fill={P.teal} opacity={0.4 + 0.6 * take}>
            flashcp driver
          </text>

          {/* apps */}
          {APPS.map((name, a) => (
            <g key={name}>
              <rect
                x={APP_X}
                y={APP_YS[a]}
                width={APP_W}
                height={APP_H}
                rx={12}
                fill={P.panel}
                stroke={P.border}
                strokeWidth={2}
              />
              <rect
                x={APP_X}
                y={APP_YS[a]}
                width={APP_W}
                height={APP_H}
                rx={12}
                fill="none"
                stroke={P.text}
                strokeWidth={2.5}
                opacity={appsBusy * keep}
              />
              <AppIcon kind={a} x={APP_X + 22} y={APP_YS[a] + 19} color={P.muted} />
              <text x={APP_X + 70} y={APP_YS[a] + 44} fontSize={24} fill={P.text}>
                {name}
              </text>
            </g>
          ))}
          <text x={APP_X} y={368} fontSize={22} fill={P.muted} opacity={ramp(f, APP_DONE, APP_DONE + 8) * keep}>
            {'a cp -r of all 20,000 files returns after '}
            <tspan fontWeight={700} fill={P.text}>{`${fmt(DRIVE_MODE.appDoneS)} s`}</tspan>
          </text>

          {/* staging lane */}
          <g opacity={0.4 + 0.6 * take}>
            <rect x={LANE.x} y={LANE.y} width={LANE.w} height={LANE.h} rx={14} fill={P.panel} stroke={P.border} strokeWidth={2} />
            <rect
              x={LANE.x}
              y={LANE.y}
              width={LANE.w}
              height={LANE.h}
              rx={14}
              fill="none"
              stroke={P.teal}
              strokeWidth={2.5}
              opacity={take}
            />
            {Array.from({length: N}, (_, i) => (
              <rect
                key={i}
                x={SLOT_X0 + (i % 6) * SLOT_PX}
                y={SLOT_Y0 + Math.floor(i / 6) * SLOT_PY}
                width={TILE_W}
                height={TILE_H}
                rx={5}
                fill={P.empty}
              />
            ))}
            <text x={LANE.x + LANE.w / 2} y={302} textAnchor="middle" fontSize={22} fill={P.muted}>
              staged at once, saved in order
            </text>
          </g>

          {/* pipe from the lane to the USB port */}
          <rect
            x={LANE.x + LANE.w - 1}
            y={PIPE_Y - 17}
            width={STICK.x - 54 - (LANE.x + LANE.w) + 1}
            height={34}
            fill={P.panel}
            stroke={P.border}
            strokeWidth={2}
            opacity={0.4 + 0.6 * take}
          />
          <rect x={STICK.x - 54} y={PIPE_Y - 34} width={20} height={68} rx={4} fill={P.grid} />

          {/* stick */}
          <g transform={`translate(${stickDx} 0)`} opacity={stickA}>
            <rect x={STICK.x - 34} y={PIPE_Y - 25} width={38} height={50} rx={4} fill={P.grid} />
            <rect x={STICK.x - 24} y={PIPE_Y - 13} width={10} height={10} fill={P.bg} opacity={0.7} />
            <rect x={STICK.x - 24} y={PIPE_Y + 3} width={10} height={10} fill={P.bg} opacity={0.7} />
            <rect x={STICK.x} y={STICK.y} width={STICK.w} height={STICK.h} rx={16} fill={P.panel} stroke={stickStroke} strokeWidth={2.5} />
            <text x={BAR.x} y={STICK.y + 36} fontSize={24} fontWeight={700} fill={P.text}>
              {DRIVE_MODE.label}
            </text>
            {Array.from({length: BAR.cells}, (_, c) => {
              const cw = BAR.w / BAR.cells;
              const frac = clamp01(saved * BAR.cells - c);
              return (
                <g key={c}>
                  <rect x={BAR.x + c * cw} y={BAR.y} width={cw - 2} height={BAR.h} rx={2} fill={P.empty} />
                  <rect x={BAR.x + c * cw} y={BAR.y} width={(cw - 2) * frac} height={BAR.h} rx={2} fill={P.teal} opacity={keep} />
                </g>
              );
            })}
            <text x={BAR.x} y={STICK.y + 128} fontSize={22} fill={P.muted} opacity={1 - take}>
              exFAT stick
            </text>
            <text x={BAR.x} y={STICK.y + 128} fontSize={22} fontWeight={700} fill={P.teal} opacity={take}>
              served by flashcp ✓
            </text>
          </g>

          {/* files: app -> staging lane -> stick */}
          {TILES.map((t, i) => {
            if (f < t.launch || f >= t.leave + STREAM_FLIGHT) return null;
            if (f < t.leave) {
              const p = ramp(f, t.launch, t.land, 0, 1, EASE_OUT);
              const x0 = APP_X + APP_W - 14;
              const y0 = APP_YS[t.app] + APP_H / 2;
              const x = lerp(x0, t.sx, p);
              const y = lerp(y0, t.sy, p) - 26 * 4 * p * (1 - p);
              // lands slightly oversized, then settles
              const s = lerp(0.55, 1.12, p) - 0.12 * ramp(f, t.land, t.land + 7, 0, 1, EASE_OUT);
              return <FileTile key={i} cx={x} cy={y} s={s} opacity={ramp(f, t.launch, t.launch + 2) * keep} P={P} />;
            }
            const s = lerp(1, 0.7, ramp(f, t.leave, t.leave + 8, 0, 1, EASE_IO));
            const a = 1 - ramp(f, t.leave + STREAM_FLIGHT - 4, t.leave + STREAM_FLIGHT);
            return <FileTile key={i} cx={t.outX(f)} cy={t.outY(f)} s={s} opacity={a * keep} P={P} />;
          })}

          {/* flashcp clock and eject */}
          <g opacity={keep * (copying ? 1 : 0)}>
            <text
              x={STICK.x}
              y={STICK.y + STICK.h + 44}
              fontFamily={MONO}
              fontSize={32}
              fontWeight={600}
              fill={P.teal}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {`${fmt(flashClock)} s`}
            </text>
          </g>
          <g opacity={stickA} transform={scaleAt(1075, 332, 1 - 0.07 * ejectPress)}>
            <circle cx={1075} cy={332} r={22 + 34 * ripple} fill="none" stroke={P.teal} strokeWidth={2.5} opacity={(1 - ripple) * (f >= EJECT ? 0.8 : 0)} />
            <rect x={1010} y={312} width={130} height={40} rx={20} fill={P.panel} stroke={P.border} strokeWidth={2} />
            <rect x={1010} y={312} width={130} height={40} rx={20} fill={P.teal} opacity={0.35 * ejectPress} />
            <path d="M1036 334 l10 -12 l10 12 z M1036 338 h20 v4 h-20 z" fill={P.text} />
            <text x={1066} y={340} fontSize={22} fill={P.text}>
              Eject
            </text>
          </g>

          {/* the notification */}
          <g opacity={toastIn * keep} transform={`translate(0 ${(1 - toastIn) * -22})`}>
            <rect x={TOAST_BOX.x} y={TOAST_BOX.y} width={TOAST_BOX.w} height={TOAST_BOX.h} rx={14} fill={P.panel} stroke={done ? P.teal : P.border} strokeWidth={2.5} />
            {/* saving: a turning arc; done: a tick */}
            <g opacity={1 - doneP}>
              <circle cx={TOAST_BOX.x + 28} cy={TOAST_BOX.y + 54} r={12} fill="none" stroke={P.grid} strokeWidth={4} />
              <path
                d={`M${TOAST_BOX.x + 28} ${TOAST_BOX.y + 42} a12 12 0 0 1 12 12`}
                fill="none"
                stroke={P.teal}
                strokeWidth={4}
                strokeLinecap="round"
                transform={`rotate(${(f - TOAST) * 14} ${TOAST_BOX.x + 28} ${TOAST_BOX.y + 54})`}
              />
            </g>
            <g opacity={doneP} transform={scaleAt(TOAST_BOX.x + 28, TOAST_BOX.y + 54, pop(f, DONE, fps))}>
              <circle cx={TOAST_BOX.x + 28} cy={TOAST_BOX.y + 54} r={14} fill={P.teal} />
              <path
                d={`M${TOAST_BOX.x + 21} ${TOAST_BOX.y + 54} l5 6 l9 -11`}
                fill="none"
                stroke={P.pillText}
                strokeWidth={3.5}
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </g>
            <g opacity={1 - doneP}>
              <text x={TOAST_BOX.x + 52} y={TOAST_BOX.y + 34} fontSize={22} fontWeight={700} fill={P.text} style={{fontVariantNumeric: 'tabular-nums'}}>
                {`Saving ${pendingMb} MB to ${DRIVE_MODE.label}…`}
              </text>
              {/* the notification's real body text, on two lines */}
              <text x={TOAST_BOX.x + 52} y={TOAST_BOX.y + 63} fontSize={22} fill={P.muted}>
                Don't remove the drive yet: you'll
              </text>
              <text x={TOAST_BOX.x + 52} y={TOAST_BOX.y + 90} fontSize={22} fill={P.muted}>
                be told when it can be removed.
              </text>
            </g>
            <g opacity={doneP}>
              <text x={TOAST_BOX.x + 52} y={TOAST_BOX.y + 46} fontSize={22} fontWeight={700} fill={P.text}>
                {`${DRIVE_MODE.label} can be removed`}
              </text>
              <text x={TOAST_BOX.x + 52} y={TOAST_BOX.y + 76} fontSize={22} fill={P.muted}>
                Everything was saved.
              </text>
            </g>
          </g>

          {/* kernel driver strip */}
          <line x1={60} y1={392} x2={930} y2={392} stroke={P.grid} strokeWidth={1.5} />
          <text x={K.x} y={436} fontSize={24} fontWeight={700} fill={P.coral}>
            kernel driver, same stick and format
          </text>
          <text x={500} y={436} fontSize={22} fill={P.muted}>
            one metadata round trip per file
          </text>
          {Array.from({length: K.cols * K.rows}, (_, c) => {
            const col = Math.floor(c / K.rows);
            const row = c % K.rows;
            const isMeta = col < K.metaCols;
            return (
              <rect
                key={c}
                x={K.x + col * K.px}
                y={K.y + row * K.py}
                width={K.cw}
                height={K.ch}
                rx={3}
                fill={isMeta ? P.amber : P.empty}
                fillOpacity={isMeta ? 0.22 : 1}
                stroke={isMeta ? P.amber : 'none'}
                strokeWidth={isMeta ? 1.5 : 0}
              />
            );
          })}
          <g opacity={keep * (copying ? 1 : 0)}>
            {Array.from({length: Math.min(K_DATA, kn.files + 1)}, (_, d) => {
              const c = kData(d);
              const frac = d < kn.files ? 1 : kn.writing;
              return <rect key={d} x={c.x} y={c.y} width={K.cw * frac} height={K.ch} rx={3} fill={P.coral} />;
            })}
            {kn.metaFlash > 0 ? (
              <rect x={kMeta(kn.metaCell).x} y={kMeta(kn.metaCell).y} width={K.cw} height={K.ch} rx={3} fill={P.amber} opacity={0.35 + 0.65 * kn.metaFlash} />
            ) : null}
            {kn.hop ? (
              <line
                x1={kn.hop.from.x + K.cw / 2}
                y1={kn.hop.from.y + K.ch / 2}
                x2={kn.hop.to.x + K.cw / 2}
                y2={kn.hop.to.y + K.ch / 2}
                stroke={P.coral}
                strokeWidth={2}
                strokeLinecap="round"
                opacity={0.5}
              />
            ) : null}
            <rect x={kn.head.x - 4} y={kn.head.y - 4} width={K.cw + 8} height={K.ch + 8} rx={5} fill={P.coral} fillOpacity={0.16} stroke={P.coral} strokeWidth={3} />
            <text
              x={950}
              y={482}
              fontFamily={MONO}
              fontSize={32}
              fontWeight={600}
              fill={P.coral}
              style={{fontVariantNumeric: 'tabular-nums'}}
            >
              {`${fmt(realS)} s`}
            </text>
            <text x={950} y={512} fontSize={22} fill={P.muted} opacity={kernelDone}>
              {`of ${DRIVE_MODE.kernelLabel} s`}
            </text>
          </g>

          {/* result badge */}
          <g opacity={keep} transform={scaleAt(1045, 392, pop(f, BADGE, fps))}>
            <rect x={950} y={368} width={190} height={48} rx={24} fill={P.teal} />
            <text x={1045} y={401} textAnchor="middle" fontSize={24} fontWeight={700} fill={P.pillText}>
              {`${DRIVE_MODE.speedup} sooner`}
            </text>
          </g>

          <text x={60} y={566} fontSize={22} fill={P.muted}>
            {DRIVE_MODE.footnote}
          </text>
        </g>
      </svg>
    </AbsoluteFill>
  );
};
