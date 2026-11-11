#!/usr/bin/env node
// Static charts for the README, written as plain SVG: assets/chart-<name>-{light,dark}.svg.
//
//   node charts.mjs            (or: npm run charts)   no dependencies, no network
//
// Every number is in the data blocks below with its source next to it. The sources are the
// same ones `src/data/benchmarks.ts` uses for the animations:
//   [K]   Kingston DataTraveler 3.0 128 GB (USB 3, usb-storage/BOT), 2026-11-10 afternoon,
//         dataset `small` (20,000 files / 169 MB), SINGLE runs.
//   [K2]  the same stick at 18:09, kernel driver only, phases timed:
//         4 KB clusters:   mkfs 1.53 | mount 0.38 | cp 0.73 | unmount 49.56 | all 52.20 s
//         128 KB clusters: mkfs 0.08 | mount 0.38 | cp 0.61 | unmount 287.65 | all 288.72 s
//   [#18] PR 18 (Kingston, morning session), [#22] PR 22 (drive mode core, medians of 3),
//   [#26] PR 26 (in-place re-cluster, single runs), [#9] PR 9 (SanDisk),
//   [#7] PR 7 and [#6] PR 6 (MSI SSD and SanDisk build / add / pull, medians of 3).
// A number that is not in one of these does not belong in a chart. When a number changes,
// change the README table and the image's alt text too: the alt text states every number shown.
//
// Colours are the animations' (src/shared.ts): coral is the Linux kernel driver, teal is
// flashcp, amber is the drive's own limit. Text is never drawn in a series colour.

import {mkdirSync, writeFileSync} from 'node:fs';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const ASSETS = join(dirname(fileURLToPath(import.meta.url)), '..', 'assets');

const THEMES = {
  light: {
    bg: '#ffffff', text: '#1f2328', muted: '#59636e', grid: '#d8dee4', rowLine: '#eaeef2',
    coral: '#e5534b', teal: '#0f9d8a', amber: '#b98a0b',
  },
  dark: {
    bg: '#0d1117', text: '#e6edf3', muted: '#9198a1', grid: '#30363d', rowLine: '#21262d',
    coral: '#f47067', teal: '#2dd4bf', amber: '#e3b341',
  },
};

const FONT = '-apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, Arial, sans-serif';
const W = 800;
const PAD = 24;

// ---------------------------------------------------------------------------------- helpers

const esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const n = (v) => Number(v.toFixed(1));

const text = (x, y, s, o = {}) => {
  const {size = 13, weight = 400, fill, anchor = 'start'} = o;
  return `<text x="${n(x)}" y="${n(y)}" font-size="${size}" font-weight="${weight}" fill="${fill}" text-anchor="${anchor}">${esc(s)}</text>`;
};

/** A run of differently styled pieces on one line: [[string, {weight, fill}], ...]. `halo` is
 *  a surface-coloured outline, for a label that a grid line passes behind. */
const spans = (x, y, parts, halo) =>
  `<text x="${n(x)}" y="${n(y)}" font-size="13"${halo ? ` stroke="${halo}" stroke-width="5" stroke-linejoin="round" paint-order="stroke"` : ''}>` +
  parts
    .map(([s, o]) => `<tspan font-weight="${o.weight ?? 400}" fill="${o.fill}">${esc(s)}</tspan>`)
    .join('') +
  `</text>`;

const hline = (x0, x1, y, stroke) =>
  `<line x1="${n(x0)}" x2="${n(x1)}" y1="${n(y)}" y2="${n(y)}" stroke="${stroke}" stroke-width="1"/>`;
const vline = (x, y0, y1, stroke) =>
  `<line x1="${n(x)}" x2="${n(x)}" y1="${n(y0)}" y2="${n(y1)}" stroke="${stroke}" stroke-width="1"/>`;

