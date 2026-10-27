// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/**
 * Drasi Core Data Types
 *
 * This file documents the core data structures used across all Drasi sources.
 * These types are based on drasi_core_types::models and are used for representing
 * graph elements (nodes and relations) and their changes.
 *
 * Source: /core-types/src/models/ and /core-types/src/data_value.rs
 */

import "@typespec/json-schema";

using TypeSpec.JsonSchema;

namespace Drasi.Sources;

/**
 * A property value in the Drasi graph model, in its JSON projection
 * (`From<&ElementValue> for serde_json::Value`).
 *
 * Primitive JSON values stay plain JSON. Values that plain JSON cannot hold
 * without loss use a marked envelope object:
 * - local and zoned datetimes use `DateTimeEnvelope`;
 * - integers above `i64::MAX`, non-finite floats and every rich query value
 *   (dates, times, durations, ranges, element references, expressions, ...)
 *   use `TypedValueEnvelope`, which carries a `DataValue`.
 *
 * Plain JSON conversion (`From<&serde_json::Value>`) restores datetime envelopes
 * and unsigned integers above `i64::MAX`, but treats a typed-value envelope as an
 * ordinary object. `ElementValue::try_from_data_json` additionally restores typed
 * values and rejects malformed envelopes.
 *
 * Based on: core-types/src/models/element_value.rs
 */
@jsonSchema
union ElementValue {
  /** Null value */
  null: null,

  /** Boolean value */
  boolean: boolean,

  /** 64-bit signed integer */
  integer: int64,

  /** Finite 64-bit floating point number; non-finite values use `TypedValueEnvelope` */
  float: float64,

  /** UTF-8 string */
  string: string,

  /** Array of ElementValues (recursive) */
  list: ElementValue[],

  /** Map of string keys to ElementValues (recursive) */
  object: Record<ElementValue>,

  /** Local or zoned datetime */
  datetime: DateTimeEnvelope,

  /** Lossless typed value */
  typed: TypedValueEnvelope
}

/**
 * Envelope for a local or zoned datetime property.
 *
 * Zoned datetimes keep only their fixed offset; IANA timezone names need
 * `TypedValueEnvelope`. The envelope is an internal round-trip shape, not a
 * general external API.
 */
@jsonSchema
model DateTimeEnvelope {
  "__drasi_v1_envelope__": "drasi.element_value.datetime";
  "__drasi_v1_type__": "drasi.LocalDateTime" | "drasi.ZonedDateTime";

  /** `YYYY-MM-DD HH:MM:SS[.fraction]` for local datetimes, RFC 3339 for zoned datetimes */
  value: string;
}

/**
 * Envelope for a property stored as a typed value (`ElementValue::Typed`).
 *
 * The payload is validated on construction and deserialization: a payload that
 * cannot be decoded to a query value (for example an out-of-range UTC offset or
 * an unknown operator tag) is rejected.
 */
@jsonSchema
model TypedValueEnvelope {
  "__drasi_v1_envelope__": "drasi.data-value";
  value: DataValue;
}

/**
 * Lossless, serializable form of every query value (`drasi_core_types::data_value::DataValue`).
 *
 * The shapes below are the self-describing (JSON, externally tagged) form of the
 * Rust serde derives. Integers keep their sign, temporals keep offsets and
 * timezone names, and floats are raw IEEE-754 bits, so signed zero, infinities and
 * NaN payloads survive. Float payloads can exceed 2^53, so consumers must read
 * them as unsigned 64-bit integers. Equality and hashing of this form are bitwise.
 *
 * Based on: core-types/src/data_value.rs
 */
@jsonSchema
union DataValue {
  null: "Null",
  awaiting: "Awaiting",
  bool: { Bool: boolean },

  /** Negative integers only; non-negative integers use `Unsigned` */
  signed: { Signed: int64 },

  /** Non-negative integers up to `u64::MAX` */
  unsigned: { Unsigned: uint64 },

  /** Raw IEEE-754 bits of a 64-bit float */
  float: { Float: uint64 },

  string: { String: string },
  list: { List: DataValue[] },
  object: { Object: Record<DataValue> },

  /** `YYYY-MM-DD` */
  date: { Date: string },

  /** `HH:MM:SS[.fraction]` */
  localTime: { LocalTime: string },

  /** Local time and its offset in seconds east of UTC */
  zonedTime: { ZonedTime: [string, int32] },

  /** `YYYY-MM-DDTHH:MM:SS[.fraction]` */
  localDateTime: { LocalDateTime: string },

