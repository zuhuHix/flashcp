/* flashcp site. Plain browser JS, no libraries.
   Benchmark numbers are the measured tables from README.md (merged work only), taken on
   2026-11-09, Linux 7.0, on an MSI DATAMAG 20Gbps USB SSD and a SanDisk Cruzer Blade 8 GB
   USB 2.0 stick, and 2026-11-10 on a Kingston DataTraveler 3.0 128 GB USB 3 stick (#18, #22 and
   the afternoon session; its speedups are the published figures, carried as xLabel). Times are
   seconds, medians of 3 unless a case says otherwise (single runs and ranges are labelled). A case
   whose two sides used different cluster sizes says so and carries no multiplier. Every speedup shown is computed below from these times, except where a case
   carries `x`: the published figure from the unrounded medians (5.58 / 0.38 s rounds to 14.7x; measured 14.6x). */
(function () {
  'use strict';

  var MB = 1024 * 1024;
  var KB = 1024;
  var STORAGE_KEY = 'flashcp-theme';
  var reduceQuery = window.matchMedia ? window.matchMedia('(prefers-reduced-motion: reduce)') : null;
  var darkQuery = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;

  function $(id) { return document.getElementById(id); }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  // ---------------------------------------------------------------
  // Theme toggle (manual override persists in localStorage; optional)
  // ---------------------------------------------------------------
  function currentIsDark() {
    var forced = document.documentElement.dataset.theme;
    if (forced === 'dark') return true;
    if (forced === 'light') return false;
    return !!(darkQuery && darkQuery.matches);
  }

  function setThemeToggleUI() {
    var btn = $('theme-toggle');
    if (!btn) return;
    var dark = currentIsDark();
    btn.setAttribute('aria-pressed', dark ? 'true' : 'false');
  }

  function initTheme() {
    var btn = $('theme-toggle');
    if (!btn) return;
    setThemeToggleUI();
    btn.addEventListener('click', function () {
      var next = currentIsDark() ? 'light' : 'dark';
      document.documentElement.dataset.theme = next;
      try { localStorage.setItem(STORAGE_KEY, next); } catch (e) { /* storage blocked: still switches for this visit */ }
      setThemeToggleUI();
      drawStrip(); // canvas colours come from CSS variables
      setThemedVideos();
    });
    if (darkQuery) {
      var onSystemChange = function () {
        var forced = document.documentElement.dataset.theme;
        if (forced !== 'light' && forced !== 'dark') {
          setThemeToggleUI();
          drawStrip();
          setThemedVideos();
        }
      };
      if (darkQuery.addEventListener) darkQuery.addEventListener('change', onSystemChange);
      else if (darkQuery.addListener) darkQuery.addListener(onSystemChange);
    }
  }

  // ---------------------------------------------------------------
  // Benchmark data (measured; seconds). Each case lists the normal-copy
  // baseline as `base`; speedup = base / method time.
  //   build: copy = mkfs + smart copy, full = flashcp build --fat full
  //   pull:  each drive layout has its own baseline (kernel-filled, flashcp-built)
  // ---------------------------------------------------------------
  var DATASET_LABEL = {
    small: '20k files / 169 MB',
    many: '100k files / 413 MB',
    mixed: '8.4k mixed / 1.4 GB'
  };
  var DRIVE_DATASETS = {
    ssd: ['small', 'many', 'mixed'],
    stick: ['small'], // the stick was measured on the 20k-file dataset only
    usb3: ['small', 'many']
  };
  var NO_X = 'formats differ'; // shown instead of a multiplier when the cluster sizes are not the same

  var BENCH = {
    ssd: {
      build: {
        small: { base: 2.80, copy: 2.85, flash: 0.42, full: 0.73 },
        many:  { base: 8.90, copy: 8.80, flash: 0.86, full: 1.83 },
        mixed: { base: 3.71, copy: 3.50, flash: 1.72, full: 2.21 }
      },
      add: {
        small: { base: 1.72, flash: 0.48 },
        many:  { base: 9.01, flash: 1.75 },
        mixed: { base: 3.16, flash: 1.57 }
      },
      pull: {
        small: { kernel: { base: 6.01, flash: 0.66 }, built: { base: 5.58, flash: 0.38, x: 14.6 } }, // x: published speedup, computed from unrounded medians
        many:  { kernel: { base: 27.02, flash: 2.27 }, built: { base: 31.39, flash: 1.17 } },
        mixed: { kernel: { base: 5.39, flash: 1.82 }, built: { base: 5.44, flash: 1.68 } }
      },
      mode: {
        small: { cases: [
          { sub: 'Copy until the drive is safe to remove · medians of 3', base: 1.80, flash: 1.26 }
        ] }
      }
    },
    stick: {
      build: {
        // mkfs + cp varied from 110.4 s to 179.6 s across runs on this stick; flashcp is a median of 3.
        small: { base: 110.4, baseLabel: '110.4–179.6 s', flash: 33.6, flashLabel: '33.6 s', xLabel: '3.3–5.3×' }
      },
      add: {
        small: { cases: [
          { sub: 'Onto a stick holding 20k files, 32 KB clusters on both sides · single runs; flashcp took 96–114 s in other runs', base: 209.8, baseLabel: '209.8 s', flash: 84.1, flashLabel: '84.1–114 s', xLabel: '1.8–2.5×' },
          { sub: 'The same add onto a stick after flashcp optimize (4 KB clusters) · single run; the kernel was only timed on the 32 KB stick', base: 209.8, baseLabel: '209.8 s (32 KB)', flash: 36.5, flashLabel: '36.5 s (4 KB)', xLabel: NO_X }
        ] }
      },
      pull: {
        small: { kernel: { base: 30.86, flash: 29.52 }, built: { base: 32.78, flash: 16.38 } }
      }
    },
    usb3: {
      build: {
        // Afternoon session, single runs. build of `many` has no baseline (not run, ≈25 min), so it is not shown.
        small: { cases: [
          { sub: 'Like for like: 4 KB clusters on both sides, formatting included (mkfs.exfat -c 4K + mount + cp + unmount took 52.2 s and 74.0 s) · single runs; the build took 2.72 s in the afternoon and 9.02 s in the morning', base: 52.2, baseLabel: '52.2–74.0 s', flash: 2.72, flashLabel: '2.72–9.02 s', xLabel: '6–27×' },
          { sub: 'If you just format and copy: the kernel on the default 128 KB clusters (259–289 s ending with unmount, 371 s with sync + unmount), flashcp build on 4 KB', base: 258.8, baseLabel: '259–371 s (128 KB)', flash: 2.72, flashLabel: '2.72–9.02 s (4 KB)', xLabel: NO_X }
        ] }
      },
      add: {
        small: { cases: [
          { sub: 'Onto a kernel-filled stick, 128 KB clusters on both sides · single run each; flashcp’s probe chose to zero-fill gaps (skipping took 144 s)', base: 527, baseLabel: '527 s', flash: 93.3, flashLabel: '93.3 s', xLabel: '5.7×' }
        ] }
      },
      mode: {
        small: { cases: [
          { sub: 'Copy until safe to remove, 4 KB clusters on both sides · single runs; the kernel’s copy + unmount took 50.3 s in one run and about 72 s in another (74.0 s with ≈2 s of formatting and mounting)', base: 50.3, baseLabel: '50.3–72 s', flash: 5.31, flashLabel: '5.31 s', xLabel: '9–14×' },
          { sub: 'Copy until safe to remove, default 128 KB clusters on both sides · single runs; the kernel figures are whole runs (formatting and mounting are 0.46 s of them): 259 s and 289 s ending with unmount (2.8–3.1×), 371 s with sync + unmount (4.0×)', base: 258.8, baseLabel: '259–371 s', flash: 93.8, flashLabel: '93.8 s', xLabel: '2.8–4.0×' }
        ] }
      },
      pull: {
        small: { kernel: { base: 11.3, flash: 7.96, xLabel: '1.4×' }, built: { base: 7.2, flash: 1.15, xLabel: '6.2×' } },
        many:  { built: { base: 38.6, flash: 3.60, xLabel: '10.7×' } } // kernel-filled layout not measured on `many`
      }
    }
  };

  var NORMAL_LABEL = {
    build: 'Normal copy: mkfs + cp',
    add: 'Normal copy: mount + cp',
    pull: 'Normal copy: mount + cp',
    mode: 'Linux kernel driver: cp + unmount'
  };
  var FLASH_LABEL = { build: 'flashcp build', add: 'flashcp add', pull: 'flashcp pull', mode: 'flashcp drive mode' };
  var KERNEL_STICK_NOTE = 'on par: the stick’s measured 13.8 MB/s sequential read is the limit';

  // Table B: copies onto an existing mounted volume. Speedup vs naive cp.
  var DATA_B = [
    {
      label: '20k files / 169 MB',
      naive: 1.66,
      methods: [
        { label: 'cp', x: 1.01, fill: 'fill-copy' },
        { label: 'smart', x: 1.08, fill: 'fill-copy' },
        { label: 'smart-w4', x: 1.17, fill: 'fill-copy' },
        { label: 'pack (tar, archive)', x: 4.31, fill: 'fill-flash' },
        { label: 'pack-auto (adaptive zstd, archive)', x: 4.93, fill: 'fill-flash' }
      ]
    },
    {
      label: '8.4k files / 1.4 GB',
      naive: 2.63,
      methods: [
        { label: 'cp', x: 0.97, fill: 'fill-copy' },
        { label: 'smart', x: 0.97, fill: 'fill-copy' },
        { label: 'smart-w4', x: 0.66, fill: 'fill-copy' },
        { label: 'pack (tar, archive)', x: 0.86, fill: 'fill-flash' },
        { label: 'pack-auto (adaptive zstd, archive)', x: 0.99, fill: 'fill-flash' }
      ]
    }
  ];
  var EARLY_SCALE_MAX = 5;

  // ---------------------------------------------------------------
  // Benchmark explorer
  // ---------------------------------------------------------------
  var explorer = {
    drive: 'ssd',
    op: 'build',
    dataset: 'all',
    extras: { copy: true, full: true }
  };

  function formatSeconds(s) { return s.toFixed(2) + ' s'; }
  // Two decimals below 1.5x so a 1.05x result reads as 1.05x; one decimal otherwise.
  function formatX(x) { return x < 1.5 ? x.toFixed(2) + '×' : x.toFixed(1) + '×'; }
  function formatXOne(x) { return x.toFixed(1) + '×'; }

  // Rows for one case: normal-copy baseline first, then the flashcp variants.
  function rowsFor(c, op) {
    var rows = [{ label: NORMAL_LABEL[op], t: c.base, tLabel: c.baseLabel, fill: 'fill-normal' }];
    if (op === 'build' && c.copy !== undefined && explorer.extras.copy) {
      rows.push({ label: 'mkfs + smart copy', t: c.copy, fill: 'fill-copy' });
    }
    rows.push({ label: FLASH_LABEL[op], t: c.flash, tLabel: c.flashLabel, x: c.x, xLabel: c.xLabel, fill: 'fill-flash' });
    if (op === 'build' && c.full !== undefined && explorer.extras.full) {
      rows.push({ label: 'flashcp build, full FAT', t: c.full, fill: 'fill-flash2' });
    }
    return rows;
  }

  // One block of bars. The first row is the baseline; every other row is scored against it.
  function renderBars(box, rows, sub, note) {
    if (sub) box.appendChild(el('h4', 'bar-sub-title', sub));

    var maxTime = 0;
    rows.forEach(function (r) { if (r.t > maxTime) maxTime = r.t; });
    var baseline = rows[0].t;

    rows.forEach(function (r, i) {
      var speedup = r.x !== undefined ? r.x : baseline / r.t;
      var isBase = i === 0;

      var row = el('div', 'bar-row');
      row.appendChild(el('div', 'bar-label', r.label));

      var main = el('div', 'bar-main');
      var track = el('div', 'bar-track');
      track.setAttribute('role', 'img');
      track.setAttribute('aria-label', r.label + ': ' + (r.tLabel || formatSeconds(r.t)) +
        (isBase ? ', baseline' : ', ' + (r.xLabel || formatX(speedup)) + ' vs baseline'));
      var fill = el('div', 'bar-fill ' + r.fill);
      track.appendChild(fill);
      main.appendChild(track);
      row.appendChild(main);

      var value = el('div', 'bar-value');
      value.appendChild(document.createTextNode((r.tLabel || formatSeconds(r.t)) + '  '));
      value.appendChild(el('span', 'sp', isBase ? 'baseline' : (r.xLabel || formatX(speedup))));
      row.appendChild(value);

      box.appendChild(row);

      // Set the width after insertion so the CSS transition can run.
      var pct = (r.t / maxTime) * 100;
      requestAnimationFrame(function () { fill.style.width = pct.toFixed(2) + '%'; });
    });

    if (note) box.appendChild(el('p', 'note', note));
  }

  function renderMain() {
    var box = $('bars-main');
    if (!box) return;
    box.innerHTML = '';

    var drive = explorer.drive;
    var op = explorer.op;


    var sets = DRIVE_DATASETS[drive].filter(function (ds) {
      return explorer.dataset === 'all' || ds === explorer.dataset;
    });

    var shown = 0;
    sets.forEach(function (ds) {
      var ops = BENCH[drive][op];
      var c = ops && ops[ds];
      if (!c) {
        if (explorer.dataset !== 'all') {
          box.appendChild(el('p', 'note', 'Not measured on this drive for this dataset.'));
          shown++;
        }
        return;
      }
      shown++;

      box.appendChild(el('h3', 'bar-group-title', DATASET_LABEL[ds]));

      if (c.cases) {
        c.cases.forEach(function (k) { renderBars(box, rowsFor(k, op), k.sub, ''); });
      } else if (op === 'pull') {
        if (c.kernel) renderBars(box, rowsFor(c.kernel, op), 'Kernel-filled drive',
          drive === 'stick' ? KERNEL_STICK_NOTE : '');
        if (c.built) renderBars(box, rowsFor(c.built, op), 'flashcp-built drive',
          drive === 'stick' ? 'After flashcp optimize, pulling a stick that was kernel-filled went from 28.9 s to 16.0 s (1.8×), and optimize itself took 81–103 s once.' : '');
      } else {
        renderBars(box, rowsFor(c, op), '', '');
      }
    });

    if (!shown) box.appendChild(el('p', 'note', 'Not measured on this drive.'));
    box.appendChild(el('p', 'note', 'Shorter bar = faster. Speedup is baseline time ÷ method time, for the same dataset and drive layout.'));
  }

  // Keep the controls consistent with the explorer state (drive limits dataset choice).
  function syncControls() {
    var drive = explorer.drive;
    var op = explorer.op;
    var allowed = DRIVE_DATASETS[drive];

    var datasetOk = explorer.dataset === 'all' ? allowed.length > 1 : allowed.indexOf(explorer.dataset) !== -1;
    if (!datasetOk) {
      explorer.dataset = allowed[0];
    }

    markPressed($('drive-buttons'), 'data-drive', drive);
    markPressed($('op-buttons'), 'data-op', op);

    var datasetBox = $('dataset-buttons');
    if (datasetBox) {
      Array.prototype.forEach.call(datasetBox.querySelectorAll('button[data-dataset]'), function (b) {
        var id = b.getAttribute('data-dataset');
        var available = id === 'all' ? allowed.length > 1 : allowed.indexOf(id) !== -1;
        b.disabled = !available;
        b.setAttribute('aria-pressed', id === explorer.dataset ? 'true' : 'false');
      });
    }

    var note = $('explorer-note');
    if (note) {
      if (drive === 'stick') {
        note.textContent = 'The stick was measured only on the 20k-file dataset, so the other sizes are not offered.';
        note.hidden = false;
      } else if (drive === 'usb3') {
        note.textContent = 'The Kingston stick was measured on the 20k and 100k-file datasets, mostly as single runs. On 100k files only a pull from a flashcp-built stick has a baseline; build, add and drive mode have baselines on 20k files only.';
        note.hidden = false;
      } else {
        note.textContent = '';
        note.hidden = true;
      }
    }

    // Extra build variants exist for the SSD only.
    var variants = $('variant-group');
    if (variants) variants.hidden = !(drive === 'ssd' && op === 'build');
    var variantBox = $('variant-checks');
    if (variantBox) {
      Array.prototype.forEach.call(variantBox.querySelectorAll('input[type="checkbox"]'), function (input) {
        input.checked = !!explorer.extras[input.value];
      });
    }
  }

  function markPressed(container, attr, value) {
    if (!container) return;
    Array.prototype.forEach.call(container.querySelectorAll('button[' + attr + ']'), function (b) {
      b.setAttribute('aria-pressed', b.getAttribute(attr) === value ? 'true' : 'false');
    });
  }

  function refreshExplorer() {
    syncControls();
    renderMain();
  }

  function initExplorer() {
    var driveBox = $('drive-buttons');
    if (driveBox) {
      driveBox.addEventListener('click', function (ev) {
        var btn = ev.target.closest('button[data-drive]');
        if (!btn) return;
        explorer.drive = btn.getAttribute('data-drive');
        refreshExplorer();
      });
    }

    var opBox = $('op-buttons');
    if (opBox) {
      opBox.addEventListener('click', function (ev) {
        var btn = ev.target.closest('button[data-op]');
        if (!btn) return;
        explorer.op = btn.getAttribute('data-op');
        refreshExplorer();
      });
    }

    var datasetBox = $('dataset-buttons');
    if (datasetBox) {
      datasetBox.addEventListener('click', function (ev) {
        var btn = ev.target.closest('button[data-dataset]');
        if (!btn || btn.disabled) return;
        explorer.dataset = btn.getAttribute('data-dataset');
        refreshExplorer();
      });
    }

    var checks = $('variant-checks');
    if (checks) {
      checks.addEventListener('change', function (ev) {
        var input = ev.target;
        if (!input || input.type !== 'checkbox') return;
        explorer.extras[input.value] = input.checked;
        renderMain();
      });
    }

    refreshExplorer();
  }

  // ---------------------------------------------------------------
  // Earlier experiments (Table B)
  // ---------------------------------------------------------------
  function renderEarly() {
    var box = $('bars-early');
    if (!box) return;
    box.innerHTML = '';

    DATA_B.forEach(function (ds) {
      box.appendChild(el('h3', 'bar-group-title', ds.label + '  (naive copy ' + ds.naive.toFixed(2) + ' s = 1×)'));

      ds.methods.forEach(function (m) {
        var row = el('div', 'bar-row');
        row.appendChild(el('div', 'bar-label', m.label));

        var main = el('div', 'bar-main');
        var track = el('div', 'bar-track');
        track.setAttribute('role', 'img');
        track.setAttribute('aria-label', m.label + ': ' + formatXOne(m.x) + ' vs naive copy');
        var fill = el('div', 'bar-fill ' + m.fill);
        track.appendChild(fill);
        var ref = el('span', 'ref-line');
        ref.style.left = ((1 / EARLY_SCALE_MAX) * 100).toFixed(2) + '%';
        track.appendChild(ref);
        main.appendChild(track);
        row.appendChild(main);

        row.appendChild(el('div', 'bar-value', formatXOne(m.x)));
        box.appendChild(row);

        var pct = Math.min(m.x / EARLY_SCALE_MAX, 1) * 100;
        requestAnimationFrame(function () { fill.style.width = pct.toFixed(2) + '%'; });
      });
    });

    box.appendChild(el('p', 'note', 'Bars share a 0–5× scale. The vertical line marks 1× (naive copy). Pack is an archive, not loose files.'));
  }

  function initTabs() {
    var tabs = Array.prototype.slice.call(document.querySelectorAll('button[data-panel]'));
    tabs.forEach(function (tab) {
      tab.addEventListener('click', function () {
        tabs.forEach(function (t) {
          var panel = $(t.getAttribute('data-panel'));
          var on = t === tab;
          t.setAttribute('aria-pressed', on ? 'true' : 'false');
          if (panel) panel.hidden = !on;
        });
      });
    });
  }

  // ---------------------------------------------------------------
  // "How many writes?" model
  // ---------------------------------------------------------------
  var FILE_MIN = 100;          // files
  var FILE_MAX = 200000;
  var SIZE_MIN = 1 * KB;       // bytes
  var SIZE_MAX = 10 * MB;
  var FLASH_CHUNK = 8 * MB;
  var FLASH_FIXED = 4;         // boot region and tables

  function sliderFraction(id) { return Number($(id).value) / 1000; }

  function fileCount() {
    // log scale from FILE_MIN to FILE_MAX
    var n = FILE_MIN * Math.pow(FILE_MAX / FILE_MIN, sliderFraction('files-range'));
    var step = Math.pow(10, Math.max(0, Math.floor(Math.log10(n)) - 1)); // 2 significant digits
    return Math.round(n / step) * step;
  }

  function avgSize() {
    return SIZE_MIN * Math.pow(SIZE_MAX / SIZE_MIN, sliderFraction('size-range'));
  }

  function formatBytes(b) {
    if (b >= 1024 * MB) return (b / (1024 * MB)).toFixed(1) + ' GB';
    if (b >= MB) return (b / MB).toFixed(b >= 10 * MB ? 0 : 1).replace(/\.0$/, '') + ' MB';
    return Math.round(b / KB) + ' KB';
  }

  function computeWrites(n, s) {
    var perFile = 2 + Math.max(1, Math.ceil(s / MB)); // 2 metadata + data writes
    var normal = n * perFile;
    var flash = Math.ceil((n * s) / FLASH_CHUNK) + FLASH_FIXED;
    return { normal: normal, flash: flash, perFile: perFile };
  }

  function updateWrites() {
    var n = fileCount();
    var s = avgSize();
    var w = computeWrites(n, s);

    $('files-out').textContent = n.toLocaleString('en-US') + ' files';
    $('size-out').textContent = formatBytes(s) + ' average';
    $('normal-count').textContent = w.normal.toLocaleString('en-US');
    $('flash-count').textContent = w.flash.toLocaleString('en-US');

    var ratio = w.normal / w.flash;
    var ratioText = ratio >= 10 ? Math.round(ratio).toLocaleString('en-US') : ratio.toFixed(1);
    $('ratio').textContent = ratioText + '× fewer writes';
  }

  // ---------------------------------------------------------------
  // Write-head strip (canvas, schematic)
  // ---------------------------------------------------------------
  var strip = { canvas: null, ctx: null, width: 0, height: 0, running: false, raf: 0, visible: false };
  var HOP_MS = 260;
  var STEPS = 24;
  var FLASH_PAUSE_MS = 900;
  var TRAIL = 8;

  // Normal copy: alternate between metadata area (left) and a data area that scatters across the drive.
  function normalPos(k) {
    if (k % 2 === 0) return 0.07;                        // metadata writes, left edge
    var frac = ((Math.floor(k / 2) * 0.618034) % 1);    // data writes, spread out
    return 0.3 + 0.66 * frac;
  }

  // flashcp: the first few writes are boot/tables at the left, then one ascending sweep.
  function flashPos(k) {
    if (k < 4) return 0.07;
    return 0.07 + 0.86 * ((k - 4) / (STEPS - 5));
  }

  function isMetaNormal(k) { return k % 2 === 0; }

  function cssVar(name, fallback) {
    var v = getComputedStyle(document.documentElement).getPropertyValue(name);
    return (v && v.trim()) || fallback;
  }

  function resizeStrip() {
    var c = strip.canvas;
    if (!c) return;
    var dpr = window.devicePixelRatio || 1;
    var rect = c.getBoundingClientRect();
    strip.width = rect.width;
    strip.height = rect.height;
    c.width = Math.round(rect.width * dpr);
    c.height = Math.round(rect.height * dpr);
    strip.ctx = c.getContext('2d');
    strip.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  // Draw one frame. t = milliseconds (animated), or null for a static full picture.
  function drawStrip(t) {
    if (!strip.canvas || !strip.ctx) return;
    var ctx = strip.ctx;
    var W = strip.width;
    var H = strip.height;
    if (!ctx || !W) return;

    var cNormal = cssVar('--c-normal', '#e5534b');
    var cFlash = cssVar('--c-flash', '#0f9d8a');
    var cMeta = cssVar('--c-meta', '#d4a72c');
    var cTrack = cssVar('--track', '#eaeef2');
    var cText = cssVar('--muted', '#59636e');

    ctx.clearRect(0, 0, W, H);

    var pad = 16;
    var span = W - pad * 2;
    var rows = [
      { y: H * 0.32, name: 'normal copy', color: cNormal, kind: 'normal' },
      { y: H * 0.74, name: 'flashcp', color: cFlash, kind: 'flash' }
    ];

    // Labels and tracks
    ctx.font = '12px system-ui, sans-serif';
    ctx.textBaseline = 'middle';
    rows.forEach(function (r) {
      ctx.fillStyle = cText;
      ctx.fillText(r.name, pad, r.y - 16);
      ctx.strokeStyle = cTrack;
      ctx.lineWidth = 4;
      ctx.lineCap = 'round';
      ctx.beginPath();
      ctx.moveTo(pad, r.y);
      ctx.lineTo(W - pad, r.y);
      ctx.stroke();
    });

    // Metadata zone marker
    ctx.fillStyle = cMeta;
    ctx.globalAlpha = 0.5;
    ctx.fillRect(pad + 0.07 * span - 6, H * 0.12, 12, 3);
    ctx.globalAlpha = 1;

    function posOf(kind, k) {
      return kind === 'normal' ? normalPos(k) : flashPos(k);
    }

    function colourOf(kind, k) {
      if (kind === 'normal') return isMetaNormal(k) ? cMeta : cNormal;
      return k < 4 ? cMeta : cFlash;
    }

    function drawDot(x, y, color, alpha, radius) {
      ctx.globalAlpha = alpha;
      ctx.fillStyle = color;
      ctx.beginPath();
      ctx.arc(x, y, radius, 0, Math.PI * 2);
      ctx.fill();
      ctx.globalAlpha = 1;
    }

    rows.forEach(function (r) {
      var cycle = r.kind === 'flash' ? STEPS * HOP_MS + FLASH_PAUSE_MS : STEPS * HOP_MS;
      var phaseMs = t === null ? null : (t % cycle);

      if (phaseMs === null) {
        // Static: draw the full pattern once.
        for (var k = 0; k < STEPS; k++) {
          drawDot(pad + posOf(r.kind, k) * span, r.y, colourOf(r.kind, k), 0.85, 4.5);
        }
        return;
      }

      var k0 = Math.floor(phaseMs / HOP_MS);
      var p = (phaseMs % HOP_MS) / HOP_MS;

      if (r.kind === 'flash' && k0 >= STEPS) {
        // Pause before the next sweep; leave the last positions visible but faded.
        for (var j = STEPS - TRAIL; j < STEPS; j++) {
          drawDot(pad + posOf('flash', j) * span, r.y, colourOf('flash', j), 0.2, 3.5);
        }
        return;
      }

      // Trail: recent writes, fading.
      for (var i = TRAIL; i >= 1; i--) {
        var kk = k0 - i;
        if (kk < 0 || kk >= STEPS) continue;
        drawDot(pad + posOf(r.kind, kk) * span, r.y, colourOf(r.kind, kk), 0.15 + 0.5 * (1 - i / TRAIL), 3.5);
      }

      // Head: jump quickly (ease-out) from the previous position to the current one.
      var from = posOf(r.kind, Math.max(k0 - 1, 0));
      var to = posOf(r.kind, k0);
      var e = 1 - Math.pow(1 - p, 3);
      var x = pad + (from + (to - from) * e) * span;
      drawDot(x, r.y, colourOf(r.kind, k0), 1, 6);
    });
  }

  function animationFrame(now) {
    if (!strip.running) return;
    drawStrip(now);
    strip.raf = requestAnimationFrame(animationFrame);
  }

  function startStrip() {
    if (strip.running) return;
    if (reduceQuery && reduceQuery.matches) return;
    strip.running = true;
    strip.raf = requestAnimationFrame(animationFrame);
  }

  function stopStrip() {
    strip.running = false;
    if (strip.raf) cancelAnimationFrame(strip.raf);
    strip.raf = 0;
  }

  function initStrip() {
    strip.canvas = $('head-strip');
    if (!strip.canvas) return;
    resizeStrip();

    // Static frame first, so reduced-motion users see the pattern.
    drawStrip(null);

    if (reduceQuery) {
      var onMotionChange = function () {
        if (reduceQuery.matches) {
          stopStrip();
          drawStrip(null);
        } else if (strip.visible) {
          startStrip();
        }
      };
      if (reduceQuery.addEventListener) reduceQuery.addEventListener('change', onMotionChange);
      else if (reduceQuery.addListener) reduceQuery.addListener(onMotionChange);
    }

    if ('IntersectionObserver' in window) {
      var io = new IntersectionObserver(function (entries) {
        entries.forEach(function (entry) {
          strip.visible = entry.isIntersecting;
          if (strip.visible) startStrip();
          else stopStrip();
        });
      });
      io.observe(strip.canvas);
    } else {
      strip.visible = true;
      startStrip();
    }

    window.addEventListener('resize', function () {
      resizeStrip();
      if (strip.running) return;
      drawStrip(null);
    });
  }

  function initWrites() {
    ['files-range', 'size-range'].forEach(function (id) {
      var input = $(id);
      if (input) input.addEventListener('input', updateWrites);
    });
    updateWrites();
    initStrip();
  }

  // ---------------------------------------------------------------
  // Hero video fallback
  // ---------------------------------------------------------------
  // Section videos come in a light and a dark version: <video data-light="…" data-dark="…">.
  // The one matching the current theme is loaded; with reduced motion it does not autoplay.
  function setThemedVideos() {
    var dark = currentIsDark();
    var still = !!(reduceQuery && reduceQuery.matches);
    Array.prototype.forEach.call(document.querySelectorAll('video[data-light][data-dark]'), function (v) {
      var want = v.getAttribute(dark ? 'data-dark' : 'data-light');
      if (v.getAttribute('src') !== want) v.setAttribute('src', want);
      v.controls = still;
      if (still) v.pause();
      else { var p = v.play(); if (p && p.catch) p.catch(function (e) { if (e && e.name === 'NotAllowedError') v.controls = true; }); }
    });
  }

  function initVideo() {
    setThemedVideos();
    if (reduceQuery) {
      if (reduceQuery.addEventListener) reduceQuery.addEventListener('change', setThemedVideos);
      else if (reduceQuery.addListener) reduceQuery.addListener(setThemedVideos);
    }
    var video = document.querySelector('.hero-media video');
    var fallback = $('video-fallback');
    if (!video || !fallback) return;
    var showFallback = function () {
      video.hidden = true;
      fallback.hidden = false;
    };
    video.addEventListener('error', showFallback);
    // A source that never loads leaves networkState at NO_SOURCE once loading gives up.
    video.addEventListener('stalled', function () {
      if (video.networkState === 3 && !video.readyState) showFallback();
    });
  }

  // ---------------------------------------------------------------
  // Boot
  // ---------------------------------------------------------------
  function init() {
    initTheme();
    initExplorer();
    initTabs();
    renderEarly();
    initWrites();
    initVideo();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