/** A horizontal bar from the baseline: square at x, 4 px rounded at the data end. */
const hbar = (x, y, w, h, fill, opacity = 1, round = true) => {
  const r = round ? Math.min(4, w / 2) : 0;
  const d = `M${n(x)},${n(y)}H${n(x + w - r)}Q${n(x + w)},${n(y)} ${n(x + w)},${n(y + r)}V${n(y + h - r)}Q${n(x + w)},${n(y + h)} ${n(x + w - r)},${n(y + h)}H${n(x)}Z`;
  return `<path d="${d}" fill="${fill}"${opacity < 1 ? ` fill-opacity="${opacity}"` : ''}/>`;
};

/** A column from the baseline: square at the bottom, 4 px rounded on the cap. */
const vbar = (x, yBase, w, h, fill) => {
  const r = Math.min(4, h / 2);
  const y = yBase - h;
  const d = `M${n(x)},${n(yBase)}V${n(y + r)}Q${n(x)},${n(y)} ${n(x + r)},${n(y)}H${n(x + w - r)}Q${n(x + w)},${n(y)} ${n(x + w)},${n(y + r)}V${n(yBase)}Z`;
  return `<path d="${d}" fill="${fill}"/>`;
};

const dot = (x, y, fill, bg) =>
  `<circle cx="${n(x)}" cy="${n(y)}" r="5" fill="${fill}" stroke="${bg}" stroke-width="2"/>`;
/** A measured range: a pill whose round ends sit on the two values. */
const pill = (x0, x1, y, fill, bg) =>
  `<rect x="${n(x0 - 5)}" y="${n(y - 5)}" width="${n(x1 - x0 + 10)}" height="10" rx="5" fill="${fill}" stroke="${bg}" stroke-width="2"/>`;
const mark = (x0, x1, y, fill, bg) => (x1 > x0 ? pill(x0, x1, y, fill, bg) : dot(x0, y, fill, bg));
const diamond = (x, y, fill, bg) =>
  `<path d="M${n(x)},${n(y - 6.5)}L${n(x + 6.5)},${n(y)}L${n(x)},${n(y + 6.5)}L${n(x - 6.5)},${n(y)}Z" fill="${fill}" stroke="${bg}" stroke-width="2"/>`;
const swatch = (x, y, fill) => `<rect x="${n(x)}" y="${n(y - 9)}" width="10" height="10" rx="2" fill="${fill}"/>`;

/** A legend row: [[kind, colour, label], ...], kind = 'square' | 'dot' | 'diamond'. Returns [svg, endX]. */
const legend = (c, x, y, items) => {
  let out = '';
  for (const [kind, fill, label] of items) {
    if (kind === 'square') out += swatch(x, y, fill);
    else if (kind === 'diamond') out += diamond(x + 5, y - 4, fill, c.bg);
    else out += dot(x + 5, y - 4, fill, c.bg);
    out += text(x + 17, y, label, {size: 12.5, fill: c.muted});
    x += 17 + String(label).length * 12.5 * 0.5 + 24;
  }
  return out;
};

/** The frame every chart shares: surface, title, subtitle, body, footnotes. */
const frame = (c, height, {title, subtitle, desc}, body, notes = []) => {
  let out = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${height}" width="${W}" height="${height}" font-family='${FONT}' role="img" aria-label="${esc(title)}">\n`;
  out += `<title>${esc(title)}</title>\n<desc>${esc(desc)}</desc>\n`;
  out += `<rect width="${W}" height="${height}" fill="${c.bg}"/>\n`;
  out += text(PAD, 34, title, {size: 17, weight: 600, fill: c.text}) + '\n';
  out += text(PAD, 55, subtitle, {size: 13, fill: c.muted}) + '\n';
  out += body + '\n';
  notes.forEach((s, i) => {
    out += text(PAD, height - 14 - (notes.length - 1 - i) * 17, s, {size: 12, fill: c.muted}) + '\n';
  });
  return out + '</svg>\n';
};

// ------------------------------------------------------------------------- 1. speed-up overview
// How many times faster flashcp is than the Linux kernel exfat driver, dataset `small`.
// [name, lo, hi, label]: lo–hi is a measured range (equal = one figure), the label is the
// README table's own figure.

