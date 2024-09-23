# Understanding the drasi-core repo code organization

This document describes the high-level organization of code for the `drasi-core` repository. The goal of this document is to capture most of the important details, not every single thing will be described here.

In general you should ask for guidance before creating a new top-level folder in the repo. There is usually a better place to put something.

## Root folders

| Folder     | Description                                                                           |
| ---------- | --------------------------------------------------------------------------------------|
| `core/`    | The main Drasi library that provides continuous queries that computes the diff of the query result when data changes |
| `docs/`    | The documentation for contributing |
| `examples/`| Basic usage examples of using Drasi as an embedded continuous query engine |
| `index-garnet/`  | A Garnet/Redis storage implementation for Drasi |
| `index-rocksdb/` | A RocksDb  storage implementation for Drasi |
| `middleware/` | Middleware implementations |
| `query-ast/` | The abstract syntax tree for continuous queries |
| `query-cypher/` | The Cypher parser, that produces the AST from a Cypher string |
| `query-perf/` | Performance testing harness |
| `shared-tests/` | Portable integration tests that can be run against multiple storage implementations |
