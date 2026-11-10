/**
 * motion.ts: pure, deterministic helpers for smooth Remotion animation (the subset the README
 * animations use, from the `remotion-smooth-motion` helper library). No dependencies. Every
 * function is a pure function of its inputs and accepts fractional frames, so it is safe for
 * Remotion's out-of-order, multi-tab rendering.
 *
 *   - track()        multi-key curves with continuous velocity (no hitch at the keys)
 *   - springStep()   closed-form spring, same physics as Remotion's spring()
 *   - smoothstep(), bump(), clamp01(), lerp()
 */

// ---------------------------------------------------------------------------
// Basics
// ---------------------------------------------------------------------------

export const clamp01 = (x: number) => (x < 0 ? 0 : x > 1 ? 1 : x);
export const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

/** Hermite 3t^2-2t^3: zero velocity at both ends. */
export const smoothstep = (t: number) => {
  const x = clamp01(t);
  return x * x * (3 - 2 * x);
};

/** Smooth bump on [0,1]: 0 at both ends with zero slope, 1 at the middle (16 s^2 (1-s)^2). */
export const bump = (s: number) => {
  const x = clamp01(s);
  return 16 * x * x * (1 - x) * (1 - x);
};

// ---------------------------------------------------------------------------
// Keyframe tracks
// ---------------------------------------------------------------------------

export type Key = {
  /** Frame (may be fractional). Keys must be sorted by f. */
  f: number;
  /** Value at this key. */
  v: number;
  /**
   * Optional override of the outgoing segment:
   *  - 'hold'   stay at v until the next key, then jump (stepped / "on twos" look)
   *  - 'linear' constant speed to next key (mechanical motion, planted feet)
   *  - a function (0..1)->(0..1), e.g. Easing.bezier(...) for a hand-tuned segment.
   *    Using a custom easing breaks velocity continuity at that key on purpose.
   */
  out?: 'hold' | 'linear' | ((t: number) => number);
  /**
   * Optional explicit velocity at this key, in value-per-frame (After Effects "speed").
   * Use it to hand off velocity to/from another curve, e.g. a jump's takeoff speed into
   * a ballistic arc, so the join has no hitch.
   */
  slope?: number;
};

export type TrackMode =
  /** Monotone cubic (Fritsch-Carlson). Never overshoots keys, flat at local extremes,
   *  flows THROUGH in-between keys. Best default for poses, UI and camera. */
  | 'monotone'
  /** Catmull-Rom. Smoothest flow, may overshoot slightly past keys. Good for paths and loose,
   *  organic motion where a little overshoot reads as life. */
  | 'catmull';

export type TrackOptions = {
  mode?: TrackMode;
  /** Velocity at the first/last key: 'ease' (0, slow-in/slow-out) or 'flow' (keep the slope). */
  ends?: 'ease' | 'flow';
  /** Before the first / after the last key: hold the end value (default) or extend the tangent. */
  extrapolate?: 'clamp' | 'extend';
};

/**
 * Build a smooth curve through keyframes. Unlike chaining `interpolate()` segments with
 * ease-in-out on each, the curve does NOT stop at every key: velocity is continuous through
 * in-between keys and only reaches zero at real turning points, holds and (by default) the ends.
 *
 *   const armAngle = track([{f:0,v:-20},{f:12,v:35},{f:20,v:28},{f:40,v:28}]);
 *   armAngle(frame)
 *
 * Repeated equal values form a clean hold. Build tracks once at module scope or in useMemo.
 */