const SPEEDUP = [
  {
    drive: 'USB SSD · MSI DATAMAG',
    note: 'medians of 3',
    rows: [
      ['build a blank drive', 6.7, 6.7, '6.7×'], // [#6] 2.80 / 0.42
      ['add to a drive with files', 3.6, 3.6, '3.6×'], // [#7] 1.72 / 0.48
      ['pull from a kernel-filled drive', 9.1, 9.1, '9.1×'], // [#7] 6.01 / 0.66
      ['pull from a flashcp-built drive', 14.6, 14.6, '14.6×'], // [#7] 5.58 / 0.38
      ['drive mode, until safe to remove', 1.4, 1.4, '1.4×'], // [#22] 1.80 / 1.26
    ],
  },
  {
    drive: 'USB 3 stick · Kingston DataTraveler 3.0',
    note: 'single runs unless noted',
    rows: [
      ['build a blank stick, 4 KB clusters for both', 5.8, 27.2, '6–27×'], // [K][K2][#18] 52.2 / 9.02 .. 74.0 / 2.72
      ['drive mode, 4 KB clusters for both', 9.5, 13.9, '9–14×'], // [K][K2] 50.3 / 5.31 .. 74.0 / 5.31
      ['drive mode, default 128 KB clusters for both', 2.8, 4.0, '2.8–4.0×'], // [K][K2] 258.8 / 93.8 .. 370.7 / 93.8
      ['add, default 128 KB clusters for both', 5.7, 5.7, '5.7×'], // [#18] 527 / 93.3
      ['pull from a kernel-filled stick (median of 3)', 1.4, 1.4, '1.4×'], // [#18] 11.3 / 7.96
      ['pull from a flashcp-built stick (median of 3)', 6.2, 6.2, '6.2×'], // [#18] 7.2 / 1.15
    ],
  },
  {
    drive: 'USB 2.0 stick · SanDisk Cruzer Blade',
    note: 'build and pull: medians of 3 · add: single runs',
    rows: [
      ['build (4 KB) vs kernel on its default 32 KB format', 2.5, 5.3, '2.5–5.3×'], // [#9][#6] 86.3 / 34.6 .. 179.6 / 33.6
      ['add, 32 KB clusters for both', 1.8, 2.5, '1.8–2.5×'], // [#9] 209.8 / 114 .. 209.8 / 84.1
      ['pull from a kernel-filled stick', 1.05, 1.05, '1.05×'], // [#7] 30.86 / 29.52
      ['pull from a flashcp-built stick', 2.0, 2.0, '2.0×'], // [#7] 32.78 / 16.38
    ],
  },
];

function speedup(c) {
  const x0 = 352;
  const x1 = 712;
  const ticks = [1, 2, 5, 10, 20, 50];
  const X = (v) => x0 + ((x1 - x0) * Math.log10(v)) / Math.log10(50);
  const ROW = 25;
  const HEAD = 36;
  const top = 96;
  const rows = SPEEDUP.reduce((a, g) => a + g.rows.length, 0);
  const bottom = top + SPEEDUP.length * HEAD + rows * ROW + 4;
  const height = bottom + 62;

  let b = '';
  for (const t of ticks) {
    const label = t === 1 ? '1× (no gain)' : `${t}×`;
    b += text(X(t), top - 2, label, {size: 12, fill: c.muted, anchor: 'middle'});
    b += text(X(t), bottom + 17, label, {size: 12, fill: c.muted, anchor: 'middle'});
  }
  let y = top;
  for (const g of SPEEDUP) {
    y += HEAD;
    b += spans(PAD, y - 12, [[g.drive, {weight: 600, fill: c.text}], [`   ${g.note}`, {fill: c.muted}]]);
    // the grid covers the rows only, so the group headings stay clear of it
    const y1 = y + g.rows.length * ROW;
    for (const t of ticks) b += vline(X(t), y, y1, t === 1 ? c.muted : c.grid);
    for (const [name, lo, hi, label] of g.rows) {
      const cy = y + ROW / 2;
      b += hline(x0, x1, cy, c.rowLine);
      b += text(x0 - 14, cy + 4.5, name, {size: 13, fill: c.text, anchor: 'end'});
      b += mark(X(lo), X(hi), cy, c.teal, c.bg);
      b += text(X(hi) + 13, cy + 4.5, label, {size: 13, weight: 600, fill: c.text});
      y += ROW;
    }
  }
  return frame(
    c,
    height,
    {
      title: 'How many times faster than the Linux kernel driver',
      subtitle: '20,000 small files (169 MB), the same job done both ways · a wide mark is the spread between runs',
      desc: 'Dot plot on a logarithmic axis from 1 to 50 times. ' +
        SPEEDUP.map((g) => `${g.drive}: ${g.rows.map(([name, , , label]) => `${name} ${label}`).join('; ')}.`).join(' '),
    },
    b,
    ['Logarithmic axis. Each row is one job timed both ways, to the end (unmount or sync included). The benchmark tables give the seconds.'],
  );
}

