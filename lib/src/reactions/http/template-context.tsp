/**
 * TypeSpec definition for HTTP Reaction Handlebars Template Context
 *
 * This file defines the data structures available in Handlebars templates
 * within the DrasiServerCore HTTP reaction. These context variables are
 * populated at runtime based on query results and operation type.
 */

import "@typespec/json-schema";

using TypeSpec.JsonSchema;

namespace DrasiHttpReaction.TemplateContext;

/**
 * Base template context available for all operation types
 */
@jsonSchema
model BaseContext {
  /**
   * The name/ID of the query that produced this result
   * @example "users-query"
   */
  query_name: string;

  /**
   * The operation type that triggered this template rendering
   * @example "ADD", "UPDATE", or "DELETE"
   */
  operation: OperationType;
}

/**
 * Operation types for query results
 */
enum OperationType {
  /** New row added to query results */
  ADD,

  /** Existing row in query results was modified */
  UPDATE,

  /** Row removed from query results */
  DELETE,
}

/**
 * Template context for ADD operations
 *
 * Available when new rows are added to query results.
 *
 * @example
 * ```handlebars
 * {
 *   "id": "{{after.id}}",
 *   "name": "{{after.name}}",
 *   "email": "{{after.email}}"
 * }
 * ```
 */
@jsonSchema
model AddContext extends BaseContext {
  /**
   * The operation type (always "ADD")
   */
  operation: OperationType.ADD;

  /**
   * The new row data as a JSON object
   *
   * Fields depend on the query's RETURN clause.
   * Access nested fields with dot notation: {{after.fieldName}}
   *
   * @example
   * {
   *   "id": "user_123",
   *   "name": "John Doe",
   *   "email": "john@example.com",
   *   "created_at": "2025-10-19T12:00:00Z"
   * }
   */
  after: Record<unknown>;
}

/**
 * Template context for UPDATE operations
 *
 * Available when existing rows in query results are modified.
 * Provides both before and after states for comparison.
 *
 * @example
 * ```handlebars
 * {
 *   "id": "{{after.id}}",
 *   "changes": {
 *     "name": {
 *       "from": "{{before.name}}",
 *       "to": "{{after.name}}"
 *     }
 *   }
 * }
 * ```
 */
@jsonSchema
model UpdateContext extends BaseContext {
  /**
   * The operation type (always "UPDATE")
   */
  operation: OperationType.UPDATE;

  /**
   * The previous row values before the update
   *
   * Contains the same fields as 'after' but with old values.
   * Access with: {{before.fieldName}}
   *
   * @example
   * {
   *   "id": "user_123",
   *   "name": "John Doe",
   *   "email": "john@example.com"
   * }
   */
  before: Record<unknown>;

  /**
   * The new row values after the update
   *
   * Contains the current state of the row.
   * Access with: {{after.fieldName}}
   *
   * @example
   * {
   *   "id": "user_123",
   *   "name": "John Smith",
   *   "email": "john@example.com"
   * }
   */
  after: Record<unknown>;

  /**
   * Additional change data (same as 'after')
   *
   * Provided for compatibility but contains the same values as 'after'.
   * Prefer using 'after' for new templates.
   */
  data?: Record<unknown>;
}

/**
 * Template context for DELETE operations
 *
 * Available when rows are removed from query results.
 *
 * @example
 * ```handlebars
 * {
 *   "id": "{{before.id}}",
 *   "deleted_name": "{{before.name}}"
 * }
 * ```
 */
@jsonSchema
model DeleteContext extends BaseContext {
  /**
   * The operation type (always "DELETE")
   */
  operation: OperationType.DELETE;

  /**
   * The deleted row data
   *
   * Contains the last known state of the row before deletion.
   * Access with: {{before.fieldName}}
   *
   * @example
   * {
   *   "id": "user_123",
   *   "name": "John Doe",
   *   "email": "john@example.com"
   * }
   */
  before: Record<unknown>;
}

/**
 * Union type representing any valid template context
 */
@discriminator("operation")
union TemplateContext {
  add: AddContext,
  update: UpdateContext,
  delete: DeleteContext,
}

/**
 * Helper functions available in Handlebars templates
 */
namespace Helpers {
  /**
   * JSON serialization helper
   *
   * Serializes a value as a JSON string. Useful for embedding objects
   * within JSON templates without manual escaping.
   *
   * @param value - The value to serialize
   * @returns JSON string representation
   *
   * @example
   * ```handlebars
   * {"user": {{json after}}}
   * ```
   * Renders as:
   * ```json
   * {"user": {"id": "123", "name": "John"}}
   * ```
   */
  op json(value: unknown): string;
}

/**
 * Example template contexts for each operation type
 */
namespace Examples {
  /**
   * Example ADD context for a user entity
   */
  model ExampleAddContext {
    query_name: "users-query";
    operation: OperationType.ADD;
    after: {
      id: "user_123";
      name: "John Doe";
      email: "john@example.com";
      created_at: "2025-10-19T12:00:00Z";
    };
  }

  /**
   * Example UPDATE context for a user entity
   */
  model ExampleUpdateContext {
    query_name: "users-query";
    operation: OperationType.UPDATE;
    before: {
      id: "user_123";
      name: "John Doe";
      email: "john@example.com";
    };
    after: {
      id: "user_123";
      name: "John Smith";
      email: "john.smith@example.com";
    };
    data: {
      id: "user_123";
      name: "John Smith";
      email: "john.smith@example.com";
    };
  }

  /**
   * Example DELETE context for a user entity
   */
  model ExampleDeleteContext {
    query_name: "users-query";
    operation: OperationType.DELETE;
    before: {
      id: "user_123";
      name: "John Doe";
      email: "john@example.com";
    };
  }
}
