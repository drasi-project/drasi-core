/**
 * TypeSpec definition for Drasi Platform Reaction CloudEvent format
 *
 * This file defines the structure of CloudEvents published by the Platform Reaction
 * to Redis Streams. It follows the CloudEvents 1.0 specification and includes
 * Drasi-specific payload structures for query results and control events.
 */

import "@typespec/http";
import "@typespec/openapi3";

using TypeSpec.Http;
using TypeSpec.OpenAPI;

/**
 * CloudEvent envelope following CloudEvents 1.0 specification
 *
 * This wrapper contains all events published by the Platform Reaction.
 * The `data` field contains either a ResultChangeEvent or ResultControlEvent.
 */
@doc("CloudEvent envelope wrapping result or control events")
model CloudEvent {
  /**
   * The event payload - either a change event with query results or a control event
   */
  @doc("Event payload containing query results or control signals")
  data: ResultEvent;

  /**
   * Content type of the data field - always "application/json"
   */
  @doc("Content type - always application/json")
  datacontenttype: "application/json";

  /**
   * Unique identifier for this event (UUID v4)
   */
  @doc("Unique event identifier (UUID v4)")
  id: string;

  /**
   * Name of the Dapr pubsub component that should handle this event
   */
  @doc("Dapr pubsub component name")
  pubsubname: string;

  /**
   * Identifier of the event source (e.g., "drasi-core")
   */
  @doc("Event source identifier")
  source: string;

  /**
   * CloudEvents specification version - always "1.0"
   */
  @doc("CloudEvents spec version")
  specversion: "1.0";

  /**
   * Event timestamp in ISO 8601 format
   */
  @doc("Event timestamp (ISO 8601)")
  time: string;

  /**
   * Topic/stream name where this event is published ({query-id}-results)
   */
  @doc("Topic name following pattern {query-id}-results")
  topic: string;

  /**
   * Optional distributed tracing ID
   */
  @doc("Optional trace ID for distributed tracing")
  traceid?: string;

  /**
   * Optional W3C trace parent for distributed tracing
   */
  @doc("Optional W3C trace parent")
  traceparent?: string;

  /**
   * Optional W3C trace state for distributed tracing
   */
  @doc("Optional W3C trace state")
  tracestate?: string;

  /**
   * Event type - always "com.dapr.event.sent" for Dapr compatibility
   */
  @doc("Event type - always com.dapr.event.sent")
  @extension("x-ms-client-name", "EventType")
  type: "com.dapr.event.sent";
}

/**
 * Union type for result events - either change or control
 * Discriminated by the "kind" field
 */
@discriminator("kind")
@doc("Result event - either a change event or control event")
union ResultEvent {
  /**
   * Change event containing query results (adds, updates, deletes)
   */
  @doc("Query result change event")
  change: ResultChangeEvent,

  /**
   * Control event for lifecycle management
   */
  @doc("Lifecycle control event")
  control: ResultControlEvent,
}

/**
 * Change event containing query results
 *
 * This event type contains the actual data changes from continuous query evaluation.
 * It includes arrays of added, updated, and deleted results, along with metadata
 * for performance tracking.
 */
@doc("Change event containing query results")
model ResultChangeEvent {
  /**
   * Event kind discriminator - always "change"
   */
  @doc("Event kind - always 'change' for result events")
  kind: "change";

  /**
   * Query identifier that produced these results
   */
  @doc("Query identifier")
  queryId: string;

  /**
   * Monotonically increasing sequence number for result ordering
   */
  @doc("Sequence number for ordering")
  sequence: uint64;

  /**
   * Source timestamp in milliseconds since Unix epoch
   */
  @doc("Source timestamp in milliseconds since epoch")
  sourceTimeMs: uint64;

  /**
   * Array of objects representing added results (always present, may be empty)
   */
  @doc("Results that were added")
  addedResults: Record<unknown>[];

  /**
   * Array of update payloads with before/after states (always present, may be empty)
   */
  @doc("Results that were updated with before/after states")
  updatedResults: UpdatePayload[];

  /**
   * Array of objects representing deleted results (always present, may be empty)
   */
  @doc("Results that were deleted")
  deletedResults: Record<unknown>[];

  /**
   * Optional metadata including performance tracking information
   */
  @doc("Optional metadata including tracking information")
  metadata?: Metadata;
}

/**
 * Update payload containing before and after states
 *
 * Used for update operations to show the transition from one state to another.
 * Includes optional grouping keys for aggregation queries.
 */
@doc("Update payload with before/after states")
model UpdatePayload {
  /**
   * State before the update (may be null for some aggregations)
   */
  @doc("State before update")
  before?: Record<unknown>;

  /**
   * State after the update (may be null for some aggregations)
   */
  @doc("State after update")
  after?: Record<unknown>;

  /**
   * Optional array of field names used for grouping (for aggregation queries)
   */
  @doc("Grouping keys for aggregations")
  groupingKeys?: string[];
}

