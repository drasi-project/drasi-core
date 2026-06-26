const chartInstances = new Map();

// ─── Vibrant dark color palette for charts ──────────────────
const CHART_COLORS = [
  "#4ade80", "#34d399", "#fbbf24", "#f87171", "#60a5fa",
  "#c084fc", "#2dd4bf", "#fb923c", "#a78bfa", "#38bdf8",
];

// ─── Register custom dark ECharts theme ─────────────────────
const DARK_THEME = {
  backgroundColor: "transparent",
  textStyle: { color: "#9d9db5" },
  title: { textStyle: { color: "#f0f0f5" }, subtextStyle: { color: "#6b6b80" } },
  legend: { textStyle: { color: "#9d9db5" } },
  tooltip: {
    backgroundColor: "rgba(26, 40, 32, 0.95)",
    borderColor: "rgba(255,255,255,0.1)",
    textStyle: { color: "#f0f0f5", fontSize: 12 },
    extraCssText: "border-radius:8px;box-shadow:0 4px 16px rgba(0,0,0,0.5);backdrop-filter:blur(8px);",
  },
  categoryAxis: {
    axisLine: { lineStyle: { color: "rgba(255,255,255,0.08)" } },
    axisTick: { lineStyle: { color: "rgba(255,255,255,0.08)" } },
    axisLabel: { color: "#6b6b80" },
    splitLine: { lineStyle: { color: "rgba(255,255,255,0.04)" } },
  },
  valueAxis: {
    axisLine: { lineStyle: { color: "rgba(255,255,255,0.08)" } },
    axisTick: { lineStyle: { color: "rgba(255,255,255,0.08)" } },
    axisLabel: { color: "#6b6b80" },
    splitLine: { lineStyle: { color: "rgba(255,255,255,0.06)" } },
  },
  color: CHART_COLORS,
};

const LIGHT_THEME = {
  backgroundColor: "transparent",
  textStyle: { color: "#4a5e52" },
  title: { textStyle: { color: "#1a2e22" }, subtextStyle: { color: "#7a8e82" } },
  legend: { textStyle: { color: "#4a5e52" } },
  tooltip: {
    backgroundColor: "rgba(255, 255, 255, 0.95)",
    borderColor: "rgba(0,0,0,0.1)",
    textStyle: { color: "#1a2e22", fontSize: 12 },
    extraCssText: "border-radius:8px;box-shadow:0 4px 16px rgba(0,0,0,0.1);backdrop-filter:blur(8px);",
  },
  categoryAxis: {
    axisLine: { lineStyle: { color: "rgba(0,0,0,0.1)" } },
    axisTick: { lineStyle: { color: "rgba(0,0,0,0.1)" } },
    axisLabel: { color: "#7a8e82" },
    splitLine: { lineStyle: { color: "rgba(0,0,0,0.06)" } },
  },
  valueAxis: {
    axisLine: { lineStyle: { color: "rgba(0,0,0,0.1)" } },
    axisTick: { lineStyle: { color: "rgba(0,0,0,0.1)" } },
    axisLabel: { color: "#7a8e82" },
    splitLine: { lineStyle: { color: "rgba(0,0,0,0.06)" } },
  },
  color: CHART_COLORS,
};

if (window.echarts) {
  window.echarts.registerTheme("drasi-dark", DARK_THEME);
  window.echarts.registerTheme("drasi-light", LIGHT_THEME);
}

function getEChartsTheme() {
  return document.documentElement.getAttribute("data-theme") === "light" ? "drasi-light" : "drasi-dark";
}

// ─── Shared sort comparator ─────────────────────────────────
function compareByField(a, b, field, order = "asc") {
  const va = a?.[field], vb = b?.[field];
  const na = Number(va), nb = Number(vb);
  let cmp;
  if (Number.isFinite(na) && Number.isFinite(nb)) {
    cmp = na - nb;
  } else {
    cmp = String(va ?? "").localeCompare(String(vb ?? ""));
  }
  return order === "desc" ? -cmp : cmp;
}

