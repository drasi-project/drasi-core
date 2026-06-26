// Tests for applyResultDiff in widgets.js (issue #605).
// Run: node tests/apply_result_diff.test.js
//
// Loads the actual widgets.js source into a VM sandbox so the production
// implementation is tested directly. Focuses on upsert-on-add semantics:
// an `add` for a row whose key already exists must replace it, not duplicate.

const { readFileSync } = require("fs");
const path = require("path");
const vm = require("vm");

const widgetsSource = readFileSync(
  path.join(__dirname, "../static/js/widgets.js"),
  "utf-8"
);

const widgetsSandbox = {
  window: { Handlebars: { registerHelper: () => {} } },
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
};

// Strip ES module export/import syntax so it can run as a script in vm.
const strippedSource = widgetsSource
  .replace(/^export\s+/gm, "")
  .replace(/^import\s+.*$/gm, "");

vm.runInNewContext(strippedSource, widgetsSandbox, { filename: "widgets.js" });

const applyResultDiff = widgetsSandbox.applyResultDiff;

// ─── Test runner ────────────────────────────────────────────────────
let passed = 0;
let failed = 0;

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

function newRuntime() {
  return { rows: [], latest: null, aggregation: null };
}

// ─── Tests ──────────────────────────────────────────────────────────

console.log("\n=== applyResultDiff: upsert-on-add ===");

{
  // add of an existing id replaces, not duplicates
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { id: "A1234", location: "Parking" } });
  applyResultDiff(rt, { op: "add", data: { id: "A1234", location: "Curbside" } });
  assertEqual(rt.rows.length, 1, "add with existing id replaces (no duplicate)");
  assertEqual(rt.rows[0].location, "Curbside", "replacement keeps newest value");
  assertEqual(rt.latest.location, "Curbside", "latest tracks newest add");
}

{
  // add of distinct ids appends
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { id: "A", v: 1 } });
  applyResultDiff(rt, { op: "add", data: { id: "B", v: 2 } });
  assertEqual(rt.rows.length, 2, "add with distinct ids appends");
}

{
  // rows without id fall back to full-object equality
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { name: "Alice" } });
  applyResultDiff(rt, { op: "add", data: { name: "Alice" } });
  assertEqual(rt.rows.length, 1, "add with identical keyless row upserts by equality");
}

{
  // add after seed-equivalent state behaves like update for same key
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { id: "A1234", location: "Parking" } });
  applyResultDiff(rt, { op: "update", before: { id: "A1234", location: "Parking" }, after: { id: "A1234", location: "Curbside" } });
  assertEqual(rt.rows.length, 1, "update still matches by key after add");
  assertEqual(rt.rows[0].location, "Curbside", "update applies new value");
}

// ─── Summary ────────────────────────────────────────────────────────
console.log(`\n${passed + failed} tests, ${passed} passed, ${failed} failed\n`);
process.exit(failed > 0 ? 1 : 0);