  /** RFC 3339 instant with fixed offset, and the optional original timezone name */
  zonedDateTime: { ZonedDateTime: [string, string | null] },

  /** `nanos` is the signed sub-second remainder, within +/-999,999,999 */
  duration: { Duration: { seconds: int64, nanos: int32, years: int64, months: int64 } },

  /** List range bounds; null is unbounded */
  range: { Range: [int64 | null, int64 | null] },

  element: { Element: SerializedElement },
  metadata: { Metadata: ElementMetadata },
  reference: { Reference: ElementReference },

  /** Expression-valued result */
  expression: { Expression: DataExpression },
}

/**
 * Data form of a query expression carried by an expression-valued result.
 *
 * Binary tags 0..=19 are And, Or, Eq, Ne, Lt, Le, Gt, Ge, In, Add, Subtract,
 * Multiply, Divide, Modulo, Exponent, HasLabel, Index, StartsWith, EndsWith,
 * Contains. The function tuple is name, arguments and position in the query; the
 * case tuple is match, when/then pairs and else; the iterator tuple is item
 * identifier, list, filter and map.
 */
@jsonSchema
union DataExpression {
  unary: { Unary: DataUnary },
  binary: { Binary: [uint8, DataExpression, DataExpression] },
  function: { Function: [string, DataExpression[], uint64] },
  `case`: { Case: [DataExpression | null, [DataExpression, DataExpression][], DataExpression | null] },
  list: { List: DataExpression[] },
  object: { Object: Record<DataExpression> },
  iterator: { Iterator: [string, DataExpression, DataExpression | null, DataExpression | null] },
}

/**
 * Data form of a unary query expression.
 *
 * Operator tags are 0 Not, 1 Exists, 2 IsNull, 3 IsNotNull.
 */
@jsonSchema
union DataUnary {
  operator: { Operator: [uint8, DataExpression] },
  literal: { Literal: DataLiteral },
  property: { Property: [string, string] },
  expressionProperty: { ExpressionProperty: [DataExpression, string] },
  parameter: { Parameter: string },
  identifier: { Identifier: string },
  variable: { Variable: [string, DataExpression] },
  `alias`: { Alias: [DataExpression, string] },
  range: { Range: [DataExpression | null, DataExpression | null] },
}

/** Data form of a query literal; temporal literals keep their source text. */
@jsonSchema
union DataLiteral {
  null: "Null",
  integer: { Integer: int64 },

  /** Raw IEEE-754 bits of the real literal */
  real: { Real: uint64 },

  boolean: { Boolean: boolean },
  text: { Text: string },
  date: { Date: string },
  localTime: { LocalTime: string },
  zonedTime: { ZonedTime: string },
  localDateTime: { LocalDateTime: string },
  zonedDateTime: { ZonedDateTime: string },
  duration: { Duration: string },
  object: { Object: [string, DataLiteral][] },
  expression: { Expression: DataExpression },
}

/**
 * Serde form of `ElementValue` as embedded in a `DataValue` element (binary
 * payloads use compact encodings; JSON is shown here). This differs from the
 * `ElementValue` JSON projection above: variants are externally tagged and a
 * float is `{ bits }`, the 16-digit lower-case hex image of its IEEE-754 bits
 * (a plain JSON number is also accepted when reading).
 */
@jsonSchema
union SerializedElementValue {
  null: "Null",
  bool: { Bool: boolean },
  float: { Float: { bits: string } },
  integer: { Integer: int64 },
  string: { String: string },
  list: { List: SerializedElementValue[] },
  object: { Object: SerializedPropertyMap },

  /** `YYYY-MM-DDTHH:MM:SS[.fraction]` */
  localDateTime: { LocalDateTime: string },

  /** RFC 3339 with fixed offset */
  zonedDateTime: { ZonedDateTime: string },

  /** Typed value, serialized as its `DataValue` */
  typed: { Typed: DataValue },
}

/** Serde form of `ElementPropertyMap`. */
@jsonSchema
model SerializedPropertyMap {
  values: Record<SerializedElementValue>;
}

/** Serde form of an element, externally tagged by `Node` or `Relation`. */
@jsonSchema
union SerializedElement {
  node: { Node: { metadata: ElementMetadata, properties: SerializedPropertyMap } },
  relation: {
    Relation: {
      metadata: ElementMetadata,
      in_node: ElementReference,
      out_node: ElementReference,
      properties: SerializedPropertyMap,
    },
  },
}