// ------------------------------------------------------------- 2. cp returns vs safe to remove
// Kingston, `small`, from the start of `cp` until the stick can be removed.
// Kernel: cp + unmount [K2]. Drive mode: cp + unmount + final commit [K].

const SAFE = [
  {
    group: 'Stick formatted the usual way (128 KB clusters)',
    rows: [
      ['Linux kernel driver', 'coral', 288.3, '288 s', 'cp returned after 0.61 s'], // [K2] 0.61 + 287.65
      ['flashcp drive mode', 'teal', 93.8, '93.8 s', 'cp returned after 0.93 s'], // [K]
    ],
  },
  {
    group: 'The same stick with 4 KB clusters',
    rows: [
      ['Linux kernel driver', 'coral', 50.3, '50.3 s', 'cp returned after 0.73 s'], // [K2] 0.73 + 49.56
      ['flashcp drive mode', 'teal', 5.31, '5.31 s', 'cp returned after 0.93 s'], // [K]
    ],
  },
];

function safe(c) {
  const x0 = 176;
  const x1 = 548;
  const max = 300;
  const X = (v) => x0 + ((x1 - x0) * v) / max;
  const ROW = 30;
  const HEAD = 38;
  const top = 104;
  const bottom = top + SAFE.length * HEAD + 4 * ROW + 6;
  const height = bottom + 78;

  let b = legend(c, PAD, 82, [
    ['square', c.coral, 'Linux kernel driver'],
    ['square', c.teal, 'flashcp drive mode'],
  ]);
  for (const t of [0, 100, 200, 300]) b += text(X(t), bottom + 17, `${t} s`, {size: 12, fill: c.muted, anchor: 'middle'});
  let y = top;
  for (const g of SAFE) {
    y += HEAD;
    b += text(PAD, y - 12, g.group, {size: 13, weight: 600, fill: c.text});
    for (const t of [100, 200, 300]) b += vline(X(t), y, y + g.rows.length * ROW, c.grid);
    for (const [name, col, secs, label, sub] of g.rows) {
      const cy = y + ROW / 2;
      b += text(x0 - 12, cy + 4.5, name, {size: 13, fill: c.text, anchor: 'end'});
      b += hbar(X(0), cy - 9, X(secs) - X(0), 18, c[col]);
      const lx = X(secs) + 9;
      b += spans(lx, cy + 4.5, [[label, {weight: 600, fill: c.text}], [`  ·  ${sub}`, {fill: c.muted}]], c.bg);
      y += ROW;
    }
    b += vline(X(0), y - g.rows.length * ROW, y, c.muted);
  }
  return frame(
    c,
    height,
    {
      title: 'cp returns in under a second. The stick is done much later.',
      subtitle: '20,000 small files (169 MB) onto the Kingston USB 3 stick: seconds until it is safe to remove',
      desc: 'Bar chart, seconds from the start of cp until the stick can be removed. ' +
        SAFE.map((g) => `${g.group}: ${g.rows.map(([name, , , label, sub]) => `${name} ${label} (${sub})`).join('; ')}.`).join(' '),
    },
    b,
    [
      'The rest of each bar is the unmount (kernel driver) or the eject (drive mode): the data is still going to the stick.',
      'Single runs. Other kernel runs on this stick: 259–371 s (128 KB clusters) and about 72 s (4 KB clusters).',
    ],
  );
}

