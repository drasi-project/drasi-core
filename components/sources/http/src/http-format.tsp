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
 * HTTP Source Format Specification
 *
 * This format is used by the HTTP source for directly ingesting graph changes
 * via HTTP POST requests. It provides a simple, JSON-based format for sending
 * node and relation changes to Drasi.
 *
 * Source: server-core/src/sources/http/models.rs
 */

import "@typespec/json-schema";
import "../../../../typespec/core-types.tsp";

using TypeSpec.JsonSchema;

namespace Drasi.Sources.Http;

/**
 * A source change event in the HTTP source format.
 *
 * This is a tagged union discriminated by the "operation" field.
 * Each operation type has different required fields.
 *
 * Based on: server-core/src/sources/http/models.rs (HttpSourceChange)
 */
@jsonSchema
@discriminator("operation")
union HttpSourceChange {
  /** Insert a new element */
  insert: HttpInsert,

  /** Update an existing element */
  update: HttpUpdate,

  /** Delete an element */
  delete: HttpDelete
}

/**
 * Insert operation in HTTP source format.
 */
@jsonSchema
model HttpInsert {
  /** Operation type discriminator */
  operation: "insert";

  /** The element to insert (node or relation) */
  element: HttpElement;

  /** Optional timestamp in nanoseconds (defaults to current time if not provided) */
  timestamp?: uint64;
}

/**
 * Update operation in HTTP source format.
 */
@jsonSchema
model HttpUpdate {
  /** Operation type discriminator */
  operation: "update";

  /** The updated element (full state after update) */
  element: HttpElement;

  /** Optional timestamp in nanoseconds (defaults to current time if not provided) */
  timestamp?: uint64;
}

/**
 * Delete operation in HTTP source format.
 *
 * For deletes, only the ID is required. Labels are optional.
 */
@jsonSchema
model HttpDelete {
  /** Operation type discriminator */
  operation: "delete";

  /** ID of the element to delete */
  id: string;

  /** Optional labels for the element being deleted */
  labels?: string[];

  /** Optional timestamp in nanoseconds (defaults to current time if not provided) */
  timestamp?: uint64;
}

/**
 * A graph element in the HTTP source format - either a Node or Relation.
 *
 * This is discriminated by the "type" field.
 *
 * Based on: server-core/src/sources/http/models.rs (HttpElement)
 */
@jsonSchema
@discriminator("type")
union HttpElement {
  /** A node in the graph */
  node: HttpNode,

  /** A relation (edge) in the graph */
  relation: HttpRelation
}

/**
 * A node in the HTTP source format.
 *
 * Represents an entity with an ID, labels, and properties.
 */
@jsonSchema
model HttpNode {
  /** Type discriminator */
  type: "node";

  /** Unique identifier for this node */
  id: string;

  /** Labels/types for this node (e.g., ["User", "Person"]) */
  labels: string[];

  /** Node properties (JSON object) */
  @extension("x-typespec-name", "properties")
  properties?: {};
}

/**
 * A relation in the HTTP source format.
 *
 * Represents a directed edge connecting two nodes.
 */
@jsonSchema
model HttpRelation {
  /** Type discriminator */
  type: "relation";

  /** Unique identifier for this relation */
  id: string;

  /** Labels/types for this relation (e.g., ["FOLLOWS", "LIKES"]) */
  labels: string[];

  /** ID of the source node (where the relation starts) */
  from: string;

  /** ID of the target node (where the relation points to) */
  to: string;

  /** Relation properties (JSON object) */
  @extension("x-typespec-name", "properties")
  properties?: {};
}

/**
 * Batch of changes.
 *
 * The HTTP source can accept arrays of changes in a single request.
 */
@jsonSchema
model HttpSourceChangeBatch is HttpSourceChange[];

/**
 * Example usage:
 *
 * Insert a node:
 * ```json
 * {
 *   "operation": "insert",
 *   "element": {
 *     "type": "node",
 *     "id": "user_123",
 *     "labels": ["User", "Person"],
 *     "properties": {
 *       "name": "John Doe",
 *       "age": 30,
 *       "active": true
 *     }
 *   },
 *   "timestamp": 1234567890000
 * }
 * ```
 *
 * Insert a relation:
 * ```json
 * {
 *   "operation": "insert",
 *   "element": {
 *     "type": "relation",
 *     "id": "follows_123",
 *     "labels": ["FOLLOWS"],
 *     "from": "user_123",
 *     "to": "user_456",
 *     "properties": {
 *       "since": "2024-01-01"
 *     }
 *   }
 * }
 * ```
 *
 * Update a node:
 * ```json
 * {
 *   "operation": "update",
 *   "element": {
 *     "type": "node",
 *     "id": "user_123",
 *     "labels": ["User"],
 *     "properties": {
 *       "name": "Jane Doe",
 *       "age": 31
 *     }
 *   }
 * }
 * ```
 *
 * Delete an element:
 * ```json
 * {
 *   "operation": "delete",
 *   "id": "user_123",
 *   "labels": ["User"],
 *   "timestamp": 1234567890000
 * }
 * ```
 *
 * Batch of changes:
 * ```json
 * [
 *   {
 *     "operation": "insert",
 *     "element": {
 *       "type": "node",
 *       "id": "user_1",
 *       "labels": ["User"],
 *       "properties": {"name": "Alice"}
 *     }
 *   },
 *   {
 *     "operation": "insert",
 *     "element": {
 *       "type": "node",
 *       "id": "user_2",
 *       "labels": ["User"],
 *       "properties": {"name": "Bob"}
 *     }
 *   },
 *   {
 *     "operation": "insert",
 *     "element": {
 *       "type": "relation",
 *       "id": "follows_1",
 *       "labels": ["FOLLOWS"],
 *       "from": "user_1",
 *       "to": "user_2",
 *       "properties": {"since": "2025-01-01"}
 *     }
 *   }
 * ]
 * ```
 */
