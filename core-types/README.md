# Shared Drasi data types

This package owns graph elements, source changes, query values, projected result
changes, storage-survival descriptions and indexed-state contracts. It does not
evaluate queries, parse query languages, run components or open storage.

`ComponentId` and `ComponentName` are defined together in `component_identity`
and re-exported at the crate root. Component owners and selectors carry these
types, not text: UUID parsing and name validation occur at input boundaries.
Names are case-sensitive, reject empty/whitespace/control characters, and never
implicitly select an ID. The contracts, engine and runtime share these exact
types.

The optional `openapi` feature exposes their canonical string schemas: UUID
format for `ComponentId`, and the same nonempty/name constraints used by
`ComponentName` validation. Storage APIs use `StorageScope`, `StorePartition`,
`OutputPartition`, and `CheckpointKey` to keep ownership separate from physical
keys, data namespaces, and framework metadata.

The existing implementations are shared rather than copied. `drasi-core` exposes
the same types through its existing model and value paths, while native component
contracts can use them without depending on the query engine. The query AST
dependency supplies data definitions for expression-valued results, not a parser
or evaluator.

`data_value::DataValue` is the one lossless, serializable form of every
`VariableValue`, including expression, list-range and awaiting values. Use
`DataValue::encode(&value)` and `DataValue::decode()` (or `TryFrom`), or
`encode_variables` / `decode_variables` for named maps. Integers keep their sign,
temporals keep offsets and timezone names, and floats are stored as raw IEEE-754
bits so signed zero, infinities and NaN payloads are preserved. Invalid decoded
values return `DataValueError`. Graph element and query publishers share this
codec rather than copying it.

## Element property values

`ElementValue` keeps common primitives compact (null, booleans, `i64`, floats,
strings, lists, objects, local datetimes and fixed-offset datetimes). Anything a
primitive cannot hold without loss is stored as `ElementValue::Typed`, a boxed
`TypedElementValue` that wraps a validated `DataValue`:

- integers above `i64::MAX`, and rich query values such as dates, times,
  durations, ranges, element references, expressions and `Awaiting`;
- zoned datetimes that carry an IANA timezone name (those without a name stay
  `ElementValue::ZonedDateTime`);
- nested values, which convert recursively without defaulting.

Converting a `VariableValue` to an `ElementValue` no longer replaces
out-of-range integers or non-finite floats with defaults. `TypedElementValue`
can only be built from a payload that decodes to a valid query value, and
deserialization applies the same check. Use `TypedElementValue::try_from` for a
`DataValue` and `as_data_value()` to read it back.

Floats in `ElementValue` keep their exact bits. Binary serialization is unchanged
for finite and non-finite values; human-readable serialization writes a float as
`{"bits": "<16 hex digits>"}` and still reads a plain number.

The JSON projection (`From<&ElementValue> for serde_json::Value`) keeps plain JSON
for primitives and finite floats, writes datetimes as the existing datetime
envelope, and writes typed values and non-finite floats as
`{"__drasi_v1_envelope__": "drasi.data-value", "value": <DataValue>}`.
`From<&serde_json::Value>` restores datetime envelopes and reads unsigned integers
above `i64::MAX` as typed values, but treats a data-value envelope as an ordinary
object. `ElementValue::try_from_data_json` restores typed values and returns an
error for a malformed or invalid envelope. `typespec/core-types.tsp` documents
these shapes and must be kept in sync by hand.

## Scope

Element and archive indexes, result indexes and accumulator values, future
queues, checkpoints, outbox/live-result writers and session interfaces are
available here without the evaluator. `ResultKey` retains the evaluator's exact
numeric grouping equality and hashing rules; `DataValue` equality and hashing
are separate and bitwise. Element-index join configuration carries the join
definitions and matching slot numbers; predicates and path topology remain with
the evaluator. These interfaces do not provide a native
indexed-storage service or prove shared transaction ownership.

Engine-dependent value tests remain in `drasi-core`; run those together with
`cargo test -p drasi-core-types` when changing these definitions.