// ----------------------------------------------------------------- 3. file size and the gain
// MSI DATAMAG SSD, medians of 3 [#6][#7]. Speed-up over the kernel driver per dataset.

const SIZE_SETS = ['mixed', 'small', 'many'];
const SIZE = [
  ['build', 'a blank drive', [2.2, 6.7, 10.3]],
  ['add', 'onto a drive holding 1.4 GB', [2.0, 3.6, 5.1]],
  ['pull', 'from a kernel-filled drive', [3.0, 9.1, 11.9]],
  ['pull', 'from a flashcp-built drive', [3.2, 14.6, 26.8]],
];

function size(c) {
  const left = 58;
  const gap = 18;
  const pw = (W - left - PAD - gap * 3) / 4;
  const top = 118;
  const base = 318;
  const max = 30;
  const Y = (v) => base - ((base - top) * v) / max;
  const height = base + 92;

  let b = '';
  for (const t of [0, 10, 20, 30]) {
    b += text(left - 10, Y(t) + 4, `${t}×`, {size: 12, fill: c.muted, anchor: 'end'});
  }
  SIZE.forEach(([name, sub, vals], i) => {
    const px = left + i * (pw + gap);
    b += text(px, 84, name, {size: 14, weight: 600, fill: c.text});
    b += text(px, 101, sub, {size: 12, fill: c.muted});
    for (const t of [10, 20, 30]) b += hline(px, px + pw, Y(t), c.grid);
    vals.forEach((v, j) => {
      const cx = px + (pw * (j + 0.5)) / 3;
      b += vbar(cx - 12, base, 24, base - Y(v), c.teal);
      b += text(cx, Y(v) - 7, `${v.toFixed(1)}×`, {size: 13, weight: 600, fill: c.text, anchor: 'middle'});
      b += text(cx, base + 18, SIZE_SETS[j], {size: 12, fill: c.muted, anchor: 'middle'});
    });
    b += hline(px, px + pw, base, c.muted);
  });
  return frame(
    c,
    height,
    {
      title: 'The smaller the files, the bigger the gain',
      subtitle: 'Times faster than the Linux kernel driver on the MSI USB SSD, by dataset (medians of 3)',
      desc: 'Four small bar charts sharing one scale from 0 to 30 times, one bar per dataset (mixed, small, many). ' +
        SIZE.map(([name, sub, v]) => `${name} ${sub}: mixed ${v[0]}×, small ${v[1]}×, many ${v[2]}×.`).join(' '),
    },
    b,
    [
      'mixed = 8,400 files, 1.4 GB (about 170 KB each) · small = 20,000 files, 169 MB (about 8 KB each)',
      'many = 100,000 files, 413 MB (about 4 KB each)',
    ],
  );
}

// --------------------------------------------------------------------- 4. the drive's own limit
// `small` written to a blank drive, against the time the drive needs for 169 MB at its measured
// sequential write rate (8 MB writes): MSI 999 MB/s (`flashcp probe`), Kingston 82.7 MB/s (the
// probe; it sustains less, see the README), SanDisk 6.5 MB/s. 169 / rate = 0.17, 2.0, 26 s.
// Kernel = mkfs.exfat + mount + cp + unmount: MSI 2.80 s on its default format [#6]; Kingston
// with 4 KB clusters 52.2–74.0 s [K][K2]; SanDisk on its default 32 KB format 86.3–179.6 s [#9][#6].
// flashcp build (4 KB clusters on all three): MSI 0.42 s [#6]; Kingston 2.72–9.02 s [K][#18];
// SanDisk 33.6–34.6 s [#9][#6].