/**
 * A map of properties for a graph element.
 *
 * Property names are strings, and values are ElementValues.
 *
 * Based on: core-types/src/models/element_value.rs
 */
@jsonSchema
model ElementPropertyMap is Record<ElementValue>;

/**
 * Reference to a graph element by source and element ID.
 *
 * Uniquely identifies an element across all sources in the system.
 *
 * Based on: core-types/src/models/element.rs
 */
@jsonSchema
model ElementReference {
  /** ID of the source that owns this element */
  source_id: string;

  /** Unique ID of the element within the source */
  element_id: string;
}

/**
 * Timestamp type used for element effective times.
 *
 * Represents milliseconds since Unix epoch (u64 in Rust).
 *
 * Based on: core-types/src/models/element.rs
 */
@jsonSchema
scalar ElementTimestamp extends uint64;

/**
 * Metadata for a graph element.
 *
 * Contains identification, labels, and temporal information.
 *
 * Based on: core-types/src/models/element.rs
 */
@jsonSchema
model ElementMetadata {
  /** Reference to this element */
  reference: ElementReference;

  /** Labels/types assigned to this element (e.g., ["User", "Person"]) */
  labels: string[];

  /** Timestamp when this element version became effective (milliseconds) */
  effective_from: ElementTimestamp;
}

/**
 * A graph element - either a Node or a Relation.
 *
 * This is the fundamental unit of data in Drasi. Nodes represent entities,
 * and Relations represent connections between nodes.
 *
 * Based on: core-types/src/models/element.rs
 */
@jsonSchema
union Element {
  /** A node (vertex) in the graph */
  node: Node,

  /** A relation (edge) in the graph */
  relation: Relation
}

/**
 * A node in the graph.
 *
 * Represents an entity with properties.
 */
@jsonSchema
model Node {
  /** Discriminator field */
  type: "node";

  /** Node metadata */
  metadata: ElementMetadata;

  /** Node properties */
  properties: ElementPropertyMap;
}

/**
 * A relation (edge) in the graph.
 *
 * Represents a directed connection from one node to another.
 */
@jsonSchema
model Relation {
  /** Discriminator field */
  type: "relation";

  /** Relation metadata */
  metadata: ElementMetadata;

  /** Reference to the source node (outgoing) */
  out_node: ElementReference;

  /** Reference to the target node (incoming) */
  in_node: ElementReference;

  /** Relation properties */
  properties: ElementPropertyMap;
}

/**
 * Types of source change operations.
 *
 * Based on: core-types/src/models/source_change.rs
 */
@jsonSchema
enum SourceChangeOperation {
  /** Insert a new element */
  insert: "insert",

  /** Update an existing element's properties */
  update: "update",

  /** Replace an existing element in full */
  replace: "replace",

  /** Delete an element */
  delete: "delete",

  /** Future event (scheduled change) */
  future: "future"
}

/**
 * A change event from a source.
 *
 * Represents an insert, update, or delete operation on a graph element.
 *
 * Based on: core-types/src/models/source_change.rs
 */
@jsonSchema
union SourceChange {
  /** Insert a new element */
  insert: InsertChange,

  /** Update an existing element's properties */
  update: UpdateChange,

  /** Replace an existing element in full */
  replace: ReplaceChange,

  /** Delete an element */
  delete: DeleteChange
}

/**
 * Insert operation - adds a new element to the graph.
 */
@jsonSchema
model InsertChange {
  /** Discriminator field */
  operation: "insert";

  /** The element being inserted */
  element: Element;
}

/**
 * Update operation - patches an existing element.
 *
 * Every property of `element` is set (null is stored as a value). Only the names
 * in `removed_properties` are deleted; every other property stays unchanged. A
 * name may not be both set and removed. Without an existing element the update
 * creates it from the properties it sets.
 */
@jsonSchema
model UpdateChange {
  /** Discriminator field */
  operation: "update";

  /** The properties to set, with the element's metadata */
  element: Element;

  /** Names of the properties to delete */
  removed_properties: string[];
}

/**
 * Replace operation - replaces an existing element in full. Properties the new
 * element omits are dropped. Without an existing element it creates the element.
 */
@jsonSchema
model ReplaceChange {
  /** Discriminator field */
  operation: "replace";

  /** The complete new element */
  element: Element;
}

/**
 * Delete operation - removes an element from the graph.
 *
 * Only metadata is needed for deletion (no properties required).
 */
@jsonSchema
model DeleteChange {
  /** Discriminator field */
  operation: "delete";

  /** Metadata of the element being deleted */
  metadata: ElementMetadata;
}