export function track(keys: readonly Key[], opts: TrackOptions = {}): (frame: number) => number {
  const { mode = 'monotone', ends = 'ease', extrapolate = 'clamp' } = opts;
  if (keys.length === 0) throw new Error('track(): need at least one key');
  for (let i = 1; i < keys.length; i++) {
    if (!(keys[i].f > keys[i - 1].f)) throw new Error(`track(): key frames must strictly increase (index ${i})`);
  }
  const n = keys.length;
  if (n === 1) return () => keys[0].v;

  const f = keys.map((k) => k.f);
  const v = keys.map((k) => k.v);
  const h = f.slice(1).map((x, i) => x - f[i]);
  const d = h.map((hi, i) => (v[i + 1] - v[i]) / hi); // secant slopes
  const m = new Array<number>(n).fill(0); // tangents (value per frame)

  if (mode === 'catmull') {
    for (let i = 1; i < n - 1; i++) m[i] = (v[i + 1] - v[i - 1]) / (f[i + 1] - f[i - 1]);
  } else {
    // Fritsch-Carlson monotone tangents (weighted harmonic mean, non-uniform spacing).
    for (let i = 1; i < n - 1; i++) {
      if (d[i - 1] === 0 || d[i] === 0 || Math.sign(d[i - 1]) !== Math.sign(d[i])) {
        m[i] = 0;
      } else {
        const w1 = 2 * h[i] + h[i - 1];
        const w2 = h[i] + 2 * h[i - 1];
        m[i] = (w1 + w2) / (w1 / d[i - 1] + w2 / d[i]);
      }
    }
  }
  if (ends === 'flow') {
    m[0] = d[0];
    m[n - 1] = d[n - 2];
  }
  // Keys that start or end a special segment get a flat or linear tangent so the joint is clean.
  for (let i = 0; i < n - 1; i++) {
    const out = keys[i].out;
    if (out === 'hold') {
      m[i] = 0;
      m[i + 1] = 0;
    } else if (out === 'linear') {
      m[i] = d[i];
      m[i + 1] = d[i];
    }
  }
  for (let i = 0; i < n; i++) if (keys[i].slope !== undefined) m[i] = keys[i].slope as number;

  const evalSeg = (i: number, x: number) => {
    const out = keys[i].out;
    const t = (x - f[i]) / h[i];
    if (out === 'hold') return v[i];
    if (out === 'linear') return v[i] + (v[i + 1] - v[i]) * t;
    if (typeof out === 'function') return v[i] + (v[i + 1] - v[i]) * out(t);
    const t2 = t * t;
    const t3 = t2 * t;
    return (
      (2 * t3 - 3 * t2 + 1) * v[i] +
      (t3 - 2 * t2 + t) * h[i] * m[i] +
      (-2 * t3 + 3 * t2) * v[i + 1] +
      (t3 - t2) * h[i] * m[i + 1]
    );
  };

  return (x: number) => {
    if (x <= f[0]) return extrapolate === 'extend' ? v[0] + (x - f[0]) * m[0] : v[0];
    if (x >= f[n - 1]) return extrapolate === 'extend' ? v[n - 1] + (x - f[n - 1]) * m[n - 1] : v[n - 1];
    // binary search for the segment
    let lo = 0;
    let hi = n - 1;
    while (hi - lo > 1) {
      const mid = (lo + hi) >> 1;
      if (f[mid] <= x) lo = mid;
      else hi = mid;
    }
    return evalSeg(lo, x);
  };
}

// ---------------------------------------------------------------------------
// Closed-form springs (stateless, fractional-frame safe)
// ---------------------------------------------------------------------------

export type SpringParams = { stiffness?: number; damping?: number; mass?: number };

/**
 * Unit step response of a damped spring, 0 at t<=0 and settling at 1.
 * Same physics/units as Remotion's spring() config (defaults: mass 1, stiffness 100, damping 10).
 * `t` is in SECONDS.
 */
export function springStep(t: number, { stiffness = 100, damping = 10, mass = 1 }: SpringParams = {}): number {
  if (t <= 0) return 0;
  const w0 = Math.sqrt(stiffness / mass);
  const zeta = damping / (2 * Math.sqrt(stiffness * mass));
  if (zeta < 1) {
    const wd = w0 * Math.sqrt(1 - zeta * zeta);
    return 1 - Math.exp(-zeta * w0 * t) * (Math.cos(wd * t) + ((zeta * w0) / wd) * Math.sin(wd * t));
  }
  // Remotion's spring() treats every damping ratio >= 1 as critically damped (so damping: 200
  // settles in ~1s instead of crawling). Match that so values agree with spring()/Easing.spring().
  return 1 - Math.exp(-w0 * t) * (1 + w0 * t);
}

