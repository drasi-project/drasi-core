# Kafka Bootstrap Provider

`drasi-bootstrap-kafka` bootstraps initial query state by reading Kafka from each partition's low watermark to high watermark and emitting events through the bootstrap channel.

## Behavior

- Reads each partition from `Offset::Beginning` (effective low watermark)
- Captures `{low, high}` watermarks and completes when all partitions reach their high watermark
- Emits mapped changes (Insert/Update/Delete) in stream order
- Emits tombstones as Delete events
- Returns `BootstrapResult.source_position` set to high watermark vector for streaming handoff

## Configuration

Bootstrap config supports:

- `bootstrapServers`
- `topic`
- `nodeLabel`
- optional `mappings`
- optional security/SASL settings (`securityProtocol`, `saslMechanism`, `saslUsername`, `saslPassword`)
- optional `additionalProperties`

When bootstrap config omits values, descriptor creation can fallback to source config fields.

## Usage

Use via source builder or plugin descriptor registration. See:

- `examples/lib/kafka-getting-started/`
- `components/sources/kafka/tests/integration.rs`
