// Tests for custom Handlebars helpers registered in widgets.js.
// Run: node tests/handlebars_helpers.test.js
//
// This file loads the actual widgets.js source into a VM sandbox so that
// helper implementations are tested directly from production code.

const { readFileSync } = require("fs");
const path = require("path");
const vm = require("vm");

// Load Handlebars via vm so the UMD picks up our fake environment
const handlebarsSource = readFileSync(
  path.join(__dirname, "../static/js/vendor/handlebars.min.js"),
  "utf-8"
);

const hbsSandbox = { module: { exports: {} }, exports: {}, global: {}, self: {} };
vm.runInNewContext(handlebarsSource, hbsSandbox, { filename: "handlebars.min.js" });
const Handlebars = hbsSandbox.module.exports;

// Load widgets.js into a sandbox with browser-like stubs.
// The file uses `window`, `document`, ES module `export`, and `Map`.
const widgetsSource = readFileSync(
  path.join(__dirname, "../static/js/widgets.js"),
  "utf-8"
);

const widgetsSandbox = {
  window: {
    Handlebars,
    echarts: null,
    DOMPurify: {
      sanitize: (html, opts) => {
        // Simple mock: strip <script> tags, preserve other attributes
        return html.replace(/<script\b[^<]*(?:(?!<\/script>)<[^<]*)*<\/script>/gi, "");
      },
    },
  },
  document: { createElement: () => ({}), documentElement: { getAttribute: () => "dark" } },
  Map,
  Number,
  Math,
  Array,
  String,
  Object,
  RegExp,
  JSON,
  console,
  // export keyword stub — widgets.js uses `export const ...` and `export function ...`
};

// Strip ES module export syntax so it can run as a script in vm
const strippedSource = widgetsSource
  .replace(/^export\s+/gm, "")
  .replace(/^import\s+.*$/gm, "");

vm.runInNewContext(strippedSource, widgetsSandbox, { filename: "widgets.js" });

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
  const tpl = Handlebars.compile('{{link url "Click here"}}');
  const result = tpl({ url: "https://example.com" });
  assertEqual(
    result,
    '<a href="https://example.com" target="_blank" rel="noopener noreferrer">Click here</a>',
    "link with url and text"
  );
}

{
  const tpl = Handlebars.compile("{{link url}}");
  const result = tpl({ url: "https://example.com" });
  assertEqual(
    result,
    '<a href="https://example.com" target="_blank" rel="noopener noreferrer">https://example.com</a>',
    "link with url only (text defaults to url)"
  );
}

{
  const tpl = Handlebars.compile('{{link url "<script>alert(1)</script>"}}');
  const result = tpl({ url: "https://example.com" });
  assert(
    result.includes("&lt;script&gt;") && !result.includes("<script>"),
    "link escapes HTML in label text"
  );
}

{
  const tpl = Handlebars.compile('{{link url "test"}}');
  const result = tpl({ url: 'https://example.com/path?a=1&b="quoted"' });
  assert(
    result.includes("&amp;") && result.includes("&quot;"),
    "link escapes special characters in href"
  );
}

{
  const tpl = Handlebars.compile('{{link url "evil"}}');
  const result = tpl({ url: "javascript:alert(1)" });
  assert(
    !result.includes("href") && result.includes("evil"),
    "link rejects javascript: URLs and renders plain text"
  );
}

{
  const tpl = Handlebars.compile('{{link url "relative"}}');
  const result = tpl({ url: "/dashboard/page" });
  assert(
    result.includes('href="/dashboard/page"'),
    "link allows relative URLs"
  );
}

{
  const tpl = Handlebars.compile('{{link url "mail"}}');
  const result = tpl({ url: "mailto:user@example.com" });
  assert(
    result.includes("href=") && result.includes("mailto:"),
    "link allows mailto: URLs"
  );
}

console.log("\n=== sortBy helper ===");

{
  const tpl = Handlebars.compile("{{#sortBy rows \"value\"}}{{this.name}}:{{this.value}} {{/sortBy}}");
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
  const tpl = Handlebars.compile('{{#sortBy rows "value" "desc"}}{{this.name}}:{{this.value}} {{/sortBy}}');
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
  const tpl = Handlebars.compile("{{#sortBy rows \"name\"}}{{this.name}} {{/sortBy}}");
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
  const tpl = Handlebars.compile("{{#sortBy rows \"value\"}}{{@index}} {{/sortBy}}");
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
  const tpl = Handlebars.compile("{{#sortBy rows \"value\"}}{{/sortBy}}");
  const result = tpl({ rows: [] });
  assertEqual(result, "", "sortBy handles empty rows");
}

