// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const html = fs.readFileSync(path.join(__dirname, "src/index.html"), "utf8");
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const withoutPolling = script.replace(/\n  poll\(\);\s*$/, "\n");
assert.notEqual(withoutPolling, script, "Renderer tests must disable automatic polling");
const renderer = vm.createContext({
    URL,
    document: {
        getElementById: () => ({ addEventListener() {} }),
        querySelectorAll: () => [],
    },
});
vm.runInContext(withoutPolling, renderer);

test("component integer, float, boolean, and string fields are decoded", () => {
    assert.equal(renderer.logAttributeValue({ I64: 516 }), 516);
    assert.equal(renderer.logAttributeValue({ F64: 0.125 }), 0.125);
    assert.equal(renderer.logAttributeValue({ Bool: false }), false);
    assert.equal(renderer.logAttributeValue({ String: "ready" }), "ready");
});

test("handling fields are meaningful without exposing transport types", () => {
    assert.equal(renderer.formatLogField("result_changes", { I64: 2 }), "2 result changes handled");
    assert.equal(renderer.formatLogField("result_changes", { I64: 1 }), "1 result change handled");
    assert.equal(renderer.formatLogField("updated", { I64: 1 }), "1 updated");
    assert.equal(renderer.formatLogField("query_id", { String: "http-query" }), "Query: http-query");
    assert.equal(renderer.formatLogField("retry_enabled", { Bool: false }), "retry enabled: false");
    assert.equal(renderer.formatLogField("outcome", { String: "success" }), "outcome: success");
});

test("malformed fields fail explicitly instead of displaying invented values", () => {
    for (const value of [null, [], 516, {}, { I64: "516" }, { Bool: 1 }, { Unknown: 1 }, { I64: 1, String: "one" }]) {
        assert.throws(() => renderer.logAttributeValue(value), /log attribute/i);
    }
});

test("primary metric display selects real framework measurements", () => {
    assert.equal(renderer.primaryMetric("drasi_query_events_processed{query_id=\"http-query\"} 1"), true);
    assert.equal(renderer.primaryMetric("# TYPE drasi_source_events_dispatched counter"), true);
    assert.equal(renderer.primaryMetric("drasi_component_inputs_processed{component_id=\"http-output\"} 1"), true);
    assert.equal(renderer.primaryMetric("drasi_queue_depth{queue_id=\"pipe\"} 0"), false);
    assert.equal(renderer.primaryMetric("unrelated_counter 0"), false);
});

test("timestamps preserve real dates and reject invalid input", () => {
    const time = renderer.formatLogTimestamp("2026-11-09T00:20:11.1003127Z");
    assert.ok(time.includes("2026"));
    assert.ok(!time.includes("Invalid Date"));
    assert.throws(() => renderer.formatLogTimestamp("not-a-date"), /timestamp/);
    assert.throws(() => renderer.formatLogTimestamp(null), /timestamp/);
});

test("trace links preserve a Jaeger path prefix without query or fragment", () => {
    const id = "a".repeat(32);
    assert.equal(renderer.traceUrl("http://127.0.0.1:16689/", id), `http://127.0.0.1:16689/trace/${id}`);
    assert.equal(renderer.traceUrl("https://example.test/jaeger/?old=1#search", id), `https://example.test/jaeger/trace/${id}`);
});

test("invalid trace links are rejected", () => {
    assert.throws(() => renderer.traceUrl("http://127.0.0.1:16689", "../other"), /trace ID/);
    assert.throws(() => renderer.traceUrl("javascript:alert(1)", "a".repeat(32)), /HTTP/);
});

test("the existing main demo shows the real transformer without another scenario", () => {
    assert.equal(
        renderer.pipelineTopology({ scenario: "follow", source: "http", transformer: true }),
        "HTTP → arithmetic transformer (value * 2) → continuous query → application reaction (handled callback)",
    );
    assert.equal(vm.runInContext("makeTargets.follow", renderer), "observability-demo");
    assert.equal(vm.runInContext("Object.hasOwn(makeTargets, 'transformer')", renderer), false);
    assert.match(vm.runInContext("stories.follow[1]", renderer), /preserves input trace context/);
    assert.equal(
        renderer.pipelineTopology({ scenario: "postgres", source: "postgres", transformer: false }),
        "PostgreSQL logical replication → continuous query → application reaction (handled callback)",
    );
});
