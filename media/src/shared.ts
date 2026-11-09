import {interpolate} from 'remotion';

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
