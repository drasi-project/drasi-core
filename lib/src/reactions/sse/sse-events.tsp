/**
 * TypeSpec definitions for Server-Sent Events (SSE) payloads
 * emitted by the DrasiServerCore SSE Reaction.
 *
 * This specification defines the structure of events sent via
 * Server-Sent Events protocol to connected web clients.
 */

import "@typespec/json-schema";

using TypeSpec.JsonSchema;

namespace DrasiServerCore.SSE;

/**
 * Union type representing all possible SSE event payloads.
 * Clients receive one of these event types through the EventSource API.
 */
@discriminator("type")
union SSEEvent {
  QueryResultEvent,
  HeartbeatEvent,
}

/**
 * Query result event containing continuous query results from the
 * DrasiServerCore query engine. This is the primary event type
 * containing actual data changes.
 *
 * This event is broadcast when a query produces new results due to
 * data changes in the source systems.
 */
model QueryResultEvent {
  /**
   * Unique identifier of the query that produced this result set.
   * Clients can use this to filter events from specific queries.
   *
   * @example "temperature-alerts"
   * @example "customer-orders-123"
   */
  queryId: string;

  /**
   * Array of result records returned by the query.
   * Each record is a JSON object containing the fields specified
   * in the Cypher RETURN clause.
   *
   * The structure of each result object depends on the query.
   * For example:
   * - Query: MATCH (s:Sensor) RETURN s.id, s.temperature
   * - Result: {"s.id": "sensor-001", "s.temperature": 85.2}
   *
   * @example [{"id": "sensor-001", "temperature": 85.2, "location": "Building A"}]
   */
  results: unknown[];

  /**
   * Unix timestamp in milliseconds (UTC) when the SSE server
   * broadcast this event.
   *
   * Note: This is the broadcast time, not the query execution time.
   * The query execution timestamp is in the QueryResult structure
   * but is not included in the SSE event payload.
   *
   * @example 1706742123456
   */
  timestamp: int64;
}

/**
 * Heartbeat event sent periodically to keep connections alive
 * and help clients detect server unavailability.
 *
 * Heartbeats are sent at the interval configured by the
 * `heartbeat_interval_ms` property (default: 15 seconds).
 *
 * Clients should monitor heartbeat events to detect connection
 * health and implement reconnection logic if heartbeats stop.
 */
model HeartbeatEvent {
  /**
   * Event type discriminator. Always "heartbeat" for heartbeat events.
   */
  type: "heartbeat";

  /**
   * Unix timestamp in milliseconds (UTC) when the heartbeat
   * was sent by the server.
   *
   * Clients can use this to:
   * - Verify clock synchronization with server
   * - Measure round-trip latency
   * - Detect stale connections
   *
   * @example 1706742123456
   */
  ts: int64;
}

/**
 * Configuration properties for the SSE Reaction.
 * These properties are specified in the ReactionConfig.properties map.
 */
model SSEReactionProperties {
  /**
   * Network interface address to bind the HTTP server to.
   *
   * Common values:
   * - "0.0.0.0": Listen on all network interfaces (default)
   * - "127.0.0.1": Listen on localhost only (for local development)
   * - Specific IP: Listen on a specific network interface
   *
   * @default "0.0.0.0"
   */
  host?: string = "0.0.0.0";

  /**
   * TCP port number for the HTTP server.
   * Must be available and not blocked by firewall.
   *
   * @default 50051
   * @minimum 1
   * @maximum 65535
   */
  port?: uint16 = 50051;

  /**
   * HTTP path where the SSE endpoint is exposed.
   * Clients connect to http://host:port{sse_path}.
   *
   * Must start with "/". Can include path segments.
   *
   * @default "/events"
   * @example "/events"
   * @example "/api/v1/stream"
   * @example "/sse"
   */
  sse_path?: string = "/events";

  /**
   * Interval in milliseconds between heartbeat events.
   *
   * Heartbeats keep connections alive through proxies and firewalls
   * and help clients detect server unavailability.
   *
   * Recommended minimum: 5000ms (5 seconds)
   * Default: 15000ms (15 seconds)
   *
   * Too frequent: unnecessary network traffic
   * Too infrequent: proxies may timeout connections
   *
   * @default 15000
   * @minimum 1000
   */
  heartbeat_interval_ms?: uint64 = 15000;
}

/**
 * SSE protocol metadata and connection information.
 * This is not sent as an event payload, but describes the
 * protocol characteristics.
 */
model SSEConnectionInfo {
  /**
   * Content type for SSE responses.
   * Always "text/event-stream" per SSE specification.
   */
  contentType: "text/event-stream";

  /**
   * HTTP method used to establish SSE connection.
   * Always GET per SSE specification.
   */
  method: "GET";

  /**
   * Connection behavior.
   * SSE uses persistent HTTP connections with streaming response.
   */
  connection: "keep-alive";

  /**
   * Cache control header.
   * SSE responses should not be cached.
   */
  cacheControl: "no-cache";

  /**
   * CORS policy applied by the SSE reaction.
   * By default, allows all origins (*).
   */
  corsPolicy: {
    allowOrigin: string[];
    allowMethods: string[];
    allowHeaders: string[];
  };

  /**
   * Broadcast channel capacity.
   * Maximum number of events buffered before events are dropped
   * for slow clients.
   */
  broadcastBufferSize: 1024;

  /**
   * Keep-alive interval for SSE protocol-level comments.
   * Separate from heartbeat events, sent as SSE comment lines.
   *
   * @unit seconds
   */
  keepAliveInterval: 30;
}

/**
 * SSE event wire format.
 * This describes how events are encoded in the SSE protocol.
 *
 * Format:
 * ```
 * data: <JSON payload>
 *
 * ```
 *
 * Example:
 * ```
 * data: {"queryId":"temp-alerts","results":[{"temp":85}],"timestamp":1706742123456}
 *
 * ```
 */
model SSEEventWireFormat {
  /**
   * SSE field type. Always "data" for event payloads.
   */
  field: "data";

  /**
   * JSON-serialized event payload.
   * One of QueryResultEvent or HeartbeatEvent.
   */
  payload: string;

  /**
   * Event terminator. Always a blank line per SSE specification.
   */
  terminator: "\n\n";
}

/**
 * Example query result event payloads for different query types.
 */
namespace Examples {
  /**
   * Example: Simple sensor query
   * Query: MATCH (s:Sensor) WHERE s.temperature > 80 RETURN s.id, s.temperature, s.location
   */
  model SensorQueryResult {
    queryId: "temperature-alerts";
    results: [
      {
        "s.id": "sensor-001",
        "s.temperature": 85.2,
        "s.location": "Building A",
      },
      {
        "s.id": "sensor-042",
        "s.temperature": 92.7,
        "s.location": "Building C",
      },
    ];
    timestamp: 1706742123456;
  }

  /**
   * Example: Aggregation query
   * Query: MATCH (o:Order) RETURN o.customerId, COUNT(o) as orderCount, SUM(o.total) as totalAmount
   */
  model AggregationQueryResult {
    queryId: "customer-orders";
    results: [
      {
        "o.customerId": "cust-123",
        orderCount: 5,
        totalAmount: 1250.75,
      },
    ];
    timestamp: 1706742138789;
  }

  /**
   * Example: Heartbeat event
   */
  model SampleHeartbeat {
    type: "heartbeat";
    ts: 1706742153456;
  }

  /**
   * Example: Empty result set
   * Query produced no matching results after a data change
   */
  model EmptyQueryResult {
    queryId: "temperature-alerts";
    results: [];
    timestamp: 1706742168123;
  }
}
