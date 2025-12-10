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
 * Platform CloudEvent Format Specification
 *
 * This format is used by the Platform source to consume events from Redis Streams.
 * Events are wrapped in CloudEvent format and contain either data changes or control
 * messages for managing query subscriptions.
 *
 * Source: server-core/src/sources/platform/mod.rs
 */

import "@typespec/json-schema";
import "../../../../typespec/core-types.tsp";

using TypeSpec.JsonSchema;

namespace Drasi.Sources.Platform;

/**
 * CloudEvent wrapper for platform events.
 *
 * All events from the platform source are wrapped in CloudEvent format.
 * The CloudEvent specification is from https://cloudevents.io/
 *
 * Based on: server-core/src/sources/platform/mod.rs (lines 692-829)
 */
@jsonSchema
model CloudEvent {
  /** CloudEvent specification version */
  specversion?: string = "1.0";

  /** Event type identifier */
  type?: string;

  /** Event source identifier */
  source?: string;

  /** Unique event ID */
  id?: string;

  /** Event timestamp */
  time?: string;

  /** Content type of data */
  datacontenttype?: string = "application/json";

  /** Event data - array of PlatformEvent */
  data: PlatformEvent[];
}

/**
 * A platform event containing either data changes or control messages.
 *
 * Events are discriminated by the payload.source.db field:
 * - If db = "Drasi" (case-insensitive): Control message
 * - Otherwise: Data message
 *
 * Based on: server-core/src/sources/platform/mod.rs (lines 645-682)
 */
@jsonSchema
model PlatformEvent {
  /** Operation type: "i" (insert), "u" (update), "d" (delete) */
  op: PlatformOperation;

  /** Event payload containing source metadata and element data */
  payload: PlatformPayload;

  /** Optional reactivator start timestamp in nanoseconds */
  reactivatorStart_ns?: uint64;

  /** Optional reactivator end timestamp in nanoseconds */
  reactivatorEnd_ns?: uint64;
}

/**
 * Platform operation types.
 *
 * Based on CDC operation codes.
 */
@jsonSchema
enum PlatformOperation {
  /** Insert operation */
  insert: "i",

  /** Update operation */
  update: "u",

  /** Delete operation */
  delete: "d"
}

/**
 * Payload of a platform event.
 *
 * Contains the before/after state of the element and source metadata.
 *
 * Based on: server-core/src/sources/platform/mod.rs (lines 716-760)
 */
@jsonSchema
model PlatformPayload {
  /** State before the change (for update and delete) */
  before?: PlatformElementData;

  /** State after the change (for insert and update) */
  after?: PlatformElementData;

  /** Source metadata */
  source: PlatformSource;
}

/**
 * Source metadata for platform events.
 *
 * Identifies the source and timing of the event.
 *
 * Based on: server-core/src/sources/platform/mod.rs (lines 666-677)
 */
@jsonSchema
model PlatformSource {
  /** Database/namespace identifier */
  db: string;

  /** Table/collection name - also serves as element type ("node", "rel") or control type */
  table: string;

  /** Timestamp in nanoseconds when the change occurred */
  ts_ns: uint64;
}

/**
 * Element data in platform format.
 *
 * Represents a node or relation with its properties.
 *
 * Based on: server-core/src/sources/platform/mod.rs (lines 740-810)
 */
@jsonSchema
model PlatformElementData {
  /** Element ID */
  id: string;

  /** Element labels/types */
  labels: string[];

  /** Element properties (JSON object) */
  @extension("x-typespec-name", "properties")
  properties: {};

  /** For relations: ID of the source node */
  startId?: string;

  /** For relations: ID of the target node */
  endId?: string;
}

/**
 * Control message for source subscription management.
 *
 * Control messages use the same CloudEvent wrapper but with db="Drasi" and
 * table="SourceSubscription".
 *
 * Based on: server-core/src/sources/platform/mod.rs (lines 831-941)
 */
