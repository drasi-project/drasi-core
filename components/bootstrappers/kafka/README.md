# Kafka Bootstrap Provider

`drasi-bootstrap-kafka` bootstraps initial query state by reading Kafka from each partition's low watermark to high watermark and emitting events through the bootstrap channel.

## Behavior

- Reads each partition from `Offset::Beginning` (effective low watermark)
- Captures `{low, high}` watermarks and completes when all partitions reach their high watermark
- Emits mapped changes (Insert/Update/Delete) in stream order
- Emits tombstones as Delete events
- Returns `BootstrapResult.source_position` set to high watermark vector for streaming handoff

## Configuration

| Field | Required | Description |
|---|---|---|
| `bootstrapServers` | no | Kafka brokers (`host:port[,host:port]`). Falls back to source config. |
| `topic` | no | Topic to consume. Falls back to source config. |
| `nodeLabel` | no | Default node label for default mapping and tombstones. Falls back to source config. |
| `mappings` | no | Custom mapping list. Falls back to source config. |
| `securityProtocol` | no | Kafka `security.protocol` (e.g. `SASL_SSL`) |
| `saslMechanism` | no | Kafka `sasl.mechanism` |
| `saslUsername` | no | Kafka `sasl.username` |
| `saslPassword` | no | Kafka `sasl.password` |
| `additionalProperties` | no | Extra rdkafka properties |

When bootstrap config omits values, descriptor creation can fall back to source config fields.

## Usage

Use via source builder or plugin descriptor registration. See:

- `examples/lib/kafka-getting-started/`
- `components/sources/kafka/tests/integration.rs`
