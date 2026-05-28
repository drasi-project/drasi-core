// Tests for custom Handlebars helpers registered in widgets.js.
// Run: node tests/handlebars_helpers.test.js

const { readFileSync } = require("fs");
const path = require("path");
const vm = require("vm");

// Load Handlebars via vm so the UMD picks up our fake environment
const handlebarsSource = readFileSync(
  path.join(__dirname, "../static/js/vendor/handlebars.min.js"),
  "utf-8"
);

const sandbox = { module: { exports: {} }, exports: {}, global: {}, self: {} };
vm.runInNewContext(handlebarsSource, sandbox, { filename: "handlebars.min.js" });
const Handlebars = sandbox.module.exports;

// ─── Register helpers (extracted logic from widgets.js) ─────────────
const Hbs = Handlebars;

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

Hbs.registerHelper("link", function (url, text) {
  const href = String(url ?? "");
  const label = typeof text === "string" ? text : href;
  return new Hbs.SafeString(
    `<a href="${href.replace(/"/g, "&quot;")}" target="_blank" rel="noopener noreferrer">${Hbs.Utils.escapeExpression(label)}</a>`
  );
});

Hbs.registerHelper("sortBy", function (rows, field, order, options) {
  if (typeof order === "object") { options = order; order = "asc"; }
  const arr = Array.isArray(rows) ? [...rows] : [];
  arr.sort((a, b) => {
    const va = a?.[field], vb = b?.[field];
    const na = Number(va), nb = Number(vb);
    let cmp;
    if (Number.isFinite(na) && Number.isFinite(nb)) {
      cmp = na - nb;
    } else {
      cmp = String(va ?? "").localeCompare(String(vb ?? ""));
    }
    return order === "desc" ? -cmp : cmp;
  });
  let result = "";
  for (let i = 0; i < arr.length; i++) {
    result += options.fn(arr[i], { data: { ...options.data, index: i, first: i === 0, last: i === arr.length - 1 } });
  }
  return result;
});

Hbs.registerHelper("html", function (content) {
  return new Hbs.SafeString(String(content ?? ""));
});

// ─── Test runner ────────────────────────────────────────────────────
let passed = 0;
let failed = 0;

function assert(condition, message) {
  if (condition) {
    passed++;
    console.log(`  ✓ ${message}`);
  } else {
    failed++;
    console.error(`  ✗ ${message}`);
  }
}

function assertEqual(actual, expected, message) {
  if (actual === expected) {
    passed++;
    console.log(`  ✓ ${message}`);
  } else {
    failed++;
    console.error(`  ✗ ${message}`);
    console.error(`    expected: ${JSON.stringify(expected)}`);
    console.error(`    actual:   ${JSON.stringify(actual)}`);
  }
}

// ─── Tests ──────────────────────────────────────────────────────────

console.log("\n=== link helper ===");

{
  const tpl = Hbs.compile('{{link url "Click here"}}');
  const result = tpl({ url: "https://example.com" });
  assertEqual(
    result,
    '<a href="https://example.com" target="_blank" rel="noopener noreferrer">Click here</a>',
    "link with url and text"
  );
}

{
  const tpl = Hbs.compile("{{link url}}");
  const result = tpl({ url: "https://example.com" });
  assertEqual(
    result,
    '<a href="https://example.com" target="_blank" rel="noopener noreferrer">https://example.com</a>',
    "link with url only (text defaults to url)"
  );
}

{
  const tpl = Hbs.compile('{{link url "<script>alert(1)</script>"}}');
  const result = tpl({ url: "https://example.com" });
  assert(
    result.includes("&lt;script&gt;") && !result.includes("<script>"),
    "link escapes HTML in label text"
  );
}

{
  const tpl = Hbs.compile('{{link url "test"}}');
  const result = tpl({ url: 'https://example.com/path?a=1&b="quoted"' });
  assert(
    result.includes("&quot;quoted&quot;") && !result.includes('""'),
    "link escapes quotes in href"
  );
}

console.log("\n=== sortBy helper ===");

{
  const tpl = Hbs.compile("{{#sortBy rows \"value\"}}{{this.name}}:{{this.value}} {{/sortBy}}");
  const result = tpl({
    rows: [
      { name: "C", value: 30 },
      { name: "A", value: 10 },
      { name: "B", value: 20 },
    ],
  });
  assertEqual(result, "A:10 B:20 C:30 ", "sortBy ascending by numeric field");
}

{
  const tpl = Hbs.compile('{{#sortBy rows "value" "desc"}}{{this.name}}:{{this.value}} {{/sortBy}}');
  const result = tpl({
    rows: [
      { name: "A", value: 10 },
      { name: "C", value: 30 },
      { name: "B", value: 20 },
    ],
  });
  assertEqual(result, "C:30 B:20 A:10 ", "sortBy descending by numeric field");
}

{
  const tpl = Hbs.compile("{{#sortBy rows \"name\"}}{{this.name}} {{/sortBy}}");
  const result = tpl({
    rows: [
      { name: "Charlie" },
      { name: "Alice" },
      { name: "Bob" },
    ],
  });
  assertEqual(result, "Alice Bob Charlie ", "sortBy ascending by string field");
}

{
  const tpl = Hbs.compile("{{#sortBy rows \"value\"}}{{@index}} {{/sortBy}}");
  const result = tpl({
    rows: [
      { value: 3 },
      { value: 1 },
      { value: 2 },
    ],
  });
  assertEqual(result, "0 1 2 ", "sortBy provides @index data variable");
}

{
  const tpl = Hbs.compile("{{#sortBy rows \"value\"}}{{/sortBy}}");
  const result = tpl({ rows: [] });
  assertEqual(result, "", "sortBy handles empty rows");
}

console.log("\n=== html helper ===");

{
  const tpl = Hbs.compile("{{html content}}");
  const result = tpl({ content: "<strong>Bold</strong>" });
  assertEqual(result, "<strong>Bold</strong>", "html helper outputs raw HTML");
}

{
  const tpl = Hbs.compile("{{content}}");
  const result = tpl({ content: "<strong>Bold</strong>" });
  assertEqual(
    result,
    "&lt;strong&gt;Bold&lt;/strong&gt;",
    "without html helper, content is escaped"
  );
}

{
  const tpl = Hbs.compile("{{html content}}");
  const result = tpl({ content: null });
  assertEqual(result, "", "html helper handles null gracefully");
}

{
  const tpl = Hbs.compile("{{html content}}");
  const result = tpl({});
  assertEqual(result, "", "html helper handles undefined gracefully");
}

// ─── Summary ────────────────────────────────────────────────────────
console.log(`\n${passed + failed} tests, ${passed} passed, ${failed} failed\n`);
process.exit(failed > 0 ? 1 : 0);
