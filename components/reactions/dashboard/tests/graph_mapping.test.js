// Tests for rowsToGraph in widgets.js.
// Run: node tests/graph_mapping.test.js

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

const strippedSource = widgetsSource
  .replace(/^export\s+\{[^}]*\};?\s*$/gm, "")
  .replace(/^export\s+/gm, "")
  .replace(/^import\s+.*$/gm, "");

vm.runInNewContext(strippedSource, widgetsSandbox, { filename: "widgets.js" });

const rowsToGraph = widgetsSandbox.rowsToGraph;
const layoutGraphNodes = widgetsSandbox.layoutGraphNodes;
const asCoord = widgetsSandbox.asCoord;

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

function assertDeepEqual(actual, expected, message) {
  if (JSON.stringify(actual) === JSON.stringify(expected)) {
    passed++;
    console.log(`  ✓ ${message}`);
  } else {
    failed++;
    console.error(`  ✗ ${message}`);
    console.error(`    expected: ${JSON.stringify(expected)}`);
    console.error(`    actual:   ${JSON.stringify(actual)}`);
  }
}

console.log("\n=== rowsToGraph: node inference ===");

{
  const graph = rowsToGraph(
    [
      { source: "a", target: "b" },
      { source: "b", target: "c" },
    ],
    { sourceField: "source", targetField: "target" },
  );
  assertEqual(graph.nodes.length, 3, "infers three unique nodes from two edges");
  assertDeepEqual(
    graph.nodes.map((n) => n.id).sort(),
    ["a", "b", "c"],
    "node ids are source/target values",
  );
  assertEqual(graph.links.length, 2, "one link per row");
  assertEqual(graph.links[0].source, "a", "first link source");
  assertEqual(graph.links[0].target, "b", "first link target");
}

{
  const graph = rowsToGraph(
    [{ from: "n1", to: "n2", fromName: "Alpha", toName: "Beta", weight: 12 }],
    {
      sourceField: "from",
      targetField: "to",
      sourceLabelField: "fromName",
      targetLabelField: "toName",
      valueField: "weight",
    },
  );
  const byId = Object.fromEntries(graph.nodes.map((n) => [n.id, n]));
  assertEqual(byId.n1.name, "Alpha", "source label field is applied");
  assertEqual(byId.n2.name, "Beta", "target label field is applied");
  assertEqual(byId.n1.value, 12, "value field is applied to nodes");
  assertEqual(graph.links[0].value, 12, "value field is applied to links");
}

{
  const graph = rowsToGraph(
    [
      { source: "a", target: "b", srcCat: "hot", tgtCat: "cold" },
      { source: "b", target: "c", srcCat: "cold", tgtCat: "warm" },
    ],
    {
      sourceField: "source",
      targetField: "target",
      sourceCategoryField: "srcCat",
      targetCategoryField: "tgtCat",
    },
  );
  assertDeepEqual(
    graph.categories.map((c) => c.name).sort(),
    ["cold", "hot", "warm"],
    "categories collected from both endpoints",
  );
  const byId = Object.fromEntries(graph.nodes.map((n) => [n.id, n]));
  assertEqual(graph.categories[byId.a.category].name, "hot", "node a category");
  assertEqual(graph.categories[byId.c.category].name, "warm", "node c category");
}

{
  const graph = rowsToGraph(
    [{ source: "a", target: "b", kind: "CONNECTED_TO" }],
    { sourceField: "source", targetField: "target", edgeLabelField: "kind" },
  );
  assertEqual(graph.links[0].label.formatter, "CONNECTED_TO", "edge label is set");
}

{
  const graph = rowsToGraph(
    [
      { source: "a", target: "b" },
      { source: "", target: "c" },
      { source: "d", target: null },
    ],
    { sourceField: "source", targetField: "target" },
  );
  assertEqual(graph.links.length, 1, "skips rows missing an endpoint");
  assertEqual(graph.nodes.length, 2, "does not create nodes for skipped rows");
}

{
  const graph = rowsToGraph(
    [
      { source: "a", target: "b" },
      { source: "a", target: "c" },
    ],
    { sourceField: "source", targetField: "target" },
  );
  assertEqual(graph.nodes.length, 3, "shared source node is not duplicated");
}

