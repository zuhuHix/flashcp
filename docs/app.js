/* flashcp site. Plain browser JS, no libraries.
   Every benchmark number below is copied from the measured tables in README.md
   (2026-11-09, MSI DATAMAG 20Gbps, exFAT, Linux 7.0). Speedups are computed here. */
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
    });
    if (darkQuery) {
      var onSystemChange = function () {
        var forced = document.documentElement.dataset.theme;
        if (forced !== 'light' && forced !== 'dark') {
          setThemeToggleUI();
          drawStrip();
        }
      };
      if (darkQuery.addEventListener) darkQuery.addEventListener('change', onSystemChange);
      else if (darkQuery.addListener) darkQuery.addListener(onSystemChange);
    }
  }

  // ---------------------------------------------------------------
  // Benchmark data (from README.md tables)
  // ---------------------------------------------------------------

  // Table A: fresh volume. Times in seconds.
  // Method order matches METHODS_A; index 0 is the baseline (mkfs + cp).
  var METHODS_A = [
    { label: 'mkfs + cp (baseline)', fill: 'fill-normal' },
    { label: 'mkfs + smart copy', fill: 'fill-copy' },
    { label: 'build, prefix FAT (default)', fill: 'fill-flash' },
    { label: 'build, full FAT', fill: 'fill-flash2' }
  ];
  var DATA_A = [
    { id: '20k', label: '20k files / 169 MB', times: [2.80, 2.85, 0.42, 0.73] },
    { id: '100k', label: '100k files / 413 MB', times: [8.90, 8.80, 0.86, 1.83] },
    { id: '8k', label: '8.4k mixed / 1.4 GB', times: [3.71, 3.50, 1.72, 2.21] }
  ];

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
  // Benchmark explorer (Table A)
  // ---------------------------------------------------------------
  var explorer = { dataset: 'all', methods: [true, true, true, true] };

  function formatSeconds(s) { return s.toFixed(2) + ' s'; }
  function formatX(x) { return x.toFixed(1) + '×'; }

  function renderMain() {
    var box = $('bars-main');
    if (!box) return;
    box.innerHTML = '';

    var sets = DATA_A.filter(function (ds) {
      return explorer.dataset === 'all' || ds.id === explorer.dataset;
    });
    var active = [];
    METHODS_A.forEach(function (m, i) { if (explorer.methods[i]) active.push(i); });

    if (!active.length) {
      box.appendChild(el('p', 'empty-msg', 'Select at least one method to show bars.'));
      return;
    }

    sets.forEach(function (ds) {
      if (explorer.dataset === 'all') box.appendChild(el('h3', 'bar-group-title', ds.label));

      // Scale bars within this dataset so the slowest visible method fills the track.
      var maxTime = 0;
      active.forEach(function (i) { if (ds.times[i] > maxTime) maxTime = ds.times[i]; });
      var baseline = ds.times[0];

      active.forEach(function (i) {
        var t = ds.times[i];
        var speedup = baseline / t;

        var row = el('div', 'bar-row');
        row.appendChild(el('div', 'bar-label', METHODS_A[i].label));

        var main = el('div', 'bar-main');
        var track = el('div', 'bar-track');
        track.setAttribute('role', 'img');
        track.setAttribute('aria-label', METHODS_A[i].label + ': ' + formatSeconds(t) + ', ' + formatX(speedup) + ' vs baseline');
        var fill = el('div', 'bar-fill ' + METHODS_A[i].fill);
        track.appendChild(fill);
        main.appendChild(track);
        row.appendChild(main);

        var value = el('div', 'bar-value');
        value.appendChild(document.createTextNode(formatSeconds(t) + '  '));
        value.appendChild(el('span', 'sp', formatX(speedup)));
        row.appendChild(value);

        box.appendChild(row);

        // Set the width after insertion so the CSS transition can run.
        var pct = (t / maxTime) * 100;
        requestAnimationFrame(function () { fill.style.width = pct.toFixed(2) + '%'; });
      });
    });

    box.appendChild(el('p', 'note', 'Shorter bar = faster. Speedup is relative to mkfs + cp for the same dataset.'));
  }

  function initExplorer() {
    var datasetBox = $('dataset-buttons');
    if (datasetBox) {
      datasetBox.addEventListener('click', function (ev) {
        var btn = ev.target.closest('button[data-dataset]');
        if (!btn) return;
        explorer.dataset = btn.getAttribute('data-dataset');
        Array.prototype.forEach.call(datasetBox.querySelectorAll('button[data-dataset]'), function (b) {
          b.setAttribute('aria-pressed', b === btn ? 'true' : 'false');
        });
        renderMain();
      });
    }

    var checks = $('method-checks');
    if (checks) {
      checks.addEventListener('change', function (ev) {
        var input = ev.target;
        if (!input || input.type !== 'checkbox') return;
        explorer.methods[Number(input.value)] = input.checked;
        renderMain();
      });
    }

    renderMain();
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
        track.setAttribute('aria-label', m.label + ': ' + formatX(m.x) + ' vs naive copy');
        var fill = el('div', 'bar-fill ' + m.fill);
        track.appendChild(fill);
        var ref = el('span', 'ref-line');
        ref.style.left = ((1 / EARLY_SCALE_MAX) * 100).toFixed(2) + '%';
        track.appendChild(ref);
        main.appendChild(track);
        row.appendChild(main);

        row.appendChild(el('div', 'bar-value', formatX(m.x)));
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
  function initVideo() {
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