/**
 * Metadata structure containing tracking information
 *
 * This metadata provides end-to-end performance tracking across the Drasi Platform.
 * All timestamp fields are in nanoseconds since Unix epoch for high-precision timing.
 */
@doc("Metadata with performance tracking information")
model Metadata {
  /**
   * Tracking information for performance analysis
   */
  @doc("Performance tracking data")
  tracking?: TrackingMetadata;

  /**
   * Additional custom metadata fields (extensible)
   */
  @doc("Additional custom metadata")
  ...Record<unknown>;
}

/**
 * Tracking metadata for performance analysis
 *
 * Contains timing information from various stages of data processing:
 * - Source: When data entered the system and moved through source components
 * - Query: When query processing occurred
 */
@doc("Performance tracking metadata")
model TrackingMetadata {
  /**
   * Source-related tracking information
   */
  @doc("Source tracking data")
  source?: SourceTracking;

  /**
   * Query processing tracking information
   */
  @doc("Query tracking data")
  query?: QueryTracking;
}

/**
 * Source tracking data
 *
 * Tracks the flow of data through source components from origin to query processing.
 * All timestamps are in nanoseconds since Unix epoch.
 */
@doc("Source component tracking data")
model SourceTracking {
  /**
   * Sequence number for this event at the source
   */
  @doc("Source sequence number")
  seq?: uint64;

  /**
   * Original source timestamp (nanoseconds since epoch)
   */
  @doc("Original source timestamp (ns)")
  source_ns?: uint64;

  /**
   * When change router started processing (nanoseconds since epoch)
   */
  @doc("Change router start timestamp (ns)")
  changeRouterStart_ns?: uint64;

  /**
   * When change router finished processing (nanoseconds since epoch)
   */
  @doc("Change router end timestamp (ns)")
  changeRouterEnd_ns?: uint64;

  /**
   * When change dispatcher started processing (nanoseconds since epoch)
   */
  @doc("Change dispatcher start timestamp (ns)")
  changeDispatcherStart_ns?: uint64;

  /**
   * When change dispatcher finished processing (nanoseconds since epoch)
   */
  @doc("Change dispatcher end timestamp (ns)")
  changeDispatcherEnd_ns?: uint64;

  /**
   * When reactivator started (nanoseconds since epoch)
   */
  @doc("Reactivator start timestamp (ns)")
  reactivatorStart_ns?: uint64;

  /**
   * When reactivator finished (nanoseconds since epoch)
   */
  @doc("Reactivator end timestamp (ns)")
  reactivatorEnd_ns?: uint64;
}

/**
 * Query processing tracking data
 *
 * Tracks the timing of query evaluation from receiving the change to completing processing.
 * All timestamps are in nanoseconds since Unix epoch.
 */
@doc("Query processing tracking data")
model QueryTracking {
  /**
   * When result was enqueued for query processing (nanoseconds since epoch)
   */
  @doc("Query enqueue timestamp (ns)")
  enqueue_ns?: uint64;

  /**
   * When result was dequeued for processing (nanoseconds since epoch)
   */
  @doc("Query dequeue timestamp (ns)")
  dequeue_ns?: uint64;

  /**
   * When query core processing started (nanoseconds since epoch)
   */
  @doc("Query processing start timestamp (ns)")
  queryStart_ns?: uint64;

  /**
   * When query core processing ended (nanoseconds since epoch)
   */
  @doc("Query processing end timestamp (ns)")
  queryEnd_ns?: uint64;
}

/**
 * Control event for lifecycle management
 *
 * Control events signal state changes in query processing lifecycle.
 * They enable monitoring, coordination, and orchestration of query operations.
 */
@doc("Control event for query lifecycle management")
model ResultControlEvent {
  /**
   * Event kind discriminator - always "control"
   */
  @doc("Event kind - always 'control' for lifecycle events")
  kind: "control";

  /**
   * Query identifier this control event applies to
   */
  @doc("Query identifier")
  queryId: string;

  /**
   * Sequence number for ordering control events
   */
  @doc("Sequence number")
  sequence: uint64;

  /**
   * Timestamp when control event was generated (milliseconds since epoch)
   */
  @doc("Control event timestamp (ms since epoch)")
  sourceTimeMs: uint64;

  /**
   * Optional metadata (typically null for control events)
   */
  @doc("Optional metadata")
  metadata?: Metadata;

  /**
   * The control signal indicating lifecycle state
   */
  @doc("Control signal type")
  controlSignal: ControlSignal;
}

/**
 * Control signal union type
 *
 * Represents different lifecycle states for query processing.
 * Discriminated by the "kind" field.
 */
@discriminator("kind")
@doc("Control signal for query lifecycle state")
union ControlSignal {
  /**
   * Bootstrap process has started
   */
  @doc("Bootstrap started signal")
  bootstrapStarted: BootstrapStartedSignal,

  /**
   * Bootstrap process has completed
   */
  @doc("Bootstrap completed signal")
  bootstrapCompleted: BootstrapCompletedSignal,

