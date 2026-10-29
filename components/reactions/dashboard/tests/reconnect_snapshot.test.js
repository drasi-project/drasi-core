// Run: node tests/reconnect_snapshot.test.js
// Exercise the production socket and designer without a browser dependency.
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const timers = new Map();
let nextTimer = 0;
const sockets = [];
class FakeWebSocket {
  static OPEN = 1;
  constructor() {
    this.readyState = 0;
    this.sent = [];
    sockets.push(this);
  }
  send(text) { this.sent.push(JSON.parse(text)); }
  close() { this.readyState = 3; this.onclose?.(); }
  open() { this.readyState = 1; this.onopen(); }
  message(payload) { this.onmessage({ data: JSON.stringify(payload) }); }
}

const sandbox = {
  console, structuredClone, WebSocket: FakeWebSocket,
  document: {
    getElementById: () => ({}),
    documentElement: { getAttribute: () => "dark" },
  },
  window: {
    location: { protocol: "http:", host: "localhost" },
    Handlebars: { registerHelper: () => {} },
    setTimeout: (fn) => { const id = ++nextTimer; timers.set(id, fn); return id; },
    clearTimeout: (id) => timers.delete(id),
  },
  setTimeout: () => {},
};
vm.createContext(sandbox);
for (const file of ["widgets.js", "dashboard.js", "websocket.js"]) {
  const source = readFileSync(path.join(__dirname, "../static/js", file), "utf8")
    .replace(/^import\s+[\s\S]*?\s+from\s+"[^"]+";\s*$/gm, "")
    .replace(/^export\s+\{[^}]*\};?\s*$/gm, "")
    .replace(/^export\s+/gm, "");
  vm.runInContext(source, sandbox, { filename: file });
}
const designer = vm.runInContext("new DashboardDesigner('grid')", sandbox);
designer.dashboard = { widgets: [
  { id: "one", config: { queryId: "q" } },
  { id: "two", config: { queryId: "q" } },
  { id: "other", config: { queryId: "other" } },
] };
designer.renderWidget = () => {};
const changes = [];
designer.onChanged((_dashboard, refreshQueries) => changes.push(refreshQueries));
designer.emitChanged();
designer.emitChanged(["q"]);
assert.deepEqual(JSON.parse(JSON.stringify(changes)), [[], ["q"]],
  "layout/name changes must not reset widget histories with unnecessary snapshots");
const socket = vm.runInContext("new DashboardSocket()", sandbox);
const statuses = [];
socket.onStatus((status) => statuses.push(status));
socket.onQuerySnapshot((message) => designer.handleQuerySnapshot(message));
socket.onQueryResult((message) => designer.handleQueryResult(message));
const snapshot = (rows) => ({
  type: "query_snapshot", query_id: "q", revision: "12345678901234567890", rows,
});
const delta = (results) => ({
  type: "query_result", query_id: "q", revision: "12345678901234567891", results,
});
const rows = (id = "one") => JSON.parse(JSON.stringify(designer.runtimeByWidgetId.get(id)?.rows));

socket.subscribe(["q"]);
socket.start();
const first = sockets[0];
first.open();
assert.equal(statuses.at(-1), "Synchronizing");
assert.deepEqual(first.sent[0], { type: "subscribe", query_ids: ["q"] });
first.message(snapshot([{ k: "99", v: { state: "old" } }]));
assert.equal(statuses.at(-1), "Connected");
assert.deepEqual(rows(), [{ state: "old" }]);
assert.deepEqual(rows("two"), [{ state: "old" }]);

first.close();
assert.equal(statuses.at(-1), "Disconnected");
const reconnect = [...timers.values()][0];
reconnect();
const second = sockets[1];
second.open();
assert.equal(statuses.at(-1), "Synchronizing");
second.message(snapshot([]));
assert.deepEqual(rows(), []);
assert.deepEqual(rows("two"), []);
second.message(delta([{ op: "add", k: "99", data: { state: "fresh" } }]));
assert.deepEqual(rows(), [{ state: "fresh" }]);
assert.deepEqual(rows("two"), [{ state: "fresh" }]);
assert.equal(designer.runtimeByWidgetId.has("other"), false);

// An old connection cannot replace the new snapshot or apply delayed deltas.
first.message(snapshot([{ k: "99", v: { state: "stale" } }]));
first.message(delta([{ op: "delete", k: "99", data: { state: "fresh" } }]));
first.onopen();
first.onerror();
first.onclose();
assert.deepEqual(rows(), [{ state: "fresh" }]);
assert.equal(statuses.at(-1), "Connected");
assert.equal(sockets.length, 2);

// Rebinding an existing query asks for a new authoritative snapshot too.
socket.subscribe(["q"]);
assert.equal(statuses.at(-1), "Synchronizing");
second.message(snapshot([{ k: "100", v: { state: "rebound" } }]));
assert.deepEqual(rows(), [{ state: "rebound" }]);
second.message(delta([{ op: "delete", k: "100", data: { state: "rebound" } }]));
assert.deepEqual(rows(), []);

second.close();
const pendingReconnect = [...timers.values()].at(-1);
socket.stop();
pendingReconnect();
assert.equal(sockets.length, 2);
socket.start();
assert.equal(sockets.length, 3);
pendingReconnect();
reconnect();
assert.equal(sockets.length, 3, "old timers must not replace a newer connection");
socket.stop();
console.log("PASS: snapshot replacement/empty clear, shared widgets, live ordering, reconnect and stale callback fences");
