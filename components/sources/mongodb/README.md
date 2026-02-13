# MongoDB Source for Drasi

A source component that captures change streams from a MongoDB Replica Set and forwards them as Drasi Source Changes.

## Overview

The MongoDB Source connects to a MongoDB Replica Set (or Sharded Cluster) and watches for data changes (Inserts, Updates, Replaces, Deletes) using [Change Streams](https://www.mongodb.com/docs/manual/changeStreams/). It converts these database events into standardized Drasi Source Events (Nodes), allowing you to build continuous queries and reactions based on realtime MongoDB activity.

## Architecture Flow

```mermaid
graph LR
    Mongo[MongoDB Replica Set] -->|Change Stream| Source[MongoDB Source]
    Source -->|SourceChange| SourceBase[Drasi SourceBase]
    SourceBase -->|Distribute| Queries[Drasi Queries]
    Queries -->|Results| Reactions[Drasi Reactions]
```

1.  **Change Stream**: The source establishes a persistent connection to the MongoDB changestream.
2.  **Conversion**: BSON documents and change events are converted into Drasi Elements (`Node`).
3.  **Dispatch**: Changes are pushed to the Drasi Runtime for processing.

---

## Features

-   **Realtime Change Capture**: Consumes the MongoDB `watch()` stream for low-latency updates.
-   **Multi-Collection Support**: Can watch one or multiple collections within a database.
-   **Resume Tokens**: Automatically persists resume tokens to the Drasi State Store. If the source restarts, it resumes consumption exactly where it left off, preventing data loss.
-   **Partial Updates**: Efficiently handles MongoDB `update` operations. It hydrates "dot notation" fields (e.g., `user.address.city`) into structured property maps.
-   **Full Document Lookup**: Configured to use `updateLookup` to ensure the full document state is available when needed (though partial updates are preferred for performance when available).
-   **Structure Handling**: Converts complex BSON types (ObjectId, Date, Embedded Docs) into Drasi standard types.
-   **Resilience**: Handles connection drops and temporary unavailability with automatic reconnection and backoff.

## Configuration

The source is configured via a YAML file.

### basic_config.yaml
```yaml
kind: Source
name: my-mongo-source
spec:
  kind: mongodb
  properties:
    connection_string: "mongodb://user:pass@localhost:27017/mydb?replicaSet=rs0"
    database: "mydb"
    collections: 
      - "users"
      - "orders"
```

### Configuration Fields

| Field | Type | Required | Default | Description |
| :--- | :--- | :--- | :--- | :--- |
| `connection_string` | String | Yes | - | Standard MongoDB connection URI. |
| `database` | String | Optional* | - | The database name to watch. *Required if not specified in connection string.* |
| `collection` | String | Optional | - | Single collection to watch (legacy mode). |
| `collections` | List\<String\> | Optional | `[]` | List of collections to watch. **Recommended.** |
| `username` | String | Optional | - | Overrides username in connection string. |
| `password` | String | Optional | - | Overrides password in connection string. |
| `pipeline` | List\<Document\> | Optional | `[]` | Custom aggregation pipeline validation/filtering stages passed to the change stream. |

### Precedence Rules
1.  **Database**: If specified in both `connection_string` (e.g., `mongodb://.../mydb`) AND `database` config, they **MUST** match. If they differ, the source will fail to start with an `Ambiguous configuration` error.
2.  **Collections**: You must specify at least one collection via `collection` or `collections`. If both are provided, they are merged.

---

## Authentication & Secrets

Do not hardcode credentials in your YAML files. Use Drasi's environment variable injection or secret management.

### Credentials Precedence
1.  **Config Fields**: `username` / `password` in yaml (Lowest priority, discouraged for secrets).
2.  **Connection String**: Credentials embedded in the URI.
3.  **Environment Variables**: `MONGODB_USERNAME` and `MONGODB_PASSWORD` environment variables (Highest priority).

**Best Practice:**
Inject credentials via Kubernetes Secrets or environment variables at runtime:

```yaml
# In your deployment manifest
env:
  - name: MONGODB_USERNAME
    valueFrom:
      secretKeyRef:
        name: mongo-secrets
        key: username
  - name: MONGODB_PASSWORD
    valueFrom:
      secretKeyRef:
        name: mongo-secrets
        key: password
```

The source log automatically redacts connection strings and credentials.

---

## Multi-Collection Support

You can watch multiple collections in the same database.

**Configuration:**
```yaml
collections:
  - "users"
  - "products"
  - "orders"
```

**Behavior:**
-   The source opens a single stream on the **Database** and filters events by `ns.coll` (namespace collection).
-   Events are tagged with their origin collection (see Mapping Model).
-   Efficient resource usage compared to running multiple sources.

---

## Update Handling Semantics

Drasi needs to know the *current state* of a node to evaluate queries. MongoDB provides flexible update formats; here is how they are handled:

1.  **Insert / Replace**: The `full_document` is always used to build the Element properties.
2.  **Update (Partial)**:
    -   If `full_document` is available in the event, it is used.
    -   If only `update_description` is available:
        -   **Updated Fields**: Dot-notation fields (e.g., `{"address.city": "London"}`) are "hydrated" into nested objects (`{address: {city: "London"}}`).
        -   **Removed Fields**: Fields listed in `removed_fields` are explicitly set to `Null` in the element properties.
3.  **Delete**: No properties are preserved; the element is marked for deletion.

---

## Resume Token Behavior

The source ensures "at-least-once" delivery semantics using MongoDB Resume Tokens.

-   **Storage**: Resume tokens are stored in the configured Drasi **State Store** (e.g., Redis or Kubernetes specific store) under the key `resume_token`.
-   **Restart**: On startup, the source checks the state store.
    -   **Found**: It resumes the change stream from that specific token.
    -   **Not Found**: It starts consuming from the *current moment* (Live).
-   **Persistence**: The token is updated and persisted atomically with event dispatch.

---

## Mapping Model

How MongoDB BSON translates to the Drasi Property Graph Model:

### Node Identity
Drasi Elements must have a unique ID.
Format: `CollectionName:ObjectIdHex`

Examples:
-   Collection: `users`, `_id`: `ObjectId("5f...")` -> ID: `users:5f...`
-   Collection: `settings`, `_id`: `"global"` -> ID: `settings:global`

### Property Mapping
| MongoDB (BSON) | Drasi (ElementValue) |
| :--- | :--- |
| `String` | String |
| `Int32`, `Int64` | Integer |
| `Double` | Float |
| `Boolean` | Bool |
| `Null` | Null |
| `Document` | Object (Map) |
| `Array` | List |
| `ObjectId` | String (Hex representation) |
| `DateTime` | String (RFC3339 format) |

### Deletes
A MongoDB `delete` event generates a Drasi `Change::Delete` event. The Node with the corresponding ID is removed from the query graph.

---

## Integration Testing

### Requirements
-   **Replica Set**: MongoDB Change Streams **REQUIRE** a Replica Set. They do not work on a standalone single node unless it is configured as a single-node replica set.

### Running Implementation Tests
This repository includes integration tests that spin up a Docker container.

```bash
# Run the integration tests (requires Docker)
cargo test --test mongo_integration -- --ignored
```

*Note: The `--ignored` flag is used because these tests are slow/heavy and require Docker.*

---

## MongoDB Setup Guide (Local Development)

To run this source locally, you need a MongoDB Replica Set.

1.  **Start MongoDB Container**:
    ```bash
    docker run -d --rm -p 27017:27017 \
      --name mongo-repl \
      mongo:5.0 mongod --replSet rs0 --bind_ip_all
    ```

2.  **Initiate Replica Set**:
    Inside the container (or via a client):
    ```bash
    docker exec -it mongo-repl mongosh --eval "rs.initiate()"
    ```

3.  **Verify**:
    ```bash
    docker exec -it mongo-repl mongosh --eval "rs.status().ok"
    # Should return 1
    ```

Now change streams are enabled.

---

## End-to-End Example

### 1. MongoDB Data (Input)
Command executed in MongoDB:
```javascript
db.users.insert({
  "_id": ObjectId("60b8d295f1d21034907d32a1"),
  "name": "Alice",
  "age": 30,
  "skills": ["Rust", "MongoDB"]
})
```

### 2. Drasi Source Configuration
```yaml
collections: ["users"]
```

### 3. Source Change Event (Internal)
The source generates this internal event:

```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: {
            reference: "users:60b8d295f1d21034907d32a1",
            labels: ["users"]
        },
        properties: {
            "name": "Alice",
            "age": 30,
            "skills": ["Rust", "MongoDB"]
        }
    }
}
```

### 4. Drasi Query (Cypher)
```cypher
MATCH (u:users) WHERE u.age >= 30 RETURN u.name
```

### 5. Query Output
```json
{
  "u.name": "Alice"
}
```

---

## Limitations

-   **Transaction Support**: The source does not currently expose transaction markers to downstream queries; events are processed individually.
-   **Schema Validation**: The source does not enforce a schema; downstream queries must handle optional/missing fields if the MongoDB schema is loose.

## Future Work

-   [ ] Support for Sharded Cluster specific configurations (though currently works via mongos).
-   [ ] Advanced filtering pipeline configuration via external files for complex logic.
