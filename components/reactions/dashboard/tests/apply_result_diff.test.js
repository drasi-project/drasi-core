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
  .replace(/^export\s+\{[^}]*\};?\s*$/gm, "")
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

console.log("\n=== applyResultDiff: row_signature keying ===");

{
  // add of an existing row_signature replaces, not duplicates (no id needed)
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", k: 7777, data: { location: "Parking" } });
  applyResultDiff(rt, { op: "add", k: 7777, data: { location: "Curbside" } });
  assertEqual(rt.rows.length, 1, "add with same row_signature replaces (no duplicate)");
  assertEqual(rt.rows[0].location, "Curbside", "replacement keeps newest value");
}

{
  // distinct row_signatures append
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", k: 1, data: { location: "Parking" } });
  applyResultDiff(rt, { op: "add", k: 2, data: { location: "Parking" } });
  assertEqual(rt.rows.length, 2, "add with distinct row_signatures appends");
}

{
  // update matches by row_signature even when before/after content differs
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", k: 42, data: { location: "Parking" } });
  applyResultDiff(rt, { op: "update", k: 42, before: { location: "Parking" }, after: { location: "Curbside" } });
  assertEqual(rt.rows.length, 1, "update matches by row_signature");
  assertEqual(rt.rows[0].location, "Curbside", "update applies new value");
}

{
  // seeded snapshot row (with k) then live add for same signature upserts (#605 plugin path)
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", k: 99, data: { location: "Parking" } }); // synthetic snapshot add
  applyResultDiff(rt, { op: "add", k: 99, data: { location: "Curbside" } }); // first live change as add
  assertEqual(rt.rows.length, 1, "seeded-then-add by signature upserts");
  assertEqual(rt.rows[0].location, "Curbside", "live add replaces seeded row");
}

{
  // delete by row_signature removes the row
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", k: 5, data: { location: "Parking" } });
  applyResultDiff(rt, { op: "delete", k: 5, data: { location: "Parking" } });
  assertEqual(rt.rows.length, 0, "delete by row_signature removes the row");
}

console.log("\n=== applyResultDiff: id/equality fallback (row_signature unknown) ===");

{
  // add of an existing id replaces when signature is 0/absent
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { id: "A1234", location: "Parking" } });
  applyResultDiff(rt, { op: "add", data: { id: "A1234", location: "Curbside" } });
  assertEqual(rt.rows.length, 1, "add with existing id replaces (no duplicate)");
  assertEqual(rt.rows[0].location, "Curbside", "replacement keeps newest value");
}

{
  // add of distinct ids appends
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { id: "A", v: 1 } });
  applyResultDiff(rt, { op: "add", data: { id: "B", v: 2 } });
  assertEqual(rt.rows.length, 2, "add with distinct ids appends");
}

{
  // rows without id or signature fall back to full-object equality
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", data: { name: "Alice" } });
  applyResultDiff(rt, { op: "add", data: { name: "Alice" } });
  assertEqual(rt.rows.length, 1, "add with identical keyless row upserts by equality");
}

{
  // signature takes precedence over id when both present
  const rt = newRuntime();
  applyResultDiff(rt, { op: "add", k: 1, data: { id: "X", v: 1 } });
  applyResultDiff(rt, { op: "add", k: 2, data: { id: "X", v: 2 } });
  assertEqual(rt.rows.length, 2, "different signatures with same id are distinct rows");
}

// ─── Summary ────────────────────────────────────────────────────────
console.log(`\n${passed + failed} tests, ${passed} passed, ${failed} failed\n`);
process.exit(failed > 0 ? 1 : 0);