  /**
   * Query is running normally
   */
  @doc("Running signal")
  running: RunningSignal,

  /**
   * Query has been stopped
   */
  @doc("Stopped signal")
  stopped: StoppedSignal,

  /**
   * Query has been deleted
   */
  @doc("Deleted signal")
  deleted: DeletedSignal,
}

/**
 * Bootstrap started control signal
 */
@doc("Bootstrap started signal")
model BootstrapStartedSignal {
  /**
   * Signal kind - always "bootstrapStarted"
   */
  @doc("Signal kind")
  kind: "bootstrapStarted";
}

/**
 * Bootstrap completed control signal
 */
@doc("Bootstrap completed signal")
model BootstrapCompletedSignal {
  /**
   * Signal kind - always "bootstrapCompleted"
   */
  @doc("Signal kind")
  kind: "bootstrapCompleted";
}

/**
 * Running control signal
 *
 * Emitted when the Platform Reaction starts and begins processing results.
 */
@doc("Running signal - query is actively processing")
model RunningSignal {
  /**
   * Signal kind - always "running"
   */
  @doc("Signal kind")
  kind: "running";
}

/**
 * Stopped control signal
 *
 * Emitted when the Platform Reaction stops processing results.
 */
@doc("Stopped signal - query processing has stopped")
model StoppedSignal {
  /**
   * Signal kind - always "stopped"
   */
  @doc("Signal kind")
  kind: "stopped";
}

/**
 * Deleted control signal
 *
 * Emitted when a query has been deleted.
 */
@doc("Deleted signal - query has been deleted")
model DeletedSignal {
  /**
   * Signal kind - always "deleted"
   */
  @doc("Signal kind")
  kind: "deleted";
}

/**
 * Example complete CloudEvent with change event
 */
@example(#{
  data: #{
    kind: "change",
    queryId: "sensor-monitoring",
    sequence: 12345,
    sourceTimeMs: 1705318245123,
    addedResults: [
      #{
        sensor_id: "temp-01",
        temperature: 23.5,
        location: "warehouse-A"
      }
    ],
    updatedResults: [
      #{
        before: #{
          sensor_id: "temp-02",
          temperature: 18.2
        },
        after: #{
          sensor_id: "temp-02",
          temperature: 19.1
        }
      }
    ],
    deletedResults: [],
    metadata: #{
      tracking: #{
        source: #{
          seq: 12345,
          source_ns: 1744055144490466971,
          changeRouterStart_ns: 1744055159124143047,
          changeRouterEnd_ns: 1744055173551481387
        },
        query: #{
          enqueue_ns: 1744055173551481387,
          dequeue_ns: 1744055178510629042,
          queryStart_ns: 1744055178510650750,
          queryEnd_ns: 1744055178510848750
        }
      }
    }
  },
  datacontenttype: "application/json",
  id: "550e8400-e29b-41d4-a716-446655440000",
  pubsubname: "drasi-pubsub",
  source: "drasi-core",
  specversion: "1.0",
  time: "2025-01-15T10:30:45.456Z",
  topic: "sensor-monitoring-results",
  traceid: "",
  traceparent: "",
  tracestate: "",
  type: "com.dapr.event.sent"
})
@doc("CloudEvent with change data")
model ExampleChangeCloudEvent extends CloudEvent {}

/**
 * Example CloudEvent with control event (Running)
 */
@example(#{
  data: #{
    kind: "control",
    queryId: "sensor-monitoring",
    sequence: 1,
    sourceTimeMs: 1705318245123,
    metadata: null,
    controlSignal: #{
      kind: "running"
    }
  },
  datacontenttype: "application/json",
  id: "650e8400-e29b-41d4-a716-446655440001",
  pubsubname: "drasi-pubsub",
  source: "drasi-core",
  specversion: "1.0",
  time: "2025-01-15T10:30:45.123Z",
  topic: "sensor-monitoring-results",
  traceid: "",
  traceparent: "",
  tracestate: "",
  type: "com.dapr.event.sent"
})
@doc("CloudEvent with running control signal")
model ExampleRunningCloudEvent extends CloudEvent {}

/**
 * Example CloudEvent with control event (Stopped)
 */
@example(#{
  data: #{
    kind: "control",
    queryId: "sensor-monitoring",
    sequence: 9999,
    sourceTimeMs: 1705318345123,
    metadata: null,
    controlSignal: #{
      kind: "stopped"
    }
  },
  datacontenttype: "application/json",
  id: "750e8400-e29b-41d4-a716-446655440002",
  pubsubname: "drasi-pubsub",
  source: "drasi-core",
  specversion: "1.0",
  time: "2025-01-15T10:31:45.123Z",
  topic: "sensor-monitoring-results",
  traceid: "",
  traceparent: "",
  tracestate: "",
  type: "com.dapr.event.sent"
})
@doc("CloudEvent with stopped control signal")
model ExampleStoppedCloudEvent extends CloudEvent {}
