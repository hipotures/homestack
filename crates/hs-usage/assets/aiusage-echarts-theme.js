/* AI Usage — ECharts 6 theme + helpers
 *
 * Usage:
 *   <link rel="stylesheet" href="aiusage-tokens.css">
 *   <script src="echarts.min.js"></script>
 *   <script src="aiusage-echarts-theme.js"></script>
 *
 *   const chart = AIUsage.init(el);                // registers + inits with the current theme
 *   chart.setOption({ series: [AIUsage.bar('Codex · gpt-6.1-sol', data, 'gpt-6.1-sol')] });
 *
 * Colors are read from the CSS variables in aiusage-tokens.css, so a light/dark
 * switch is: change data-theme on <html>, then AIUsage.refresh().
 */
(function (global) {
  'use strict';

  const THEME = 'aiusage';
  const charts = new Set();

  const v = (name) =>
    getComputedStyle(document.documentElement).getPropertyValue(name).trim();

  const alpha = (hex, a) => {
    const h = hex.replace('#', '');
    const n = parseInt(h.length === 3 ? h.replace(/./g, '$&$&') : h, 16);
    return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
  };

  /* ---------- series colors: agent → hue, model → shade ---------- */
  const AGENT_VAR = { codex: '--codex', 'claude code': '--claude', claude: '--claude' };
  const MODEL_VAR = {
    'gpt-6.1-sol': '--m-gpt-6-1-sol',
    'claude-opus-5-5': '--m-claude-opus-5-5',
    'claude-haiku-5-5': '--m-claude-haiku-5-5',
    'claude-sonnet-5-5': '--m-claude-sonnet-5-5',
  };
  function colorFor(key) {
    const k = String(key || '').toLowerCase();
    if (MODEL_VAR[k]) return v(MODEL_VAR[k]);
    for (const m in MODEL_VAR) if (k.includes(m)) return v(MODEL_VAR[m]);
    for (const a in AGENT_VAR) {
      if (!k.startsWith(a)) continue;
      // "Agent · model" keys without a dedicated token get a stable shade of the agent hue.
      const rest = k.slice(a.length).replace(/^\s*·\s*/, '');
      if (!rest) return v(AGENT_VAR[a]);
      let h = 0;
      for (const ch of rest) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
      return v(AGENT_VAR[a] + ['', '-hi', '-lo'][h % 3]);
    }
    return v('--muted');
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
    '1h':   { label: '1h',       bucket: '2m',  ms: 2 * 60e3,        axis: 'HH:mm' },
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

  /* ---------- theme ---------- */
  function build() {
    const text = v('--text'), muted = v('--muted'), faint = v('--faint');
    const grid = v('--grid'), axis = v('--axis');
    const sans = v('--font-sans'), mono = v('--font-mono');

    const axisCommon = {
      axisLine: { show: false, lineStyle: { color: axis } },
      axisTick: { show: false },
      axisLabel: { color: faint, fontFamily: mono, fontSize: 11, margin: 12 },
      splitLine: { show: false, lineStyle: { color: grid, type: [3, 4] } },
      nameTextStyle: { color: muted, fontFamily: sans, fontSize: 11 },
    };

    return {
      color: [v('--codex'), v('--claude'), v('--tok-reasoning'), v('--tok-uncached'),
              v('--claude-lo'), v('--codex-lo'), v('--warn')],
      backgroundColor: 'transparent',
      textStyle: { fontFamily: sans, color: text },

      title: { left: 0, textStyle: { color: text, fontSize: 15, fontWeight: 600 },
               subtextStyle: { color: muted, fontSize: 12 } },

      grid: { left: 8, right: 8, top: 24, bottom: 8, containLabel: true },

      legend: {
        top: 0, right: 0, icon: 'roundRect', itemWidth: 10, itemHeight: 10, itemGap: 16,
        textStyle: { color: muted, fontSize: 12 },
        inactiveColor: faint,
      },

      tooltip: {
        trigger: 'axis',
        backgroundColor: v('--surface-3'),
        borderWidth: 1,
        borderColor: v('--border-strong'),
        padding: [8, 12],
        textStyle: { color: text, fontSize: 12, fontFamily: sans },
        extraCssText: 'border-radius:10px;box-shadow:0 8px 24px rgba(0,0,0,.35);backdrop-filter:blur(8px);',
        axisPointer: {
          type: 'shadow',
          shadowStyle: { color: alpha(v('--text').startsWith('#') ? v('--text') : '#ffffff', 0.04) },
          lineStyle: { color: axis, type: [3, 4] },
          crossStyle: { color: axis },
          label: { backgroundColor: v('--surface-3'), color: text, fontFamily: mono },
        },
      },

      categoryAxis: { ...axisCommon, axisLine: { show: true, lineStyle: { color: axis } } },
      timeAxis:     { ...axisCommon, axisLine: { show: true, lineStyle: { color: axis } } },
      valueAxis:    { ...axisCommon, splitNumber: 3, splitLine: { show: true, lineStyle: { color: grid, type: [3, 4] } } },
      logAxis:      { ...axisCommon, splitLine: { show: true, lineStyle: { color: grid, type: [3, 4] } } },

      bar: {
        barMaxWidth: 18, barCategoryGap: '30%',
        itemStyle: { borderRadius: [3, 3, 0, 0] },
        emphasis: { focus: 'series' },
      },
      line: {
        symbol: 'none', smooth: 0.25, lineStyle: { width: 2 },
        emphasis: { focus: 'series', lineStyle: { width: 2 } },
      },
      scatter: { symbolSize: 6 },
      heatmap: { itemStyle: { borderRadius: 4, borderColor: v('--surface'), borderWidth: 3 } },
      visualMap: {
        inRange: { color: [v('--heat-empty'), alpha(v('--codex'), 0.35), v('--codex')] },
        textStyle: { color: faint, fontSize: 11 },
        itemWidth: 10, itemHeight: 120,
      },
      dataZoom: {
        backgroundColor: 'transparent',
        borderColor: v('--border'),
        fillerColor: alpha(v('--codex'), 0.12),
        handleStyle: { color: v('--surface-3'), borderColor: v('--border-strong') },
        moveHandleStyle: { color: v('--surface-3') },
        dataBackground: { lineStyle: { color: faint, width: 1 }, areaStyle: { color: v('--track') } },
        selectedDataBackground: { lineStyle: { color: v('--codex') }, areaStyle: { color: alpha(v('--codex'), 0.2) } },
        textStyle: { color: faint, fontFamily: mono, fontSize: 11 },
      },
      markLine: { symbol: 'none', lineStyle: { color: axis, type: [3, 4] }, label: { color: muted, fontSize: 11 } },
      markArea: { itemStyle: { color: alpha('#ffffff', 0.025) }, label: { color: faint, fontSize: 11 } },
    };
  }

  function register() { echarts.registerTheme(THEME, build()); }

  function init(el, opts) {
    register();
    const c = echarts.init(el, THEME, { renderer: 'canvas', ...opts });
    charts.add(c);
    new ResizeObserver(() => c.resize()).observe(el);
    return c;
  }

  /** after toggling data-theme: rebuild the theme from CSS vars and repaint */
  function refresh() {
    register();
    for (const c of charts) {
      if (c.isDisposed()) { charts.delete(c); continue; }
      if (typeof c.setTheme === 'function') c.setTheme(THEME);   // ECharts 6
      else {                                                    // fallback
        const opt = c.getOption(), el = c.getDom();
        c.dispose(); charts.delete(c);
        init(el).setOption(opt);
      }
    }
  }

  /* ---------- series helpers ---------- */
  function bar(name, data, colorKey, extra) {
    return { type: 'bar', name, data, stack: 'total', itemStyle: { color: colorFor(colorKey || name) }, ...extra };
  }
  function area(name, data, colorKey, extra) {
    const c = colorFor(colorKey || name);
    return {
      type: 'line', name, data, stack: extra && extra.stack,
      lineStyle: { color: c, width: 2 }, itemStyle: { color: c },
      areaStyle: {
        color: new echarts.graphic.LinearGradient(0, 0, 0, 1, [
          { offset: 0, color: alpha(c, 0.28) }, { offset: 1, color: alpha(c, 0) },
        ]),
      },
      ...extra,
    };
  }
  /** visible gap in activity, e.g. [['01:08','01:16']] */
  function gaps(ranges, label = 'gap') {
    return { silent: true, data: ranges.map(([a, b]) => [{ xAxis: a, name: label }, { xAxis: b }]) };
  }
  /** peak marker with a compact label */
  function peak(formatter = (p) => p.value) {
    return {
      symbol: 'roundRect', symbolSize: [1, 1],
      label: { show: true, position: 'top', distance: 8, color: v('--text'),
               backgroundColor: v('--surface-3'), borderColor: v('--border-strong'), borderWidth: 1,
               borderRadius: 8, padding: [5, 9], fontFamily: v('--font-mono'), fontSize: 12,
               formatter },
      data: [{ type: 'max' }],
    };
  }
  /** drop series that are all zero/null — no empty legends */
  const nonEmpty = (series) => series.filter((s) => (s.data || []).some((d) => (Array.isArray(d) ? d[1] : d?.value ?? d) > 0));

  /** axis tooltip: name, colored dot, mono value, hides zero rows */
  function tooltipFormatter(valueFmt = fmt.tokens) {
    return (params) => {
      const rows = params
        .filter((p) => (Array.isArray(p.value) ? p.value[1] : p.value) > 0)
        .map((p) => {
          const val = Array.isArray(p.value) ? p.value[1] : p.value;
          return `<div style="display:flex;justify-content:space-between;gap:16px">
            <span>${p.marker}${p.seriesName}</span>
            <span style="font-family:${v('--font-mono')}">${valueFmt(val)}</span></div>`;
        });
      const head = `<div style="color:${v('--muted')};margin-bottom:4px">${params[0]?.axisValueLabel ?? ''}</div>`;
      return head + (rows.join('') || `<span style="color:${v('--faint')}">no data</span>`);
    };
  }

  global.AIUsage = { THEME, build, register, init, refresh, colorFor, fmt, RANGES, bucketFor,
                     bar, area, gaps, peak, nonEmpty, tooltipFormatter };
})(window);
