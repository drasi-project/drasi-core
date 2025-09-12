
# AGENTS.md: `index-garnet` Crate

## Architectural Intent
A **persistent storage backend** implementing the core interfaces (`ElementIndex`, `ResultIndex`, `FutureQueue`) using a Redis-compatible server (Garnet).

## Architectural Rules
*   **Protobuf Serialization**: All complex structs MUST be serialized using Protocol Buffers (`prost`) before storage.
*   **Pipelining**: Write operations MUST use Redis pipelines (`redis::pipe()`) to batch commands and minimize network round-trips.
*   **Atomic Updates**: Where possible, use pipelining to ensure atomic updates to the element and its associated indexes.

## Data Mapping Strategy
*   **Elements**: Stored as Redis **Hashes** (`HSET`).
    *   Key: `ei:{query_id}:{source_id}:{element_id}`
    *   Fields: `e` (Protobuf data), `slots` (BitSet of affinities).
*   **Graph Structure**: Modeled with Redis **Sets** (`SADD`/`SREM`).
    *   Inbound/Outbound Keys: `ei:{query_id}:$in:...` / `ei:{query_id}:$out:...`.
*   **Future Queue**: Stored as a Redis **Sorted Set** (`ZADD`/`ZPOPMIN`).
    *   Key: `fqi:{query_id}`
    *   Score: `due_time` (timestamp).
    *   Member: Protobuf `StoredFutureElementRef`.