const LIMIT = [
  ['USB SSD', 'MSI DATAMAG', {v: 0.17, label: '0.17 s'}, {lo: 0.42, hi: 0.42, label: '0.42 s'}, {lo: 2.8, hi: 2.8, label: '2.80 s'}],
  ['USB 3 stick', 'Kingston DataTraveler 3.0', {v: 2.0, label: '2.0 s'}, {lo: 2.72, hi: 9.02, label: '2.72–9.02 s'}, {lo: 52.2, hi: 74.0, label: '52–74 s'}],
  ['USB 2.0 stick', 'SanDisk Cruzer Blade', {v: 26, label: '26 s'}, {lo: 33.6, hi: 34.6, label: '33.6–34.6 s'}, {lo: 86.3, hi: 179.6, label: '86–180 s'}],
];

function limit(c) {
  const x0 = 214;
  const x1 = 744;
  const lo = 0.05;
  const hi = 300;
  const X = (v) => x0 + ((x1 - x0) * Math.log10(v / lo)) / Math.log10(hi / lo);
  const ROW = 62;
  const top = 108;
  const bottom = top + LIMIT.length * ROW;
  const height = bottom + 95;

  let b = legend(c, PAD, 82, [
    ['diamond', c.amber, 'the drive itself: 169 MB at its sequential write speed (computed)'],
    ['dot', c.teal, 'flashcp build'],
    ['dot', c.coral, 'Linux kernel driver'],
  ]);
  for (const t of [0.1, 1, 10, 100]) {
    b += vline(X(t), top, bottom, c.grid);
    b += text(X(t), bottom + 17, `${t} s`, {size: 12, fill: c.muted, anchor: 'middle'});
  }
  LIMIT.forEach(([kind, model, floor, flash, kern], i) => {
    const cy = top + i * ROW + ROW / 2 + 6;
    b += hline(x0, x1, cy, c.rowLine);
    b += text(PAD, cy - 3, kind, {size: 13, weight: 600, fill: c.text});
    b += text(PAD, cy + 14, model, {size: 12, fill: c.muted});
    b += mark(X(kern.lo), X(kern.hi), cy, c.coral, c.bg);
    b += mark(X(flash.lo), X(flash.hi), cy, c.teal, c.bg);
    b += diamond(X(floor.v), cy, c.amber, c.bg);
    // the limit's label sits to its left, flashcp's above its mark, the kernel's to its right
    b += text(X(floor.v) - 12, cy + 4.5, floor.label, {size: 13, fill: c.text, anchor: 'end'});
    b += text((X(flash.lo) + X(flash.hi)) / 2, cy - 13, flash.label, {size: 13, weight: 600, fill: c.text, anchor: 'middle'});
    b += text(X(kern.hi) + 12, cy + 4.5, kern.label, {size: 13, fill: c.text});
  });
  return frame(
    c,
    height,
    {
      title: 'How close each way gets to the drive’s own speed',
      subtitle: 'Writing 20,000 small files (169 MB) to a blank drive, formatting included',
      desc: 'Dot plot on a logarithmic seconds axis, three marks per drive. ' +
        LIMIT.map(([kind, model, f, fl, k]) => `${kind} (${model}): the drive itself ${f.label}, flashcp build ${fl.label}, kernel driver ${k.label}.`).join(' '),
    },
    b,
    [
      'Logarithmic axis. The drive itself = 169 MB ÷ its measured sequential write rate (999, 82.7 and 6.5 MB/s).',
      'flashcp build used 4 KB clusters. Kernel driver = mkfs.exfat + cp + unmount: its default format on the SSD (128 KB',
      'clusters) and the SanDisk (32 KB clusters); 4 KB clusters on the Kingston (259–371 s on its default 128 KB format).',
    ],
  );
}

// ------------------------------------------------------------------------ 5. release by release
// The same kind of copy across the releases: 20,000 small files onto the Kingston stick, until
// the data is on it. These are NOT one job run four times; each row says what it was. All timed
// on 2026-11-10 with the builds of that day; the release is the one the feature first shipped in.

