import {Easing, interpolate} from 'remotion';
import {springStep} from './motion';

export type Theme = 'light' | 'dark';

export const FPS = 30;
export const FONT = 'Inter, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif';
export const MONO = 'ui-monospace, SFMono-Regular, Menlo, monospace';

export type Palette = {
  bg: string;
  text: string;
  muted: string;
  empty: string;
  grid: string;
  coral: string;
  teal: string;
  tealAlt: string;
  amber: string;
  pillText: string;
  /** card / panel background, one step off the page background */
  panel: string;
  /** card outline */
  border: string;
  /** a file that is simply there (staged, or already in sync) */
  tile: string;
};

export const PALETTES: Record<Theme, Palette> = {
  light: {
    bg: '#ffffff',
    text: '#1f2328',
    muted: '#59636e',
    empty: '#eaeef2',
    grid: '#d8dee4',
    coral: '#e5534b',
    teal: '#0f9d8a',
    tealAlt: '#7dcfc2',
    amber: '#d4a72c',
    pillText: '#ffffff',
    panel: '#f6f8fa',
    border: '#d0d7de',
    tile: '#8c959f',
  },
  dark: {
    bg: '#0d1117',
    text: '#e6edf3',
    muted: '#9198a1',
    empty: '#21262d',
    grid: '#30363d',
    coral: '#f47067',
    teal: '#2dd4bf',
    tealAlt: '#178f80',
    amber: '#e3b341',
    pillText: '#0d1117',
    panel: '#161b22',
    border: '#3d444d',
    tile: '#7d8590',
  },
};

/** Clamped linear ramp (or eased) from v0 at frame a to v1 at frame b. Pure function of the frame. */
export const ramp = (
  f: number,
  a: number,
  b: number,
  v0 = 0,
  v1 = 1,
  easing?: (t: number) => number,
): number =>
  interpolate(f, [a, b], [v0, v1], {
    extrapolateLeft: 'clamp',
    extrapolateRight: 'clamp',
    easing,
  });

/** 20000 -> "20,000" without locale dependence. */
export const fmtInt = (n: number): string =>
  String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ',');

/** Seeded PRNG (mulberry32). Deterministic across frames and render workers. */
export const mulberry32 = (seed: number) => {
  let a = seed >>> 0;
  return (): number => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
};

// Easing vocabulary shared by the newer compositions (one curve per job).
/** Something enters or arrives: fast start, long settle. */
export const EASE_OUT = Easing.bezier(0.16, 1, 0.3, 1);
/** Something already on screen moves from A to B. */
export const EASE_IO = Easing.bezier(0.45, 0, 0.2, 1);
/** Something leaves or launches. */
export const EASE_IN = Easing.bezier(0.5, 0, 0.9, 0.4);

/** Pop-in scale: 0 before `start`, springs to 1 with a small overshoot. Pure function of the frame. */
export const pop = (f: number, start: number, fps = FPS): number =>
  springStep((f - start) / fps, {stiffness: 190, damping: 15, mass: 0.7});

/** Soft settle without overshoot (critically damped), for opacity and positions. */
export const settle = (f: number, start: number, fps = FPS): number =>
  springStep((f - start) / fps, {stiffness: 170, damping: 200});

/** SVG transform that scales around a point. */
export const scaleAt = (cx: number, cy: number, s: number): string =>
  `translate(${cx} ${cy}) scale(${s}) translate(${-cx} ${-cy})`;

/** 12.34 -> "12.3" with a fixed number of decimals (no locale dependence). */
export const fmt = (n: number, decimals = 1): string => n.toFixed(decimals);
