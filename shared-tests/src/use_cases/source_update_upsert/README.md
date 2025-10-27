# SourceUpdate Upsert Semantics Tests

## Purpose

Continuous Queries in Drasi process `SourceChange` events. These tests validate that `SourceChange::Update` implements correct **upsert semantics**: the first update to a non-existent element creates it, and subsequent updates modify the existing element. This is a fundamental contract of the continuous query engine.

## Why This Matters

**Stateless Source Adapters**
Many real-world data sources cannot track whether entities exist in the engine. IoT sensors send periodic snapshots, polling-based adapters fetch current state and analytics platforms like Azure Data Explorer (Kusto) can continuously export events where events describe the state of entities at a specific timestamp. These sources need a simple contract: send updates, and the engine handles creation vs. modification.

**Behavioral Consistency**
These tests guarantee that all storage backend implementations (in-memory, RocksDB, Garnet, future backends) handle upserts for `SourceChange::Update` events sent by any such sources identically.