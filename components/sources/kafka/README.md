# Kafka Source Plugin

`drasi-source-kafka` consumes Kafka topic records and maps each message to a Drasi `SourceChange`.

## Highlights

- Multi-partition replay position encoding (`[count][offsets...]`)
- Custom `KafkaPositionComparator` for component-wise replay filtering
- Template-based mapping via `drasi-source-mapping`
- Tombstone (`null` payload) support as Delete events
- SASL/security protocol passthrough (`security.protocol`, `sasl.*`)

## Configuration

| Field | Required | Description |
|---|---|---|
| `bootstrap_servers` | yes | Kafka brokers (`host:port[,host:port]`) |
| `topic` | yes | Topic to consume |
| `group_id` | yes | Consumer group id |
| `node_label` | yes | Default node label for default mapping/tombstones |
| `mappings` | no | Custom mapping list (default: key->id, payload->properties) |
| `auto_offset_reset` | no | `earliest` (default) or `latest` |
| `security_protocol` | no | Kafka `security.protocol` |
| `sasl_mechanism` | no | Kafka `sasl.mechanism` |
| `sasl_username` | no | Kafka `sasl.username` |
| `sasl_password` | no | Kafka `sasl.password` |
| `additional_properties` | no | Extra rdkafka properties |

## Testing

```bash
cargo test -p drasi-source-kafka --lib
cargo test -p drasi-source-kafka --test integration -- --ignored
```

Integration tests require Docker.

## Troubleshooting

- If no data arrives, confirm topic/partition assignment and that test data is JSON.
- If replay behaves unexpectedly, ensure resume positions are valid and partition count matches.
- For auth issues, verify `security_protocol`/`sasl_*` values are valid for the broker.
