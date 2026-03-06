# RabbitMQ Reaction

The RabbitMQ reaction publishes Drasi continuous query results to RabbitMQ exchanges.

## Features
- Publish ADD/UPDATE/DELETE results to RabbitMQ
- Exchange types: direct, topic, fanout, headers
- Handlebars templates for routing keys, headers, and body
- TLS support for AMQPS connections

## Configuration Example

```rust
use drasi_reaction_rabbitmq::{
    ExchangeType, PublishSpec, QueryPublishConfig, RabbitMQReaction, RabbitMQReactionConfig,
};
use std::collections::HashMap;

let mut routes = HashMap::new();
routes.insert(
    "user-updates".to_string(),
    QueryPublishConfig {
        added: Some(PublishSpec {
            routing_key: "user.created".to_string(),
            headers: HashMap::from([
                ("x-user-id".to_string(), "{{after.id}}".to_string()),
                ("x-operation".to_string(), "ADD".to_string()),
            ]),
            body_template: Some(
                r#"{"user_id":{{after.id}},"name":{{json after.name}}}"#.to_string(),
            ),
        }),
        updated: None,
        deleted: None,
    },
);

let config = RabbitMQReactionConfig {
    connection_string: "amqp://guest:guest@localhost:5672/%2f".to_string(),
    exchange_name: "drasi-events".to_string(),
    exchange_type: ExchangeType::Topic,
    exchange_durable: true,
    message_persistent: true,
    tls_enabled: false,
    tls_cert_path: None,
    tls_key_path: None,
    query_configs: routes,
};

let reaction = RabbitMQReaction::new("rabbitmq-reaction", vec!["user-updates".to_string()], config)?;
```

## Template Context

The following values are available in Handlebars templates:

- `after`, `before`, `data` (per operation)
- `query_id`
- `operation` (ADD, UPDATE, DELETE)
- `timestamp` (RFC3339)
- `metadata`

## TLS Notes

When `tls_enabled` is true, the connection string must use `amqps://`.
If `tls_key_path` is provided, it is treated as a PKCS#12 (PFX) identity with an empty password.

## Integration Test

Run the integration test (requires Docker):

```bash
cargo test -p drasi-reaction-rabbitmq -- --ignored --nocapture
```
