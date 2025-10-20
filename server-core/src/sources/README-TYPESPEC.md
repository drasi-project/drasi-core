# Drasi Sources - TypeSpec Documentation

This directory contains TypeSpec (.tsp) files that document the data formats used by various Drasi source types.

## Overview

TypeSpec is a language for describing APIs and data formats. These files serve as:
- **Format Documentation**: Clear, machine-readable specs for JSON data formats
- **Reference**: Examples and type definitions for developers integrating with Drasi
- **Validation**: Can be used to generate JSON Schema validators
- **Code Generation**: Can generate OpenAPI specs, client libraries, etc.

## Files

### `common-types.tsp`
Core Drasi data structures used across all sources.

**Defines:**
- `ElementValue` - Recursive union type for property values (null, bool, int, float, string, list, object)
- `ElementPropertyMap` - Map of property names to values
- `ElementReference` - Unique identifier for an element (source_id + element_id)
- `ElementMetadata` - Metadata including labels and timestamp
- `Element` - Node or Relation with properties
- `SourceChange` - Insert, Update, or Delete operations

**Based on:** `/core/src/models/`

**Used by:** All source format specifications

### `http-format.tsp`
HTTP Direct format for the HTTP source.

**Defines:**
- `DirectSourceChange` - Change events with operation discriminator
- `DirectElement` - Simplified node/relation format
- `DirectNode` - Node with id, labels, properties
- `DirectRelation` - Relation with id, labels, from, to, properties
- Batch format support

**Based on:** `server-core/src/sources/http/direct_format.rs`

**API Endpoint:** POST to HTTP source endpoint with JSON body

**Example:**
```json
{
  "operation": "insert",
  "element": {
    "type": "node",
    "id": "user_123",
    "labels": ["User"],
    "properties": {"name": "Alice"}
  }
}
```

### `platform-format.tsp`
CloudEvent-wrapped format for the Platform source (Redis Streams).

**Defines:**
- `CloudEvent` - CloudEvents 1.0 wrapper
- `PlatformEvent` - CDC-style events with op field ("i", "u", "d")
- `PlatformPayload` - Before/after state with source metadata
- `PlatformSource` - Source metadata with db, table, timestamp
- `SourceSubscriptionData` - Control messages for query subscriptions

**Based on:** `server-core/src/sources/platform/mod.rs`

**Data Source:** Redis Streams via consumer groups

**Key Features:**
- Supports both data changes and control messages
- Control messages have `db="Drasi"` and `table="SourceSubscription"`
- Data messages use `table="node"` or `table="rel"`
- Includes profiling timestamps (reactivatorStart_ns, reactivatorEnd_ns)

**Example:**
```json
{
  "data": [{
    "op": "i",
    "payload": {
      "after": {
        "id": "user_1",
        "labels": ["User"],
        "properties": {"name": "Alice"}
      },
      "source": {
        "db": "myapp",
        "table": "node",
        "ts_ns": 1704067200000000000
      }
    }
  }]
}
```

### `postgres-types.tsp`
PostgreSQL-specific types and WAL message formats.

**Defines:**
- `PostgresValue` - All PostgreSQL data types
- `ColumnInfo` - Column metadata from relations
- `RelationInfo` - Table schema information
- `ReplicaIdentity` - DEFAULT, NOTHING, FULL, INDEX
- `WalMessage` - WAL logical replication messages
- `TransactionInfo` - Transaction metadata
- Common PostgreSQL type OIDs

**Based on:**
- `server-core/src/sources/postgres/types.rs`
- `server-core/src/sources/postgres/decoder.rs`

**Protocol:** PostgreSQL logical replication (pgoutput plugin)

**Note:** This is primarily for internal reference. The actual wire protocol uses PostgreSQL's binary format, not JSON.

### `application-api.tsp`
Documentation for the Application source (Rust API).

**Defines:**
- Conceptual structure (not a JSON API)
- Usage examples in Rust
- Configuration format

**Based on:** `server-core/src/sources/application/`

**Important:** This is a Rust-only API. The TypeSpec file is for documentation only. Applications embed DrasiServerCore and call Rust APIs directly using `drasi_core::models` types.

**Example Rust Usage:**
```rust
let handle = core.get_application_source_handle("my-source").await?;
handle.send_change(SourceChange::Insert { element: node }).await?;
```

## Using TypeSpec Files

### Viewing Documentation
Read the .tsp files directly - they contain extensive comments and examples.

### Generating JSON Schema
```bash
# Install TypeSpec CLI
npm install -g @typespec/compiler

# Generate JSON Schema
tsp compile common-types.tsp --emit @typespec/json-schema
tsp compile http-format.tsp --emit @typespec/json-schema
tsp compile platform-format.tsp --emit @typespec/json-schema
```

### Generating OpenAPI Specs
```bash
tsp compile http-format.tsp --emit @typespec/openapi3
```

### Integration
1. **Validation**: Use generated JSON Schema to validate incoming data
2. **Documentation**: Generate HTML docs from TypeSpec
3. **Client Generation**: Generate client libraries in various languages
4. **Testing**: Use examples in TypeSpec for test cases

## Format Comparison

| Source Type | Format | Protocol | Bootstrap Support |
|-------------|--------|----------|-------------------|
| HTTP | Direct JSON | HTTP POST | Script File |
| Platform | CloudEvent JSON | Redis Streams | Platform Provider |
| PostgreSQL | Binary WAL | PostgreSQL Protocol | PostgreSQL Provider |
| Application | Rust API | In-process | Application Provider |
| gRPC | Protobuf | gRPC | (via .proto files) |
| Mock | Direct JSON | In-process | Script File |

## gRPC and Protobuf

**Note:** The gRPC source uses Protocol Buffers, not JSON. The schema is already defined in `.proto` files:
- Location: `server-core/proto/drasi/v1/`
- Files: Check the proto directory for source and query definitions
- Documentation: See protobuf comments in .proto files

TypeSpec is not used for gRPC because Protocol Buffers already provide a complete schema definition language. If you need gRPC documentation, refer to the .proto files directly.

## Related Documentation

- **Core Models**: `/core/src/models/` - Rust source of truth
- **HTTP Source**: `server-core/src/sources/http/` - Implementation
- **Platform Source**: `server-core/src/sources/platform/` - Implementation
- **PostgreSQL Source**: `server-core/src/sources/postgres/` - Implementation
- **Application Source**: `server-core/src/sources/application/` - Implementation

## Maintenance

When updating source implementations:
1. Update the Rust code first
2. Update corresponding TypeSpec file(s)
3. Update examples in TypeSpec comments
4. Regenerate any derived schemas/docs

## Questions?

- TypeSpec Documentation: https://typespec.io/
- Drasi Project: https://github.com/drasi-project/
- Core Library Docs: `cargo doc --open`

## License

These TypeSpec files are part of the Drasi project and are licensed under the Apache License 2.0.