{
  const graph = rowsToGraph([], { sourceField: "source", targetField: "target" });
  assertEqual(graph.nodes.length, 0, "empty rows produce no nodes");
  assertEqual(graph.links.length, 0, "empty rows produce no links");
}

console.log("\n=== layoutGraphNodes: stability ===");

{
  const nodes = [{ id: "a" }, { id: "b" }, { id: "c" }];
  const links = [{ source: "a", target: "b" }, { source: "b", target: "c" }];
  const first = layoutGraphNodes(nodes, links, 800, 400, new Map(), "circular");
  const second = layoutGraphNodes(nodes, links, 800, 400, first, "circular");
  assertEqual(first.get("a").x, second.get("a").x, "existing node x is unchanged on update");
  assertEqual(first.get("a").y, second.get("a").y, "existing node y is unchanged on update");
  assertEqual(first.get("c").x, second.get("c").x, "all existing node x values stay put");
}

{
  const nodes = [{ id: "a" }, { id: "b" }];
  const first = layoutGraphNodes(nodes, [{ source: "a", target: "b" }], 800, 400, new Map(), "circular");
  const grown = layoutGraphNodes(
    [...nodes, { id: "c" }],
    [{ source: "a", target: "b" }, { source: "b", target: "c" }],
    800,
    400,
    first,
    "circular",
  );
  assertEqual(first.get("a").x, grown.get("a").x, "adding a node does not move existing x");
  assertEqual(first.get("b").y, grown.get("b").y, "adding a node does not move existing y");
  assertEqual(grown.has("c"), true, "new node receives a position");
}

{
  const nodes = [{ id: "n1" }, { id: "n2" }, { id: "n3" }, { id: "n4" }];
  const links = [
    { source: "n1", target: "n2" },
    { source: "n2", target: "n3" },
    { source: "n3", target: "n4" },
    { source: "n4", target: "n1" },
  ];
  const pos = layoutGraphNodes(nodes, links, 900, 500, new Map(), "force");
  let minY = Infinity;
  let maxY = -Infinity;
  for (const p of pos.values()) {
    minY = Math.min(minY, p.y);
    maxY = Math.max(maxY, p.y);
  }
  assertEqual(maxY - minY > 80, true, "force layout spreads nodes vertically across the canvas");
}

{
  const nodes = Array.from({ length: 10 }, (_, i) => ({ id: `sensor_${i}` }));
  const links = nodes.map((n, i) => ({ source: n.id, target: `sensor_${(i + 1) % 10}` }));
  const pos = layoutGraphNodes(nodes, links, 1044, 277, new Map(), "force");
  let minY = Infinity;
  let maxY = -Infinity;
  let finite = 0;
  for (const p of pos.values()) {
    if (Number.isFinite(p.x) && Number.isFinite(p.y)) finite++;
    minY = Math.min(minY, p.y);
    maxY = Math.max(maxY, p.y);
  }
  assertEqual(finite, 10, "wide short canvas keeps finite coordinates");
  assertEqual(maxY - minY > 80, true, "wide short canvas still spreads nodes vertically");
}

console.log("\n=== asCoord: ignore ECharts nulls ===");

{
  assertEqual(asCoord(null), null, "null is not a coordinate");
  assertEqual(asCoord(undefined), null, "undefined is not a coordinate");
  assertEqual(asCoord(""), null, "empty string is not a coordinate");
  assertEqual(asCoord(0), 0, "zero is a valid coordinate");
  assertEqual(asCoord([142.5]), 142.5, "unwraps array coords from getOption");
}

{
  const prev = new Map([
    ["a", { x: null, y: null }],
    ["b", { x: null, y: null }],
  ]);
  const pos = layoutGraphNodes(
    [{ id: "a" }, { id: "b" }],
    [{ source: "a", target: "b" }],
    800,
    400,
    prev,
    "circular",
  );
  assertEqual(Number.isFinite(pos.get("a").x), true, "null prev x is treated as missing");
  assertEqual(Number.isFinite(pos.get("a").y), true, "null prev y is treated as missing");
  assertEqual(
    pos.get("a").x === pos.get("b").x && pos.get("a").y === pos.get("b").y,
    false,
    "null prev does not stack nodes at the origin",
  );
}

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
