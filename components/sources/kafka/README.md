# Kafka Source Plugin

`drasi-source-kafka` consumes Kafka topic records and maps each message to a Drasi `SourceChange`.

## Highlights

- Multi-partition replay position encoding with per-partition comparator
- Custom `KafkaPositionComparator` for partition-aware replay filtering
- Template-based mapping via `drasi-source-mapping`
- Tombstone (`null` payload) support as Delete events
- SASL/security protocol passthrough (`security.protocol`, `sasl.*`)

## Configuration

| Field | Required | Description |
|---|---|---|
| `bootstrapServers` | yes | Kafka brokers (`host:port[,host:port]`) |
| `topic` | yes | Topic to consume |
| `groupId` | yes | Consumer group id |
| `nodeLabel` | yes | Default node label for default mapping/tombstones |
| `mappings` | no | Custom mapping list (default: key→id, payload→properties) |
| `autoOffsetReset` | no | `earliest` (default) or `latest` |
| `securityProtocol` | no | Kafka `security.protocol` |
| `saslMechanism` | no | Kafka `sasl.mechanism` |
| `saslUsername` | no | Kafka `sasl.username` |
| `saslPassword` | no | Kafka `sasl.password` |
| `additionalProperties` | no | Extra rdkafka properties |

## Incoming trace context

Kafka record headers `traceparent` and optional `tracestate` use the shared
SDK's W3C extraction. Each record (including tombstones) retains its own trace
ID, sender parent and sampling decision while it is dispatched. Missing context
clears the previous record's parent; malformed metadata warns and leaves the
record context-free without rejecting valid data. Context is not added to
payload properties or Kafka checkpoint positions.

## Testing

```bash
cargo test -p drasi-source-kafka --lib
cargo test -p drasi-source-kafka --test integration -- --ignored
```

Integration tests require Docker.

## Troubleshooting

- If no data arrives, confirm topic/partition assignment and that test data is JSON.
- If replay behaves unexpectedly, ensure resume positions are valid and partition count matches.
- For auth issues, verify `securityProtocol`/`sasl*` values are valid for the broker.
