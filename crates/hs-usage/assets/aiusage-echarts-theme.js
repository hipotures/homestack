/* AI Usage — ECharts 6 theme and helpers on The Mechanics tokens.
 *
 *   const chart = AIUsage.init(el);   // registers the theme of the active data-tm-theme and inits (SVG renderer)
 *   chart.setOption({ series: [AIUsage.bar('Codex · gpt-6.1-sol', data, 'Codex · gpt-6.1-sol')] });
 *
 * Every color comes from CSS: --tm-* tokens (The Mechanics, vendor/the-mechanics) and the AI Usage identity
 * colors in aiusage.css. After a theme switch, AIUsage.refresh() rebuilds the theme and repaints every chart.
 * The theme object follows The Mechanics ECharts adapter (src/the-mechanics/adapters/echarts.js, v0.1.2).
 */
(function (global) {
  'use strict';

  const THEME = 'aiusage';
  const charts = new Set();

  const v = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);

  /* ---------- series colors: agent → identity hue, model → a tone of that hue ---------- */
  const AGENT_VAR = { antigravity: '--aiu-antigravity', codex: '--aiu-codex', 'claude code': '--aiu-claude', claude: '--aiu-claude' };
  const MODEL_VAR = {
    'gpt-6.1-sol': '--aiu-m-gpt-6-1-sol',
    'gpt-5.6-luna': '--aiu-m-gpt-5-6-luna',
    'claude-opus-5-5': '--aiu-m-claude-opus-5-5',
    'claude-haiku-5-5': '--aiu-m-claude-haiku-5-5',
    'claude-sonnet-5-5': '--aiu-m-claude-sonnet-5-5',
  };
  /** Mixes a #rrggbb color toward white (f > 0) or black (f < 0). */
  function shade(hex, f) {
    const n = parseInt(String(hex).replace('#', ''), 16);
    if (Number.isNaN(n)) return hex;
    const mix = (c) => Math.round(f > 0 ? c + (255 - c) * f : c * (1 + f));
    return '#' + [(n >> 16) & 255, (n >> 8) & 255, n & 255].map((c) => mix(c).toString(16).padStart(2, '0')).join('');
  }
  // Models without a dedicated token: well-separated light, mid and dark tones of the agent hue.
  const EXTRA_TONES = [0.45, -0.2, -0.5];
  function colorFor(key) {
    const k = String(key || '').toLowerCase();
    if (MODEL_VAR[k]) return v(MODEL_VAR[k]);
    for (const m in MODEL_VAR) if (k.includes(m)) return v(MODEL_VAR[m]);
    for (const a in AGENT_VAR) {
      if (!k.startsWith(a)) continue;
      // "Agent · model" keys without a dedicated token get a stable tone of the agent hue.
      const rest = k.slice(a.length).replace(/^\s*·\s*/, '');
      if (!rest) return v(AGENT_VAR[a]);
      let h = 0;
      for (const ch of rest) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
      return shade(v(AGENT_VAR[a]), EXTRA_TONES[h % EXTRA_TONES.length]);
    }
    return v('--tm-text-3');
  }

  /* ---------- formatters ---------- */
  const fmt = {
    tokens(n) {
      if (n == null) return '—';
      const a = Math.abs(n);
      if (a >= 1e9) return (n / 1e9).toFixed(a >= 1e10 ? 0 : 1) + 'B';
      if (a >= 1e6) return (n / 1e6).toFixed(a >= 1e7 ? 0 : 1) + 'M';
      if (a >= 1e3) return (n / 1e3).toFixed(a >= 1e4 ? 0 : 1) + 'k';
      return String(Math.round(n));
    },
    cost(n, precise) {
      if (n == null) return '—';
      if (precise) return '$' + n.toFixed(4);
      return '$' + (Math.abs(n) >= 100 ? n.toFixed(0) : n.toFixed(2));
    },
    pct(n, d = 1) { return n == null ? '—' : n.toFixed(d) + '%'; },
    full(n) { return n == null ? '—' : n.toLocaleString('pl-PL'); },
    /** seconds → "850 ms", "5.3 s", "14 min", "6 h 45 min" */
    duration(s) {
      if (s == null) return '—';
      if (s < 1) return Math.round(s * 1000) + ' ms';
      if (s < 60) return s.toFixed(s < 10 ? 1 : 0) + ' s';
      const m = Math.round(s / 60);
      if (m < 60) return m + ' min';
      return Math.floor(m / 60) + ' h' + (m % 60 ? ' ' + (m % 60) + ' min' : '');
    },
  };

  /* ---------- range → bucket size (≈ 30–90 points per chart) ---------- */
  const RANGES = {
    '1h':   { label: '1h',       bucket: '1m',  ms: 60e3,            axis: 'HH:mm' },
    '24h':  { label: '24h',      bucket: '30m', ms: 30 * 60e3,       axis: 'HH:mm' },
    '7d':   { label: '7d',       bucket: '3h',  ms: 3 * 3600e3,      axis: 'ddd HH:mm' },
    '30d':  { label: '30d',      bucket: '12h', ms: 12 * 3600e3,     axis: 'D MMM' },
    '90d':  { label: '90d',      bucket: '1d',  ms: 86400e3,         axis: 'D MMM' },
    '180d': { label: '180d',     bucket: '3d',  ms: 3 * 86400e3,     axis: 'D MMM' },
    '1y':   { label: '1 year',   bucket: '1w',  ms: 7 * 86400e3,     axis: 'MMM' },
    'all':  { label: 'All',      bucket: 'auto', ms: null,           axis: 'MMM yyyy' },
  };
  /** for 'all' pass the span in ms; returns a bucket ms that gives ≤ ~80 points */
  function bucketFor(range, spanMs) {
    const r = RANGES[range];
    if (r && r.ms) return r.ms;
    const steps = [86400e3, 7 * 86400e3, 14 * 86400e3, 30 * 86400e3, 91 * 86400e3];
    return steps.find((s) => spanMs / s <= 80) || steps[steps.length - 1];
  }

  /* ---------- theme (The Mechanics chart grammar) ---------- */
  function build() {
    const text = v('--tm-text'), text2 = v('--tm-text-2'), text3 = v('--tm-text-3');
    const grid = v('--tm-chart-grid'), axis = v('--tm-chart-axis'), mono = v('--tm-font-mono'), ui = v('--tm-font-ui');
    // Chart text: Plex Mono medium 11px; axis numbers in --tm-text-2; bold only in tooltips.
    const axisLabel = { color: text2, fontFamily: mono, fontSize: 11, fontWeight: 500, hideOverlap: true, margin: 10 };
    const axisName = { color: text2, fontFamily: mono, fontSize: 11, fontWeight: 500 };
    const baseAxis = { axisLine: { show: true, lineStyle: { color: axis, width: 1 } }, axisTick: { show: false }, axisLabel, nameTextStyle: axisName, splitLine: { show: false } };
    return {
      color: [1, 2, 3, 4, 5, 6, 7, 8].map((i) => v(`--tm-chart-${i}`)),
      backgroundColor: 'transparent',
      textStyle: { fontFamily: mono, fontSize: 11, color: text2 },
      grid: { left: 4, right: 14, top: 28, bottom: 2, containLabel: true },
      categoryAxis: baseAxis,
      timeAxis: baseAxis,
      valueAxis: { ...baseAxis, axisLine: { show: false }, splitNumber: 3, splitLine: { show: true, lineStyle: { color: grid, width: 1 } } },
      logAxis: { ...baseAxis, axisLine: { show: false }, splitLine: { show: true, lineStyle: { color: grid, width: 1 } } },
      legend: {
        top: 0, right: 0, icon: 'rect', itemWidth: 10, itemHeight: 10, itemGap: 14,
        textStyle: { color: text, fontFamily: mono, fontSize: 11, fontWeight: 500 },
        inactiveColor: text3,
        pageTextStyle: { color: text2, fontFamily: mono },
        pageIconColor: text, pageIconInactiveColor: text3,
      },
      tooltip: {
        trigger: 'axis',
        backgroundColor: v('--tm-surface'),
        borderColor: v('--tm-line-1'),
        borderWidth: 1,
        padding: [6, 8],
        textStyle: { color: text, fontFamily: ui, fontSize: 12 },
        // Hard offset shadow, no radius; below sticky chrome.
        extraCssText: 'border-radius:0;box-shadow:var(--tm-shadow-raised);z-index:var(--tm-z-sticky);',
        axisPointer: {
          type: 'shadow',
          shadowStyle: { color: v('--tm-hover') },
          lineStyle: { color: text2, type: [2, 2], width: 1 },
          crossStyle: { color: text2 },
          label: { backgroundColor: v('--tm-surface'), color: text, fontFamily: mono, borderColor: v('--tm-line-1'), borderWidth: 1 },
        },
      },
      bar: { barMaxWidth: 18, barCategoryGap: '30%', itemStyle: { borderRadius: 0 }, emphasis: { disabled: true } },
      line: { symbol: 'none', smooth: false, symbolSize: 6, lineStyle: { width: 2 }, emphasis: { disabled: true } },
      scatter: { symbolSize: 9, emphasis: { disabled: true } },
      heatmap: { itemStyle: { borderColor: v('--tm-surface'), borderWidth: 2 } },
      visualMap: {
        inRange: { color: [v('--tm-heat-lo'), v('--tm-heat-hi')] },
        textStyle: { color: text2, fontFamily: mono, fontSize: 11 },
        itemWidth: 10, itemHeight: 120,
      },
      dataZoom: {
        backgroundColor: 'transparent',
        borderColor: v('--tm-line-3'),
        fillerColor: v('--tm-hover'),
        handleStyle: { color: v('--tm-surface'), borderColor: v('--tm-line-1') },
        moveHandleStyle: { color: v('--tm-surface') },
        dataBackground: { lineStyle: { color: text3, width: 1 }, areaStyle: { color: v('--tm-inset') } },
        selectedDataBackground: { lineStyle: { color: text2 }, areaStyle: { color: v('--tm-hover') } },
        textStyle: { color: text2, fontFamily: mono, fontSize: 11 },
      },
      markLine: { symbol: 'none', lineStyle: { color: v('--tm-chart-threshold'), type: [5, 3], width: 1.5 }, label: { color: text2, fontFamily: mono, fontSize: 11 } },
      markArea: { itemStyle: { color: v('--tm-hover') }, label: { color: text2, fontFamily: mono, fontSize: 11 } },
    };
  }

  function register() { echarts.registerTheme(THEME, build()); }

  function init(el, opts) {
    register();
    const c = echarts.init(el, THEME, { renderer: 'svg', ...opts });
    charts.add(c);
    new ResizeObserver(() => c.resize()).observe(el);
    return c;
  }

  /** after changing data-tm-theme: rebuild the theme from CSS tokens and repaint */
  function refresh() {
    register();
    for (const c of charts) {
      if (c.isDisposed()) { charts.delete(c); continue; }
      c.setTheme(THEME);
    }
  }

  /* ---------- series helpers ---------- */
  function bar(name, data, colorKey, extra) {
    return { type: 'bar', name, data, stack: 'total', barMinHeight: 0, itemStyle: { color: colorFor(colorKey || name) }, ...extra };
  }
  /** Boxed peak callout above the highest stack (The Mechanics peakMarker). */
  function peak(formatter = (p) => p.value) {
    return {
      silent: true, symbol: 'rect', symbolSize: 1, itemStyle: { color: 'transparent' },
      label: { show: true, position: 'top', distance: 8, color: v('--tm-text'),
               backgroundColor: v('--tm-surface'), borderColor: v('--tm-line-1'), borderWidth: 1,
               padding: [3, 6], fontFamily: v('--tm-font-mono'), fontSize: 11, fontWeight: 600, formatter },
      data: [{ type: 'max' }],
    };
  }
  /** drop series that are all zero/null — no empty legends */
  const nonEmpty = (series) => series.filter((s) => (s.data || []).some((d) => (Array.isArray(d) ? d[1] : d?.value ?? d) > 0));

  /** Tooltip markup in The Mechanics chart-tooltip grammar: bold mono title, swatch · name · value rows. */
  function tipTitle(text) { return `<div class="tm-chart-tooltip__title">${esc(text)}</div>`; }
  function tipRow(name, value, { color, muted, html } = {}) {
    const swatch = color ? `<span class="tm-legend__swatch" style="--_series:${esc(color)}"></span>` : '';
    return `<div class="tm-chart-tooltip__row${muted ? ' aiu-tip-muted' : ''}">${swatch}<span>${html ? name : esc(name)}</span><span>${html ? value : esc(value)}</span></div>`;
  }
  /** axis tooltip: title, swatch rows with mono values, hides zero rows */
  function tooltipFormatter(valueFmt = fmt.tokens) {
    return (params) => {
      const rows = params
        .filter((p) => (Array.isArray(p.value) ? p.value[1] : p.value) > 0)
        .map((p) => tipRow(p.seriesName, valueFmt(Array.isArray(p.value) ? p.value[1] : p.value), { color: p.color }));
      return tipTitle(params[0]?.axisValueLabel ?? '') + (rows.join('') || tipRow('no data', '', { muted: true }));
    };
  }

  global.AIUsage = { THEME, build, register, init, refresh, colorFor, shade, fmt, RANGES, bucketFor,
                     bar, peak, nonEmpty, tooltipFormatter, tipTitle, tipRow, esc };
})(window);