const RELEASES = [
  // whole runs on a freshly formatted stick: 258.8 [K] and 288.7 [K2] ending with an unmount,
  // 315.5 (sync -f) and 370.7 (sync) [K]. PR 22's own kernel run, 315.1 s, is inside this range.
  ['Without flashcp', 'the Linux kernel driver', '128 KB clusters, freshly formatted stick', 'coral', 258.8, 370.7, '259–371 s'],
  // flashcp add onto a stick that already held files: 89.7 [K], 93.3 [#18], 98.0 [#26]
  ['0.1.0 · flashcp add', 'a command, on an unmounted stick', '128 KB clusters, stick already holding files', 'teal', 89.7, 98.0, '90–98 s'],
  // drive mode: 73.8 (median of 3) [#22], 93.8 and 94.7 [K]
  ['0.3.0 · drive mode', 'automatic, from any app', '128 KB clusters', 'teal', 73.8, 94.7, '74–95 s'],
  // [#26]: the same stick after `recluster` (6.35 s, not in the bar), 40,000 files on it
  ['0.4.0 · after “Make faster”', 'flashcp add onto the converted stick, 4 KB clusters', 'the one-time 6.35 s conversion is not in the bar', 'teal', 5.48, 5.48, '5.48 s'],
];

function releases(c) {
  const x0 = 356;
  const x1 = 712;
  const max = 400;
  const X = (v) => x0 + ((x1 - x0) * v) / max;
  const ROW = 62;
  const top = 78;
  const bottom = top + RELEASES.length * ROW + 4;
  const height = bottom + 78;

  let b = '';
  for (const t of [0, 100, 200, 300, 400]) {
    b += vline(X(t), top, bottom, t === 0 ? c.muted : c.grid);
    b += text(X(t), bottom + 17, `${t} s`, {size: 12, fill: c.muted, anchor: 'middle'});
  }
  RELEASES.forEach(([name, sub, cond, col, lo, hi, label], i) => {
    const cy = top + i * ROW + ROW / 2 + 2;
    b += text(x0 - 12, cy - 11, name, {size: 13, weight: 600, fill: c.text, anchor: 'end'});
    b += text(x0 - 12, cy + 5, sub, {size: 12, fill: c.muted, anchor: 'end'});
    b += text(x0 - 12, cy + 21, cond, {size: 12, fill: c.muted, anchor: 'end'});
    // the faster run solid, the spread up to the slower run fainter
    if (hi > lo) {
      b += hbar(X(0), cy - 9, X(hi) - X(0), 18, c[col], 0.4);
      b += hbar(X(0), cy - 9, X(lo) - X(0), 18, c[col], 1, false);
    } else {
      b += hbar(X(0), cy - 9, X(lo) - X(0), 18, c[col]);
    }
    b += text(X(hi) + 9, cy + 4.5, label, {size: 13, weight: 600, fill: c.text});
  });
  b += vline(X(0), top, bottom, c.muted);
  return frame(
    c,
    height,
    {
      title: '20,000 small files onto a Kingston stick, release by release',
      subtitle: 'Seconds until the data is on the stick · each figure under the release its feature first shipped in',
      desc: 'Bar chart in seconds; the solid part is the fastest run and the fainter part reaches the slowest. ' +
        RELEASES.map(([name, sub, cond, , , , label]) => `${name} (${sub}; ${cond}): ${label}.`).join(' '),
    },
    b,
    [
      'Solid = the fastest run, fainter = up to the slowest. Not one job run four times: the rows differ in what was',
      'on the stick, and all were timed on 2026-11-10 with that day’s builds, not with each release’s binary.',
    ],
  );
}

// ------------------------------------------------------------------------------------- write

const CHARTS = {speedup, 'safe-to-remove': safe, 'file-size': size, 'drive-limit': limit, releases};

mkdirSync(ASSETS, {recursive: true});
for (const [name, draw] of Object.entries(CHARTS)) {
  for (const [theme, c] of Object.entries(THEMES)) {
    const file = join(ASSETS, `chart-${name}-${theme}.svg`);
    writeFileSync(file, draw(c));
    console.log(file);
  }
}