// ─── Handlebars helpers ─────────────────────────────────────
if (window.Handlebars) {
  const Hbs = window.Handlebars;

  Hbs.registerHelper("sum", function (field, options) {
    const rows = options?.data?.root?.rows ?? [];
    return rows.reduce((acc, r) => acc + (Number(r?.[field]) || 0), 0);
  });

  Hbs.registerHelper("avg", function (field, options) {
    const rows = options?.data?.root?.rows ?? [];
    if (rows.length === 0) return 0;
    const sum = rows.reduce((acc, r) => acc + (Number(r?.[field]) || 0), 0);
    return (sum / rows.length).toFixed(2);
  });

  Hbs.registerHelper("min", function (field, options) {
    const rows = options?.data?.root?.rows ?? [];
    if (rows.length === 0) return 0;
    const val = Math.min(...rows.map((r) => Number(r?.[field]) || 0));
    return Number.isInteger(val) ? val : Number(val.toFixed(2));
  });

  Hbs.registerHelper("max", function (field, options) {
    const rows = options?.data?.root?.rows ?? [];
    if (rows.length === 0) return 0;
    const val = Math.max(...rows.map((r) => Number(r?.[field]) || 0));
    return Number.isInteger(val) ? val : Number(val.toFixed(2));
  });

  Hbs.registerHelper("count", function (options) {
    return (options?.data?.root?.rows ?? []).length;
  });

  Hbs.registerHelper("format", function (value, style) {
    const n = Number(value);
    if (!Number.isFinite(n)) return value;
    switch (style) {
      case "currency": return "$" + n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
      case "percent": return n.toFixed(2) + "%";
      case "compact":
        if (Math.abs(n) >= 1e9) return (n / 1e9).toFixed(1) + "B";
        if (Math.abs(n) >= 1e6) return (n / 1e6).toFixed(1) + "M";
        if (Math.abs(n) >= 1e3) return (n / 1e3).toFixed(1) + "K";
        return n.toLocaleString();
      default: return n.toLocaleString();
    }
  });

  Hbs.registerHelper("eq", function (a, b) { return a === b; });
  Hbs.registerHelper("gt", function (a, b) { return Number(a) > Number(b); });
  Hbs.registerHelper("lt", function (a, b) { return Number(a) < Number(b); });
  Hbs.registerHelper("gte", function (a, b) { return Number(a) >= Number(b); });
  Hbs.registerHelper("lte", function (a, b) { return Number(a) <= Number(b); });

  // Hyperlink helper — generates an <a> tag that opens in a new tab.
  // Usage: {{link url "Link Text"}} or {{link url}} (defaults text to the URL)
  Hbs.registerHelper("link", function (url, text) {
    const href = String(url ?? "");
    const label = typeof text === "string" ? text : href;
    // Only allow safe URL schemes
    const allowedScheme = /^(https?:|mailto:|\/|#)/i;
    if (href && !allowedScheme.test(href)) {
      return new Hbs.SafeString(Hbs.Utils.escapeExpression(label));
    }
    return new Hbs.SafeString(
      `<a href="${Hbs.Utils.escapeExpression(href)}" target="_blank" rel="noopener noreferrer">${Hbs.Utils.escapeExpression(label)}</a>`
    );
  });

  // Sort helper — block helper that sorts rows by a field.
  // Usage: {{#sortBy rows "field"}}...{{/sortBy}} or {{#sortBy rows "field" "desc"}}...{{/sortBy}}
  Hbs.registerHelper("sortBy", function (rows, field, order, options) {
    if (typeof order === "object") { options = order; order = "asc"; }
    const arr = Array.isArray(rows) ? [...rows] : [];
    arr.sort((a, b) => compareByField(a, b, field, order));
    let result = "";
    for (let i = 0; i < arr.length; i++) {
      result += options.fn(arr[i], { data: { ...options.data, index: i, first: i === 0, last: i === arr.length - 1 } });
    }
    return new Hbs.SafeString(result);
  });

  // Group helper — block helper that groups rows by a field value.
  // Usage: {{#groupBy rows "field"}}{{@key}}: {{#each this}}...{{/each}}{{/groupBy}}
  Hbs.registerHelper("groupBy", function (rows, field, options) {
    const arr = Array.isArray(rows) ? rows : [];
    const groups = {};
    for (const row of arr) {
      const key = String(row?.[field] ?? "");
      if (!groups[key]) groups[key] = [];
      groups[key].push(row);
    }
    const sortedKeys = Object.keys(groups).sort((a, b) => a.localeCompare(b));
    let result = "";
    for (const key of sortedKeys) {
      result += options.fn(groups[key], { data: { ...options.data, key } });
    }
    return result;
  });

  // HTML helper — marks content as safe HTML (bypasses Handlebars escaping).
  // Sanitized by DOMPurify if available; otherwise falls back to escaping.
  // Usage: {{html myHtmlContent}}
  Hbs.registerHelper("html", function (content) {
    const raw = String(content ?? "");
    if (window.DOMPurify) {
      return new Hbs.SafeString(window.DOMPurify.sanitize(raw, { ADD_ATTR: ["target", "rel"] }));
    }
    // No DOMPurify available — escape to prevent XSS
    return Hbs.Utils.escapeExpression(raw);
  });

  // Replace helper — replaces all occurrences of a substring.
  // Usage: {{replace value "search" "replacement"}}
  Hbs.registerHelper("replace", function (value, search, replacement) {
    if (typeof value !== "string") return value;
    if (typeof search !== "string") return value;
    if (typeof replacement !== "string") replacement = "";
    return value.split(search).join(replacement);
  });

  // TrimPrefix helper — removes a prefix from the start of a string.
  // Usage: {{trimPrefix value "prefix"}}
  Hbs.registerHelper("trimPrefix", function (value, prefix) {
    const str = String(value ?? "");
    if (typeof prefix === "string" && str.startsWith(prefix)) {
      return str.slice(prefix.length);
    }
    return str;
  });
}

export const WIDGET_TYPES = [
  "line_chart",
  "bar_chart",
  "pie_chart",
  "table",
  "gauge",
  "kpi",
  "text",
  "map",
];

// ─── Widget type metadata for picker ────────────────────────
export const WIDGET_TYPE_META = {
  line_chart: {
    name: "Line Chart",
    description: "Track trends over time",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="22 12 18 12 15 21 9 3 6 12 2 12"/></svg>`,
    color: "#4ade80",
  },
  bar_chart: {
    name: "Bar Chart",
    description: "Compare values across categories",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><rect x="3" y="12" width="4" height="9" rx="1"/><rect x="10" y="6" width="4" height="15" rx="1"/><rect x="17" y="2" width="4" height="19" rx="1"/></svg>`,
    color: "#34d399",
  },
  pie_chart: {
    name: "Pie Chart",
    description: "Show proportions of a whole",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M21.21 15.89A10 10 0 1 1 8 2.83"/><path d="M22 12A10 10 0 0 0 12 2v10z"/></svg>`,
    color: "#fbbf24",
  },
  table: {
    name: "Table",
    description: "Display data in rows and columns",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><rect x="3" y="3" width="18" height="18" rx="2"/><line x1="3" y1="9" x2="21" y2="9"/><line x1="3" y1="15" x2="21" y2="15"/><line x1="9" y1="3" x2="9" y2="21"/></svg>`,
    color: "#60a5fa",
  },
  gauge: {
    name: "Gauge",
    description: "Show a single metric on a dial",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/></svg>`,
    color: "#f87171",
  },
  kpi: {
    name: "KPI",
    description: "Highlight a key number at a glance",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M12 20V10"/><path d="M18 20V4"/><path d="M6 20v-4"/></svg>`,
    color: "#c084fc",
  },
  text: {
    name: "Markdown",
    description: "Rich text with Handlebars templates",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><polyline points="4 7 4 4 20 4 20 7"/><line x1="9" y1="20" x2="15" y2="20"/><line x1="12" y1="4" x2="12" y2="20"/></svg>`,
    color: "#9d9db5",
  },
  map: {
    name: "Map",
    description: "Plot data points geographically",
    icon: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><polygon points="1 6 1 22 8 18 16 22 23 18 23 2 16 6 8 2 1 6"/><line x1="8" y1="2" x2="8" y2="18"/><line x1="16" y1="6" x2="16" y2="22"/></svg>`,
    color: "#2dd4bf",
  },
};

// ─── Default widget config per type ─────────────────────────
export function getDefaultConfig(widgetType) {
  const base = { queryId: "" };
  switch (widgetType) {
    case "line_chart": return { ...base, xField: "timestamp", yFields: ["value"], maxPoints: 100 };
    case "bar_chart": return { ...base, categoryField: "name", valueFields: ["value"] };
    case "pie_chart": return { ...base, nameField: "name", valueField: "value" };
    case "table": return { ...base, columns: [] };
    case "gauge": return { ...base, valueField: "value", min: 0, max: 100, aggregation: "last", filterField: "", filterValue: "" };
    case "kpi": return { ...base, valueField: "value", label: "KPI", aggregation: "last", filterField: "", filterValue: "" };
    case "text": return { ...base, template: "## Hello\n\n{{#each rows}}\n- **{{this.name}}**: {{this.value}}\n{{/each}}" };
    case "map": return { ...base, latField: "lat", lngField: "lng", valueField: "value" };
    default: return base;
  }
}

export function createWidgetRuntime() {
  return { rows: [], aggregation: null, latest: null };
}

export function createDefaultWidget(widgetType, index = 0) {
  const meta = WIDGET_TYPE_META[widgetType] || { name: widgetType };
  // Per-type default sizes for better UX
  const sizes = {
    table: { w: 6, h: 5 },
    bar_chart: { w: 4, h: 5 },
    line_chart: { w: 6, h: 5 },
    pie_chart: { w: 4, h: 5 },
    gauge: { w: 3, h: 4 },
    kpi: { w: 3, h: 3 },
    text: { w: 4, h: 4 },
    map: { w: 6, h: 5 },
  };
  const size = sizes[widgetType] || { w: 4, h: 4 };
  return {
    id: crypto.randomUUID(),
    type: widgetType,
    title: `${meta.name} ${index + 1}`,
    grid: { x: 0, y: 0, w: size.w, h: size.h, autoPosition: true },
    config: getDefaultConfig(widgetType),
  };
}

// ─── Runtime data accumulation ──────────────────────────────

// The engine stamps every row with a `row_signature` (delivered as `k`) that is
// the canonical row identity. We key rows by it when known, attaching it to the
// stored row object as a non-enumerable `__sig` so it never leaks into rendering
// (templates, table columns, JSON). When the signature is unknown (0/absent) we
// fall back to an `id`/equality heuristic.

function rowSig(value) {
  return typeof value === "number" && value > 0 ? value : 0;
}

function tagSig(row, sig) {
  if (row && typeof row === "object" && sig) {
    Object.defineProperty(row, "__sig", {
      value: sig,
      enumerable: false,
      configurable: true,
      writable: true,
    });
  }
  return row;
}

function rowKey(row) {
  if (row && typeof row === "object") {
    const sig = rowSig(row.__sig);
    if (sig) return `sig:${sig}`;
    if (row.id !== undefined) return `id:${row.id}`;
    return JSON.stringify(row);
  }
  return JSON.stringify(row);
}

// Identity key for an incoming diff payload, given its signature and data.
function diffKey(sig, data) {
  if (sig) return `sig:${sig}`;
  if (data && typeof data === "object" && data.id !== undefined) return `id:${data.id}`;
  return JSON.stringify(data);
}

function findRowIndexByKey(rows, key) {
  return rows.findIndex((r) => rowKey(r) === key);
}

function normalizeDiffData(diff) {
  if (diff.op === "add" || diff.op === "delete") return diff.data ?? null;
  if (diff.op === "update") return diff.after ?? diff.data ?? null;
  if (diff.op === "aggregation") return diff.after ?? null;
  return null;
}

export function applyResultDiff(runtime, diff) {
  if (!runtime) return;

  const sig = rowSig(diff.k ?? diff.row_signature);

  if (diff.op === "add") {
    const row = normalizeDiffData(diff);
    if (!row) return;
    // Insert-of-an-existing-element is an upsert in drasi: replace an existing
    // row with the same identity instead of appending a duplicate (issue #605).
    const idx = findRowIndexByKey(runtime.rows, diffKey(sig, row));
    tagSig(row, sig);
    if (idx >= 0) runtime.rows[idx] = row;
    else runtime.rows.push(row);
    runtime.latest = row;
    return;
  }
  if (diff.op === "update") {
    const after = normalizeDiffData(diff);
    if (!after) return;
    // row_signature is stable across an update; match by it first, then fall
    // back to the `before` state, then the `after` key.
    let idx = sig ? findRowIndexByKey(runtime.rows, `sig:${sig}`) : -1;
    if (idx < 0 && diff.before) idx = findRowIndexByKey(runtime.rows, diffKey(0, diff.before));
    if (idx < 0) idx = findRowIndexByKey(runtime.rows, diffKey(sig, after));
    tagSig(after, sig);
    if (idx >= 0) runtime.rows[idx] = after;
    else runtime.rows.push(after);
    runtime.latest = after;
    return;
  }
  if (diff.op === "delete") {
    const row = normalizeDiffData(diff);
    const idx = findRowIndexByKey(runtime.rows, diffKey(sig, row));
    if (idx >= 0) runtime.rows.splice(idx, 1);
    runtime.latest = runtime.rows.length > 0 ? runtime.rows[runtime.rows.length - 1] : null;
    return;
  }
  if (diff.op === "aggregation") {
    runtime.aggregation = normalizeDiffData(diff);
    runtime.latest = runtime.aggregation;
  }
}

// ─── Helpers ────────────────────────────────────────────────

function asNumber(v) {
  if (typeof v === "number") return v;
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

function asRows(rt) {
  return Array.isArray(rt?.rows) ? rt.rows : [];
}

function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}

function getLatestValue(rt, field) {
  const src = rt?.aggregation ?? rt?.latest;
  return src && typeof src === "object" ? asNumber(src[field]) : 0;
}

// Aggregation modes for KPI/Gauge widgets
const AGGREGATION_MODES = ["last", "first", "sum", "avg", "count", "min", "max", "filter"];
export { AGGREGATION_MODES };

function getAggregatedValue(rt, field, mode = "last", filterField = "", filterValue = "") {
  // If query returned an aggregation result, use it directly
  if (rt?.aggregation && typeof rt.aggregation === "object") {
    return asNumber(rt.aggregation[field]);
  }

  const rows = asRows(rt);
  if (rows.length === 0) return 0;

  switch (mode) {
    case "first":
      return asNumber(rows[0]?.[field]);
    case "last":
      return asNumber(rows[rows.length - 1]?.[field]);
    case "sum":
      return rows.reduce((acc, r) => acc + asNumber(r?.[field]), 0);
    case "avg": {
      const sum = rows.reduce((acc, r) => acc + asNumber(r?.[field]), 0);
      return rows.length > 0 ? sum / rows.length : 0;
    }
    case "count":
      return rows.length;
    case "min":
      return Math.min(...rows.map((r) => asNumber(r?.[field])));
    case "max":
      return Math.max(...rows.map((r) => asNumber(r?.[field])));
    case "filter": {
      const match = rows.find((r) => String(r?.[filterField]) === String(filterValue));
      return match ? asNumber(match[field]) : 0;
    }
    default:
      return asNumber(rows[rows.length - 1]?.[field]);
  }
}

function formatNumber(n) {
  if (Math.abs(n) >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (Math.abs(n) >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (Math.abs(n) >= 1e3) return (n / 1e3).toFixed(1) + "K";
  if (Number.isInteger(n)) return n.toLocaleString();
  return n.toFixed(2);
}

/**
 * Detect available field names from runtime rows.
 * @returns {string[]}
 */
export function detectFields(runtime) {
  const rows = asRows(runtime);
  if (rows.length === 0) return [];
  const keys = new Set();
  for (const row of rows.slice(0, 10)) {
    if (row && typeof row === "object") {
      for (const k of Object.keys(row)) keys.add(k);
    }
  }
  return Array.from(keys);
}

// ─── Chart instance management ──────────────────────────────

function getThemeColors() {
  const style = getComputedStyle(document.documentElement);
  return {
    accent: style.getPropertyValue("--accent").trim(),
    textPrimary: style.getPropertyValue("--text-primary").trim(),
    textSecondary: style.getPropertyValue("--text-secondary").trim(),
    textMuted: style.getPropertyValue("--text-muted").trim(),
    bgElevated: style.getPropertyValue("--bg-elevated").trim(),
    borderSubtle: style.getPropertyValue("--border-subtle").trim(),
  };
}

function getChart(container) {
  const existing = chartInstances.get(container);
  if (existing) return existing;
  const chart = window.echarts.init(container, getEChartsTheme());
  chartInstances.set(container, chart);
  return chart;
}

export function resizeAllCharts() {
  for (const chart of chartInstances.values()) {
    chart.resize();
  }
}

export function reThemeAllCharts() {
  const theme = getEChartsTheme();
  for (const [container, chart] of chartInstances.entries()) {
    const opts = chart.getOption();
    chart.dispose();
    const newChart = window.echarts.init(container, theme);
    newChart.setOption(opts);
    chartInstances.set(container, newChart);
  }
}

export function disposeWidgetChart(container) {
  if (!container) return;
  // Find the chart sub-container (.widget-chart or .widget-map)
  const chartEl = container.querySelector(".widget-chart, .widget-map") || container;
  const chart = chartInstances.get(chartEl);
  if (chart) {
    chart.dispose();
    chartInstances.delete(chartEl);
  }
}

// ─── Renderers ──────────────────────────────────────────────

function renderLineChart(widget, runtime, container) {
  const rows = asRows(runtime);
  const xField = widget.config?.xField ?? "timestamp";
  const yFields = widget.config?.yFields ?? ["value"];
  const maxPts = asNumber(widget.config?.maxPoints ?? 100);
  const bounded = rows.slice(Math.max(0, rows.length - maxPts));

  const chart = getChart(container);
  chart.setOption({
    grid: { left: 48, right: 16, top: 16, bottom: 32 },
    tooltip: { trigger: "axis" },
    xAxis: { type: "category", data: bounded.map((r) => r?.[xField] ?? ""), boundaryGap: false },
    yAxis: { type: "value" },
    series: yFields.map((f, i) => ({
      name: f,
      type: "line",
      smooth: true,
      showSymbol: false,
      lineStyle: { width: 2 },
      areaStyle: { opacity: 0.08 },
      data: bounded.map((r) => asNumber(r?.[f])),
      color: CHART_COLORS[i % CHART_COLORS.length],
    })),
  }, true);
}

function renderBarChart(widget, runtime, container) {
  const rows = asRows(runtime);
  const catField = widget.config?.categoryField ?? "name";
  const valFields = widget.config?.valueFields ?? ["value"];

  const chart = getChart(container);
  chart.setOption({
    grid: { left: 48, right: 16, top: 16, bottom: 32 },
    tooltip: { trigger: "axis" },
    xAxis: { type: "category", data: rows.map((r) => r?.[catField] ?? ""), axisLabel: { rotate: rows.length > 8 ? 30 : 0 } },
    yAxis: { type: "value" },
    series: valFields.map((f, i) => ({
      name: f,
      type: "bar",
      barMaxWidth: 40,
      itemStyle: { borderRadius: [4, 4, 0, 0] },
      data: rows.map((r) => asNumber(r?.[f])),
      color: CHART_COLORS[i % CHART_COLORS.length],
    })),
  }, true);
}

function renderPieChart(widget, runtime, container) {
  const rows = asRows(runtime);
  const nameF = widget.config?.nameField ?? "name";
  const valF = widget.config?.valueField ?? "value";

  const chart = getChart(container);
  const tc = getThemeColors();
  chart.setOption({
    tooltip: { trigger: "item", formatter: "{b}: {c} ({d}%)" },
    series: [{
      type: "pie",
      radius: ["35%", "65%"],
      itemStyle: { borderRadius: 6, borderColor: tc.bgElevated, borderWidth: 2 },
      label: { color: tc.textSecondary, fontSize: 11 },
      data: rows.map((r) => ({ name: r?.[nameF] ?? "item", value: asNumber(r?.[valF]) })),
    }],
  }, true);
}

function renderGauge(widget, runtime, container) {
  const min = asNumber(widget.config?.min ?? 0);
  const max = asNumber(widget.config?.max ?? 100);
  const valF = widget.config?.valueField ?? "value";
  const mode = widget.config?.aggregation ?? "last";
  const value = getAggregatedValue(runtime, valF, mode, widget.config?.filterField, widget.config?.filterValue);

  const chart = getChart(container);
  const tc = getThemeColors();
  chart.setOption({
    series: [{
      type: "gauge",
      min,
      max,
      progress: { show: true, width: 12, itemStyle: { color: tc.accent } },
      axisLine: { lineStyle: { width: 12, color: [[1, tc.borderSubtle]] } },
      axisTick: { show: false },
      splitLine: { length: 8, lineStyle: { width: 1.5, color: tc.borderSubtle } },
      axisLabel: { distance: 16, color: tc.textMuted, fontSize: 10 },
      pointer: { length: "55%", width: 4, itemStyle: { color: tc.accent } },
      anchor: { show: true, size: 10, itemStyle: { color: tc.accent, borderColor: tc.bgElevated, borderWidth: 3 } },
      detail: { valueAnimation: true, fontSize: 20, fontWeight: 700, color: tc.textPrimary, offsetCenter: [0, "70%"], formatter: (v) => formatNumber(v) },
      data: [{ value }],
    }],
  }, true);
}

function renderMap(widget, runtime, container) {
  const rows = asRows(runtime);
  const latF = widget.config?.latField ?? "lat";
  const lngF = widget.config?.lngField ?? "lng";
  const valF = widget.config?.valueField ?? "value";

  const chart = getChart(container);
  chart.setOption({
    tooltip: { trigger: "item", formatter: (p) => `lng: ${p.value[0]}<br/>lat: ${p.value[1]}<br/>value: ${p.value[2]}` },
    xAxis: { type: "value", name: "lng" },
    yAxis: { type: "value", name: "lat" },
    series: [{
      type: "scatter",
      symbolSize: (v) => Math.max(8, Math.min(30, v[2])),
      itemStyle: { color: "#2dd4bf", shadowBlur: 10, shadowColor: "rgba(45, 212, 191, 0.4)" },
      data: rows.map((r) => [asNumber(r?.[lngF]), asNumber(r?.[latF]), asNumber(r?.[valF])]),
    }],
  }, true);
}


function renderTable(widget, runtime, container) {
  // Store latest runtime on container so click handlers always use fresh data
  container.__runtime = runtime;

  let rows = asRows(runtime);
  const cfgCols = widget.config?.columns ?? [];
  const columns = cfgCols.length > 0 ? cfgCols : Object.keys(rows[0] ?? {});

  // Apply sort if active (sort state stored on container, GC'd with DOM element)
  const sortState = container.__sortState;
  if (sortState) {
    rows = [...rows].sort((a, b) => compareByField(a, b, sortState.field, sortState.order));
  }

  const isNumeric = (v) => typeof v === "number" || (typeof v === "string" && /^-?\d+(\.\d+)?$/.test(v));

  const appendCellContent = (td, v) => {
    if (v == null) {
      const span = document.createElement("span");
      span.style.color = "var(--text-muted)";
      span.textContent = "—";
      td.appendChild(span);
      return;
    }
    if (isNumeric(v)) {
      const span = document.createElement("span");
      span.style.fontVariantNumeric = "tabular-nums";
      span.textContent = formatNumber(Number(v));
      td.appendChild(span);
      return;
    }
    td.textContent = String(v);
  };

  const table = document.createElement("table");
  table.className = "widget-table";

  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  columns.forEach((c) => {
    const th = document.createElement("th");
    th.style.cursor = "pointer";
    th.style.userSelect = "none";
    const label = String(c);
    const arrow = sortState?.field === c ? (sortState.order === "asc" ? " ▲" : " ▼") : "";
    th.textContent = label + arrow;
    th.addEventListener("click", () => {
      const current = container.__sortState;
      if (current?.field === c) {
        container.__sortState = { field: c, order: current.order === "asc" ? "desc" : "asc" };
      } else {
        container.__sortState = { field: c, order: "asc" };
      }
      renderTable(widget, container.__runtime, container);
    });
    headRow.appendChild(th);
  });
  thead.appendChild(headRow);

  const tbody = document.createElement("tbody");
  rows.forEach((r) => {
    const tr = document.createElement("tr");
    columns.forEach((c) => {
      const td = document.createElement("td");
      appendCellContent(td, r?.[c]);
      tr.appendChild(td);
    });
    tbody.appendChild(tr);
  });

  table.appendChild(thead);
  table.appendChild(tbody);
  container.replaceChildren(table);
}

function renderKpi(widget, runtime, container) {
  const valF = widget.config?.valueField ?? "value";
  const label = widget.config?.label ?? "KPI";
  const mode = widget.config?.aggregation ?? "last";
  const value = getAggregatedValue(runtime, valF, mode, widget.config?.filterField, widget.config?.filterValue);

  container.innerHTML = `
    <div class="widget-kpi">
      <div class="kpi-value">${formatNumber(value)}</div>
      <div class="kpi-label">${escapeHtml(label)}</div>
    </div>`;
}

function renderText(widget, runtime, container) {
  const template = widget.config?.template ?? "";
  const rows = asRows(runtime);
  const context = {
    rows,
    count: rows.length,
    latest: runtime?.latest ?? null,
    aggregation: runtime?.aggregation ?? null,
  };

  // Preserve scroll position across re-renders
  const existing = container.querySelector(".widget-markdown");
  const scrollTop = existing ? existing.scrollTop : 0;

  try {
    const compiled = Handlebars.compile(template);
    const markdown = compiled(context);
    const rawHtml = window.marked ? window.marked.parse(markdown) : markdown;
    const safeHtml = window.DOMPurify ? window.DOMPurify.sanitize(rawHtml, { ADD_ATTR: ["target", "rel"] }) : rawHtml;
    container.innerHTML = `<div class="widget-markdown">${safeHtml}</div>`;
  } catch (e) {
    container.innerHTML = `<div class="widget-markdown widget-error"><pre>Template error: ${escapeHtml(e.message)}</pre></div>`;
  }

  const updated = container.querySelector(".widget-markdown");
  if (updated) updated.scrollTop = scrollTop;
}

// ─── Public render dispatcher ───────────────────────────────

export function renderWidget(widget, runtime, container) {
  if (!widget || !container) return;

  if (widget.type === "text") { renderText(widget, runtime, container); return; }
  if (widget.type === "kpi") { renderKpi(widget, runtime, container); return; }
  if (widget.type === "table") { renderTable(widget, runtime, container); return; }

  // Chart types need a sub-container
  let chartEl = container.querySelector(".widget-chart, .widget-map");
  if (!chartEl) {
    container.innerHTML = `<div class="${widget.type === "map" ? "widget-map" : "widget-chart"}"></div>`;
    chartEl = container.firstElementChild;
  }
  if (!chartEl) return;

  switch (widget.type) {
    case "line_chart": renderLineChart(widget, runtime, chartEl); break;
    case "bar_chart": renderBarChart(widget, runtime, chartEl); break;
    case "pie_chart": renderPieChart(widget, runtime, chartEl); break;
    case "gauge": renderGauge(widget, runtime, chartEl); break;
    case "map": renderMap(widget, runtime, chartEl); break;
  }
}