@jsonSchema
model SourceSubscriptionData {
  /** Query ID that this subscription belongs to */
  queryId: string;

  /** Query node ID (unique identifier for this query instance) */
  queryNodeId: string;

  /** Node labels to subscribe to */
  nodeLabels?: string[];

  /** Relation labels to subscribe to */
  relLabels?: string[];
}

/**
 * Control operation types.
 *
 * Maps to platform operations for control messages.
 */
@jsonSchema
enum ControlOperation {
  /** Add a subscription */
  insert: "i",

  /** Update a subscription */
  update: "u",

  /** Remove a subscription */
  delete: "d"
}

/**
 * Example usage:
 *
 * Data Event - Insert Node:
 * ```json
 * {
 *   "specversion": "1.0",
 *   "type": "drasi.change",
 *   "source": "postgres-source",
 *   "id": "event-123",
 *   "time": "2025-01-01T00:00:00Z",
 *   "datacontenttype": "application/json",
 *   "data": [
 *     {
 *       "op": "i",
 *       "payload": {
 *         "after": {
 *           "id": "user_123",
 *           "labels": ["User", "Person"],
 *           "properties": {
 *             "name": "John Doe",
 *             "email": "john@example.com",
 *             "age": 30
 *           }
 *         },
 *         "source": {
 *           "db": "myapp",
 *           "table": "node",
 *           "ts_ns": 1704067200000000000
 *         }
 *       },
 *       "reactivatorStart_ns": 1704067200000000000,
 *       "reactivatorEnd_ns": 1704067200100000000
 *     }
 *   ]
 * }
 * ```
 *
 * Data Event - Update Relation:
 * ```json
 * {
 *   "data": [
 *     {
 *       "op": "u",
 *       "payload": {
 *         "before": {
 *           "id": "follows_1",
 *           "labels": ["FOLLOWS"],
 *           "startId": "user_123",
 *           "endId": "user_456",
 *           "properties": {
 *             "since": "2024-01-01"
 *           }
 *         },
 *         "after": {
 *           "id": "follows_1",
 *           "labels": ["FOLLOWS"],
 *           "startId": "user_123",
 *           "endId": "user_456",
 *           "properties": {
 *             "since": "2024-01-01",
 *             "weight": 5
 *           }
 *         },
 *         "source": {
 *           "db": "myapp",
 *           "table": "rel",
 *           "ts_ns": 1704067200000000000
 *         }
 *       }
 *     }
 *   ]
 * }
 * ```
 *
 * Data Event - Delete:
 * ```json
 * {
 *   "data": [
 *     {
 *       "op": "d",
 *       "payload": {
 *         "before": {
 *           "id": "user_123",
 *           "labels": ["User"]
 *         },
 *         "source": {
 *           "db": "myapp",
 *           "table": "node",
 *           "ts_ns": 1704067200000000000
 *         }
 *       }
 *     }
 *   ]
 * }
 * ```
 *
 * Control Event - Add Subscription:
 * ```json
 * {
 *   "data": [
 *     {
 *       "op": "i",
 *       "payload": {
 *         "after": {
 *           "queryId": "my-query",
 *           "queryNodeId": "my-query-instance-1",
 *           "nodeLabels": ["User", "Product"],
 *           "relLabels": ["FOLLOWS", "PURCHASED"]
 *         },
 *         "source": {
 *           "db": "Drasi",
 *           "table": "SourceSubscription",
 *           "ts_ns": 1704067200000000000
 *         }
 *       }
 *     }
 *   ]
 * }
 * ```
 *
 * Control Event - Remove Subscription:
 * ```json
 * {
 *   "data": [
 *     {
 *       "op": "d",
 *       "payload": {
 *         "before": {
 *           "queryId": "my-query",
 *           "queryNodeId": "my-query-instance-1"
 *         },
 *         "source": {
 *           "db": "Drasi",
 *           "table": "SourceSubscription",
 *           "ts_ns": 1704067200000000000
 *         }
 *       }
 *     }
 *   ]
 * }
 * ```
 */
