// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const template = fs.readFileSync(path.join(__dirname, "report.html"), "utf8");
const start = template.indexOf("function node(");
const end = template.indexOf("function match(");
assert.ok(start >= 0 && end > start, "report link helpers must be present");
const { source } = vm.runInNewContext(
  `${template.slice(start, end)}; ({ source });`,
  { document: { createElement: tag => ({ tagName: tag }) } },
);

test("source links preserve normal repository paths and line numbers", () => {
  const link = source("lib/src/computation/v1/graph.rs", 73);
  assert.equal(link.tagName, "a");
  assert.equal(link.textContent, "lib/src/computation/v1/graph.rs:73");
  assert.equal(link.href, "../../lib/src/computation/v1/graph.rs#L73");
});

test("report text cannot become markup, a URL scheme or another origin", () => {
  for (const file of [
    'lib/src/<img src=x onerror="alert(1)">.rs',
    "javascript:alert(1)",
    "//attacker.example/path",
    'lib/src/name"#?&.rs',
    "lib\\src\\name.rs",
  ]) {
    const line = '1"><svg onload="alert(1)">';
    const link = source(file, line);
    assert.equal(link.textContent, `${file}:${line}`);
    assert.equal(
      link.href,
      `../../${file.split("/").map(encodeURIComponent).join("/")}#L${encodeURIComponent(line)}`,
    );
    assert.ok(!/[<>"\\]/.test(link.href), link.href);
    const url = new URL(link.href, "https://docs.example/lib/docs/report.html");
    assert.equal(url.protocol, "https:");
    assert.equal(url.origin, "https://docs.example");
    assert.equal(url.search, "");
  }
});
