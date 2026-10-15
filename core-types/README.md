# Shared Drasi data types

This package owns graph elements, source changes, query values, projected result
changes and storage-survival descriptions. It does not evaluate queries, parse
query languages, run components or open storage.

The existing implementations are shared rather than copied. `drasi-core` exposes
the same types through its existing model and value paths, while native component
contracts can use them without depending on the query engine. The query AST
dependency supplies data definitions for expression-valued results, not a parser
or evaluator.

Serialization, numeric grouping equality and hashing behavior are unchanged.
Engine-dependent value tests remain in `drasi-core`; run those together with
`cargo test -p drasi-core-types` when changing these definitions.
