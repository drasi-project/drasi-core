# MongoDB Source Plugin

This source plugin enables Drasi to consume change events from a MongoDB Replcia Set or Sharded Cluster.

## Features

- Captures `insert`, `update`, `replace`, `delete` events via Change Streams.
- Handles resume tokens for reliable stream consumption.
- Converts MongoDB BSON types to Drasi Element types.
- Supports hydration of dot-notation updates (e.g. `{"a.b": 1}` -> `{"a": {"b": 1}}`).

## Configuration

```yaml
source:
  type: mongodb
  properties:
    connection_string: "mongodb://localhost:27017"
    database: "mydb"
    collection: "mycollection"
```

## Prerequisites

- MongoDB must be running as a Replica Set or Sharded Cluster. Standalone instances do not support Change Streams.
