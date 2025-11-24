/**
 * TypeSpec Definition for Adaptive HTTP Reaction Batch Format
 *
 * This specification defines the data format for the batch endpoint
 * used by the Drasi Adaptive HTTP Reaction when batch_endpoints_enabled is true.
 */

import "@typespec/http";
import "@typespec/json-schema";

using TypeSpec.Http;

namespace DrasiReaction.AdaptiveHttp;

/**
 * Query result data for ADD operations
 * Contains only the new row data
 */
model AddResult {
  /**
   * Operation type indicator
   */
  type: "ADD";

  /**
   * The new row data that was added to query results
   */
  data: Record<unknown>;

  /**
   * The new row data (same as data, provided for consistency)
   */
  after?: Record<unknown>;
}

/**
 * Query result data for UPDATE operations
 * Contains before and after states
 */
model UpdateResult {
  /**
   * Operation type indicator
   */
  type: "UPDATE";

  /**
   * The new row data (same as after)
   */
  data: Record<unknown>;

  /**
   * The previous row values before the update
   */
  before: Record<unknown>;

  /**
   * The new row values after the update
   */
  after: Record<unknown>;
}

/**
 * Query result data for DELETE operations
 * Contains only the deleted row data
 */
model DeleteResult {
  /**
   * Operation type indicator
   */
  type: "DELETE";

  /**
   * The deleted row data
   */
  data: Record<unknown>;

  /**
   * The deleted row data (same as data, provided for consistency)
   */
  before?: Record<unknown>;
}

/**
 * Union type for all query result types
 */
@discriminator("type")
union QueryResultData {
  AddResult,
  UpdateResult,
  DeleteResult,
}

/**
 * Batch result for a single query
 * Contains all results for one query ID with metadata
 */
model BatchResult {
  /**
   * The ID of the query that produced these results
   *
   * @example "user-changes"
   */
  query_id: string;

  /**
   * Array of query result objects
   * Each result contains type (ADD/UPDATE/DELETE) and associated data
   *
   * @minItems 1
   */
  results: QueryResultData[];

  /**
   * ISO 8601 timestamp when the batch was created
   *
   * @format date-time
   * @example "2025-10-19T12:34:56.789Z"
   */
  timestamp: string;

  /**
   * Number of results in this batch
   * This is the same as results.length and provided for convenience
   *
   * @minimum 1
   * @example 42
   */
  count: uint32;
}

/**
 * Batch request payload
 * Array of BatchResult objects, grouped by query_id
 *
 * @minItems 1
 */
@friendlyName("BatchRequest")
model BatchRequestPayload is BatchResult[];

/**
 * Success response for batch endpoint
 */
model BatchSuccessResponse {
  /**
   * Response status message
   */
  status: "success";

  /**
   * Number of batches processed
   */
  batches_processed: uint32;

  /**
   * Total number of results processed across all batches
   */
  total_results: uint32;

  /**
   * Optional message with additional details
   */
  message?: string;
}

/**
 * Error response for batch endpoint
 */
model BatchErrorResponse {
  /**
   * Response status message
   */
  status: "error";

  /**
   * Error code or type
   */
  error_code: string;

  /**
   * Human-readable error message
   */
  error_message: string;

  /**
   * Optional details about which batches or results failed
   */
  failed_batches?: {
    query_id: string;
    error: string;
  }[];
}

/**
 * Batch endpoint HTTP interface
 */
@route("/batch")
namespace Batch {
  /**
   * Process a batch of query results
   *
   * Accepts an array of BatchResult objects, each containing results for a specific query.
   * Results are grouped by query_id and include operation type (ADD/UPDATE/DELETE).
   *
   * @param body Array of BatchResult objects
   * @returns Success response with processing summary or error details
   */
  @post
  @summary("Process batch of query results")
  @tag("Reactions")
  op processBatch(
    @header("Content-Type") contentType: "application/json",
    @header("Authorization")? authorization: string,
    @body body: BatchRequestPayload
  ): BatchSuccessResponse | BatchErrorResponse;
}

/**
 * Example batch request payload
 */
@example(BatchRequestPayload, #{
  value: [
    {
      query_id: "user-changes",
      results: [
        {
          type: "ADD",
          data: {
            id: "user_123",
            name: "John Doe",
            email: "john@example.com",
            created_at: "2025-10-19T12:00:00Z"
          },
          after: {
            id: "user_123",
            name: "John Doe",
            email: "john@example.com",
            created_at: "2025-10-19T12:00:00Z"
          }
        },
        {
          type: "UPDATE",
          data: {
            id: "user_456",
            name: "Jane Smith",
            email: "jane@example.com",
            updated_at: "2025-10-19T12:30:00Z"
          },
          before: {
            id: "user_456",
            name: "Jane Doe",
            email: "jane@example.com",
            updated_at: "2025-10-19T12:00:00Z"
          },
          after: {
            id: "user_456",
            name: "Jane Smith",
            email: "jane@example.com",
            updated_at: "2025-10-19T12:30:00Z"
          }
        },
        {
          type: "DELETE",
          data: {
            id: "user_789",
            name: "Bob Wilson",
            email: "bob@example.com"
          },
          before: {
            id: "user_789",
            name: "Bob Wilson",
            email: "bob@example.com"
          }
        }
      ],
      timestamp: "2025-10-19T12:34:56.789Z",
      count: 3
    },
    {
      query_id: "order-events",
      results: [
        {
          type: "ADD",
          data: {
            order_id: "order_789",
            total: 125.50,
            status: "pending",
            created_at: "2025-10-19T12:34:00Z"
          },
          after: {
            order_id: "order_789",
            total: 125.50,
            status: "pending",
            created_at: "2025-10-19T12:34:00Z"
          }
        }
      ],
      timestamp: "2025-10-19T12:34:56.789Z",
      count: 1
    }
  ]
})

/**
 * Example success response
 */
@example(BatchSuccessResponse, #{
  value: {
    status: "success",
    batches_processed: 2,
    total_results: 4,
    message: "All results processed successfully"
  }
})

/**
 * Example error response
 */
@example(BatchErrorResponse, #{
  value: {
    status: "error",
    error_code: "VALIDATION_ERROR",
    error_message: "Invalid batch format: missing required field 'query_id'",
    failed_batches: [
      {
        query_id: "user-changes",
        error: "Missing required field: results"
      }
    ]
  }
})