console.log("\n=== html helper ===");

{
  const tpl = Handlebars.compile("{{html content}}");
  const result = tpl({ content: "<strong>Bold</strong>" });
  assertEqual(result, "<strong>Bold</strong>", "html helper outputs raw HTML");
}

{
  const tpl = Handlebars.compile("{{content}}");
  const result = tpl({ content: "<strong>Bold</strong>" });
  assertEqual(
    result,
    "&lt;strong&gt;Bold&lt;/strong&gt;",
    "without html helper, content is escaped"
  );
}

{
  const tpl = Handlebars.compile("{{html content}}");
  const result = tpl({ content: null });
  assertEqual(result, "", "html helper handles null gracefully");
}

{
  const tpl = Handlebars.compile("{{html content}}");
  const result = tpl({});
  assertEqual(result, "", "html helper handles undefined gracefully");
}

console.log("\n=== groupBy helper ===");

{
  const tpl = Handlebars.compile('{{#groupBy rows "category"}}[{{@key}}:{{#each this}}{{this.name}}{{/each}}]{{/groupBy}}');
  const result = tpl({
    rows: [
      { name: "A", category: "fruit" },
      { name: "B", category: "veggie" },
      { name: "C", category: "fruit" },
    ],
  });
  assertEqual(result, "[fruit:AC][veggie:B]", "groupBy groups rows by string field");
}

{
  const tpl = Handlebars.compile('{{#groupBy rows "category"}}{{@key}} {{/groupBy}}');
  const result = tpl({
    rows: [
      { name: "C", category: "zebra" },
      { name: "A", category: "apple" },
      { name: "B", category: "mango" },
    ],
  });
  assertEqual(result, "apple mango zebra ", "groupBy sorts groups alphabetically by @key");
}

{
  const tpl = Handlebars.compile('{{#groupBy rows "category"}}{{#each this}}{{this.name}}{{/each}}{{/groupBy}}');
  const result = tpl({
    rows: [
      { name: "first", category: "x" },
      { name: "second", category: "x" },
      { name: "third", category: "x" },
    ],
  });
  assertEqual(result, "firstsecondthird", "groupBy preserves row order within each group");
}

{
  const tpl = Handlebars.compile('{{#groupBy rows "category"}}x{{/groupBy}}');
  const result = tpl({ rows: [] });
  assertEqual(result, "", "groupBy handles empty rows");
}

{
  const tpl = Handlebars.compile('{{#groupBy rows "category"}}[{{@key}}:{{this.length}}]{{/groupBy}}');
  const result = tpl({
    rows: [
      { name: "A", category: null },
      { name: "B" },
      { name: "C", category: "valid" },
    ],
  });
  assertEqual(result, "[:2][valid:1]", "groupBy groups null/missing field values under empty string key");
}

{
  // Verify @key is correct inside nested blocks
  const tpl = Handlebars.compile('{{#groupBy rows "type"}}{{@key}}={{this.length}}|{{/groupBy}}');
  const result = tpl({
    rows: [
      { type: "bug" },
      { type: "feature" },
      { type: "bug" },
      { type: "feature" },
      { type: "feature" },
    ],
  });
  assertEqual(result, "bug=2|feature=3|", "groupBy @key contains the group value with correct counts");
}

console.log("\n=== replace helper ===");

{
  const tpl = Handlebars.compile('{{replace val "foo" "bar"}}');
  const result = tpl({ val: "foo and foo again" });
  assertEqual(result, "bar and bar again", "replace replaces all occurrences");
}

{
  const tpl = Handlebars.compile('{{replace val "xyz" "bar"}}');
  const result = tpl({ val: "hello world" });
  assertEqual(result, "hello world", "replace returns original if search not found");
}

{
  const tpl = Handlebars.compile('{{replace val "https://api.github.com/repos/" ""}}');
  const result = tpl({ val: "https://api.github.com/repos/drasi-project/drasi-core" });
  assertEqual(result, "drasi-project/drasi-core", "replace handles empty replacement (deletion)");
}

