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
 * These types are based on drasi_core::models and are used for representing
 * graph elements (nodes and relations) and their changes.
 *
 * Source: /core/src/models/
 */

import "@typespec/json-schema";

using TypeSpec.JsonSchema;

namespace Drasi.Sources;

/**
 * A property value in the Drasi graph model.
 *
 * ElementValue is a recursive union type that can represent any JSON-like value
 * including primitives, arrays, and nested objects.
 *
 * Based on: core/src/models/element_value.rs (lines 29-39)
 */
@jsonSchema
union ElementValue {
  /** Null value */
  null: null,

  /** Boolean value */
  boolean: boolean,

  /** 64-bit signed integer */
  integer: int64,

  /** 64-bit floating point number */
  float: float64,

  /** UTF-8 string */
  string: string,

  /** Array of ElementValues (recursive) */
  list: ElementValue[],

  /** Map of string keys to ElementValues (recursive) */
  object: Record<ElementValue>
}

/**
 * A map of properties for a graph element.
 *
 * Property names are strings, and values are ElementValues.
 *
 * Based on: core/src/models/element_value.rs (lines 140-180)
 */
@jsonSchema
model ElementPropertyMap is Record<ElementValue>;

/**
 * Reference to a graph element by source and element ID.
 *
 * Uniquely identifies an element across all sources in the system.
 *
 * Based on: core/src/models/element.rs (lines 24-37)
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
 * Represents nanoseconds since Unix epoch (u64 in Rust).
 *
 * Based on: core/src/models/element.rs (line 45)
 */
@jsonSchema
scalar ElementTimestamp extends uint64;

/**
 * Metadata for a graph element.
 *
 * Contains identification, labels, and temporal information.
 *
 * Based on: core/src/models/element.rs (lines 47-52)
 */
@jsonSchema
model ElementMetadata {
  /** Reference to this element */
  reference: ElementReference;

  /** Labels/types assigned to this element (e.g., ["User", "Person"]) */
  labels: string[];

  /** Timestamp when this element version became effective (nanoseconds) */
  effective_from: ElementTimestamp;
}

/**
 * A graph element - either a Node or a Relation.
 *
 * This is the fundamental unit of data in Drasi. Nodes represent entities,
 * and Relations represent connections between nodes.
 *
 * Based on: core/src/models/element.rs (lines 66-79)
 */
@jsonSchema
@discriminator("type")
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
 * Based on: core/src/models/source_change.rs (lines 19-25)
 */
@jsonSchema
enum SourceChangeOperation {
  /** Insert a new element */
  insert: "insert",

  /** Update an existing element */
  update: "update",

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
 * Based on: core/src/models/source_change.rs (lines 19-25)
 */
@jsonSchema
@discriminator("operation")
union SourceChange {
  /** Insert a new element */
  insert: InsertChange,

  /** Update an existing element */
  update: UpdateChange,

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
 * Update operation - modifies an existing element.
 */
@jsonSchema
model UpdateChange {
  /** Discriminator field */
  operation: "update";

  /** The updated element (full state after update) */
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