{
  const tpl = Handlebars.compile('{{replace val "x" "y"}}');
  const result = tpl({ val: 12345 });
  assertEqual(result, "12345", "replace handles non-string input gracefully");
}

console.log("\n=== trimPrefix helper ===");

{
  const tpl = Handlebars.compile('{{trimPrefix val "https://api.github.com/repos/"}}');
  const result = tpl({ val: "https://api.github.com/repos/drasi-project/drasi-core" });
  assertEqual(result, "drasi-project/drasi-core", "trimPrefix removes prefix when present");
}

{
  const tpl = Handlebars.compile('{{trimPrefix val "xyz"}}');
  const result = tpl({ val: "hello world" });
  assertEqual(result, "hello world", "trimPrefix returns unchanged string when prefix not present");
}

{
  const tpl = Handlebars.compile('{{trimPrefix val "prefix"}}');
  const result = tpl({ val: null });
  assertEqual(result, "", "trimPrefix handles null input");
}

{
  const tpl = Handlebars.compile('{{trimPrefix val "prefix"}}');
  const result = tpl({});
  assertEqual(result, "", "trimPrefix handles undefined input");
}

// ─── sortBy: non-array input ─────────────────────────────────────────

{
  const tpl = Handlebars.compile('{{#sortBy rows "name"}}{{this.name}} {{/sortBy}}');
  const result = tpl({ rows: null });
  assertEqual(result, "", "sortBy handles null rows");
}

{
  const tpl = Handlebars.compile('{{#sortBy rows "name"}}{{this.name}} {{/sortBy}}');
  const result = tpl({ rows: undefined });
  assertEqual(result, "", "sortBy handles undefined rows");
}

{
  const tpl = Handlebars.compile('{{#sortBy rows "name"}}{{this.name}} {{/sortBy}}');
  const result = tpl({ rows: { not: "an array" } });
  assertEqual(result, "", "sortBy handles object (non-array) rows");
}

// ─── sortBy: SafeString (no double-escaping) ─────────────────────────

{
  const tpl = Handlebars.compile('{{#sortBy rows "url"}}{{link this.url "Go"}} {{/sortBy}}');
  const result = tpl({ rows: [{ url: "https://example.com" }] });
  assert(result.includes("<a "), "sortBy does not double-escape inner HTML from link helper");
  assert(!result.includes("&lt;a"), "sortBy must not HTML-escape link helper output");
}

// ─── html helper: sanitization ───────────────────────────────────────

{
  // With DOMPurify available — should sanitize
  const tpl = Handlebars.compile("{{{html content}}}");
  const result = tpl({ content: '<b>safe</b><script>alert("xss")</script>' });
  assert(result.includes("<b>safe</b>"), "html helper preserves safe tags");
  assert(!result.includes("<script>"), "html helper strips script tags via DOMPurify");
}

{
  // Test link output survives DOMPurify ADD_ATTR (target, rel preserved)
  const tpl = Handlebars.compile("{{{html content}}}");
  const result = tpl({ content: '<a href="https://x.com" target="_blank" rel="noopener noreferrer">X</a>' });
  assert(result.includes('target="_blank"'), "html helper preserves target attribute via ADD_ATTR");
  assert(result.includes('rel="noopener noreferrer"'), "html helper preserves rel attribute via ADD_ATTR");
}

// ─── compareByField utility ──────────────────────────────────────────

{
  const a = { val: 10 }, b = { val: 3 };
  const cmp = widgetsSandbox.compareByField(a, b, "val", "asc");
  assert(cmp > 0, "compareByField: 10 > 3 ascending");
}

{
  const a = { val: 10 }, b = { val: 3 };
  const cmp = widgetsSandbox.compareByField(a, b, "val", "desc");
  assert(cmp < 0, "compareByField: 10 > 3 descending reverses");
}

{
  const a = { name: "banana" }, b = { name: "apple" };
  const cmp = widgetsSandbox.compareByField(a, b, "name", "asc");
  assert(cmp > 0, "compareByField: string comparison ascending");
}

{
  const a = { val: null }, b = { val: "hello" };
  const cmp = widgetsSandbox.compareByField(a, b, "val", "asc");
  assert(typeof cmp === "number", "compareByField: handles null values without throwing");
}

// ─── Summary ────────────────────────────────────────────────────────
console.log(`\n${passed + failed} tests, ${passed} passed, ${failed} failed\n`);
process.exit(failed > 0 ? 1 : 0);
