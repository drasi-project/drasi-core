# MQTT Reaction

A reaction that publishes Drasi continuous query result diffs to an MQTT broker, using topic and payload templates per operation and per query.

## Overview

The MQTT Reaction monitors continuous query results and publishes one MQTT message per diff event (`ADD`, `UPDATE`, `DELETE`, and `AGGREGATION`) to a configured topic.

### Key Capabilities

- **Protocol Support**: MQTT v5 by default, with MQTT v3.1.1 support
- **Transport Support**: `mqtt`, `mqtts`, `ws`, and `wss` broker URLs
- **Per-Query Routing**: Route each query and operation (`added`, `updated`, `deleted`) to its own topic/template
- **Template Fallback**: Use `default_template` when a query-specific route is not defined
- **Handlebars Rendering**: Render both payloads and topic templates using the same operation context
- **QoS and Retain Controls**: Per-operation QoS 0/1, retained messages, and empty payload publishing
- **Authentication**: Username/password credentials via identity provider integration
- **Backpressure Control**: Internal queueing and MQTT event-channel capacity tuning

### Use Cases

- **IoT Fan-Out**: Forward graph changes to device and edge consumers
- **Automation Pipelines**: Publish operational state changes to downstream MQTT workflows
- **Bridge Integrations**: Connect Drasi to broker-backed systems that consume standardized MQTT topics

**Best for**: event-driven systems where MQTT is the preferred transport for publishing result diffs.

## Configuration

### Builder Pattern (Recommended)

The builder provides a fluent, type-safe API:

```rust
use drasi_lib::identity::PasswordIdentityProvider;
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_reaction_mqtt::config::{MqttExtension, MqttQoS};
use drasi_reaction_mqtt::MqttReaction;

fn mqtt_extension(topic: &str, qos: MqttQoS) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
    }
}

let default_template = QueryConfig {
    added: Some(TemplateSpec::with_extension(
        r#"{"op":"{{operation}}","query":"{{query_name}}","data":{{json after}}}"#,
        mqtt_extension("drasi/{{query_name}}/added", MqttQoS::AtLeastOnce),
    )),
    updated: Some(TemplateSpec::with_extension(
        r#"{"op":"{{operation}}","before":{{json before}},"after":{{json after}}}"#,
        mqtt_extension("drasi/{{query_name}}/updated", MqttQoS::AtLeastOnce),
    )),
    deleted: Some(TemplateSpec::with_extension(
        r#"{"op":"{{operation}}","data":{{json before}}}"#,
        mqtt_extension("drasi/{{query_name}}/deleted", MqttQoS::AtLeastOnce),
    )),
};

let reaction = MqttReaction::builder("mqtt-reaction")
    .with_query("temperature-alerts")
    .with_url("mqtt://localhost:1883")
    .with_client_id("drasi-temperature-alerts")
    .with_keep_alive(30)
    .with_default_template(default_template)
    .with_identity_provider(Box::new(PasswordIdentityProvider::new("user", "pass")))
    .build()?;
```

#### With Per-Query Routes

Use per-query route maps when different queries need different topic or payload formats:

```rust
use std::collections::HashMap;

use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_reaction_mqtt::config::{MqttExtension, MqttQoS};
use drasi_reaction_mqtt::MqttReaction;

fn ext(topic: &str, qos: MqttQoS) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
    }
}

let mut routes = HashMap::new();
routes.insert(
    "sensor-query".to_string(),
    QueryConfig {
        added: Some(TemplateSpec::with_extension(
            "{{json after}}",
            ext("alerts/{{query_name}}/added", MqttQoS::AtMostOnce),
        )),
        updated: Some(TemplateSpec::with_extension(
            r#"{"before":{{json before}},"after":{{json after}}}"#,
            ext("alerts/{{query_name}}/updated", MqttQoS::AtLeastOnce),
        )),
        deleted: Some(TemplateSpec::with_extension(
            "{{json before}}",
            ext("alerts/{{query_name}}/deleted", MqttQoS::AtLeastOnce),
        )),
    },
);

let reaction = MqttReaction::builder("sensor-mqtt")
    .with_query("sensor-query")
    .with_url("mqtt://localhost:1883")
    .with_routes(routes)
    .build()?;
```

#### With a Default Template

Apply a fallback template to all subscribed queries without dedicated route entries:

```rust
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_reaction_mqtt::config::{MqttExtension, MqttQoS};
use drasi_reaction_mqtt::MqttReaction;

fn ext(topic: &str) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos: MqttQoS::AtLeastOnce,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
    }
}

let default_template = QueryConfig {
    added: Some(TemplateSpec::with_extension("ADD {{after.id}}", ext("events/{{query_name}}/added"))),
    updated: Some(TemplateSpec::with_extension("UPDATE {{before.id}} -> {{after.id}}", ext("events/{{query_name}}/updated"))),
    deleted: Some(TemplateSpec::with_extension("DELETE {{before.id}}", ext("events/{{query_name}}/deleted"))),
};

let reaction = MqttReaction::builder("multi-query-mqtt")
    .with_queries(vec!["query-a".to_string(), "query-b".to_string()])
    .with_url("mqtt://localhost:1883")
    .with_default_template(default_template)
    .build()?;
```

### Config Struct Approach

For programmatic construction or deserialization workflows:

```rust
use std::collections::HashMap;

use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_reaction_mqtt::config::{
    MqttExtension, MqttProtocolVersion, MqttQoS, MqttReactionConfig, QueryConfig as _,
};
use drasi_reaction_mqtt::MqttReaction;

fn ext(topic: &str, qos: MqttQoS) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
    }
}

let mut routes = HashMap::new();
routes.insert(
    "temperature-alerts".to_string(),
    QueryConfig {
        added: Some(TemplateSpec::with_extension("{{json after}}", ext("alerts/temperature/added", MqttQoS::AtLeastOnce))),
        updated: Some(TemplateSpec::with_extension(r#"{"before":{{json before}},"after":{{json after}}}"#, ext("alerts/temperature/updated", MqttQoS::AtLeastOnce))),
        deleted: Some(TemplateSpec::with_extension("{{json before}}", ext("alerts/temperature/deleted", MqttQoS::AtLeastOnce))),
    },
);

let config = MqttReactionConfig {
    url: "mqtt://localhost:1883".to_string(),
    client_id: Some("drasi-temperature-alerts".to_string()),
    protocol_version: MqttProtocolVersion::V5,
    routes,
    default_template: None,
    identity_provider: None,
    tls: None,
    event_channel_capacity: 100,
    max_inflight: Some(100),
    keep_alive: Some(30),
    clean_start: Some(true),
    conn_timeout: Some(5_000),
    session_expiry_interval: None,
};

let reaction = MqttReaction::new(
    "mqtt-reaction",
    vec!["temperature-alerts".to_string()],
    config,
)?;
```

## Configuration Options

### MqttReactionConfig

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `url` | Broker URL. Supported schemes: `mqtt`, `mqtts`, `ws`, `wss`. Must include host and port. | `String` | `mqtt://localhost:1883` in builder |
| `client_id` | MQTT client ID. If omitted, defaults to `drasi-mqtt-{reaction_id}`. | `Option<String>` | `None` |
| `protocol_version` | MQTT wire protocol version. | `MqttProtocolVersion` | `V5` |
| `routes` | Query-specific operation templates. Keys must match subscribed query IDs. | `HashMap<String, QueryConfig<MqttExtension>>` | Empty |
| `default_template` | Fallback operation templates when no query-specific route is present. | `Option<QueryConfig<MqttExtension>>` | `None` |
| `identity_provider` | Credentials provider. Only username/password credentials are accepted in v1. | `Option<Box<dyn IdentityProvider>>` | `None` |
| `tls` | TLS settings for secure schemes (`mqtts` and `wss`). Must be `None` for plain schemes. | `Option<MqttTlsConfig>` | `None` |
| `event_channel_capacity` | Internal AsyncClient->EventLoop channel capacity. Must be > 0. | `usize` | `100` |
| `max_inflight` | Maximum outgoing inflight QoS 1 messages. Must be > 0 when set. | `Option<u16>` | rumqttc default |
| `keep_alive` | Keep-alive ping interval in seconds. Must be >= 5 when set. | `Option<u64>` | rumqttc default |
| `clean_start` | Clean session/start behavior. | `Option<bool>` | rumqttc default |
| `conn_timeout` | Connection timeout in milliseconds (v5 option). Must be > 0 when set. | `Option<u64>` | rumqttc default |
| `session_expiry_interval` | Session expiry interval in seconds (v5). Must be > 0 when set. | `Option<u32>` | `None` |

### MqttTlsConfig

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `ca` | Custom CA bundle in PEM bytes. Uses system CA store when `None`. | `Option<Vec<u8>>` | `None` |
| `alpn` | Optional ALPN protocols (byte vectors). | `Option<Vec<Vec<u8>>>` | `None` |
| `client_auth` | Reserved for v2. Rejected in v1 validation. | `Option<(Vec<u8>, Vec<u8>)>` | `None` |
| `accept_invalid_certs` | Development-only mode to skip certificate verification. | `bool` | `false` |

### QueryConfig<MqttExtension>

| Name | Description | Type | Required |
|------|-------------|------|----------|
| `added` | Template spec for ADD events. | `Option<TemplateSpec<MqttExtension>>` | No |
| `updated` | Template spec for UPDATE and AGGREGATION events. | `Option<TemplateSpec<MqttExtension>>` | No |
| `deleted` | Template spec for DELETE events. | `Option<TemplateSpec<MqttExtension>>` | No |

### TemplateSpec<MqttExtension>

| Name | Description | Type |
|------|-------------|------|
| `template` | Handlebars payload template. If empty and `empty_payload` is `false`, publishes raw JSON payload. | `String` |
| `extension.topic` | MQTT topic template. Must render to a safe MQTT publish topic. | `String` |
| `extension.qos` | MQTT QoS (`AtMostOnce` or `AtLeastOnce`). | `MqttQoS` |
| `extension.retain` | MQTT retain flag. | `bool` |
| `extension.empty_payload` | Publish a zero-byte payload regardless of `template`. | `bool` |
| `extension.message_expiry_interval` | MQTT v5 message expiry interval in seconds. | `Option<u32>` |

### Builder Methods

| Method | Description |
|--------|-------------|
| `new(id)` | Create a builder with reaction ID |
| `with_query(id)` | Add one query subscription |
| `with_queries(ids)` | Set all query subscriptions |
| `with_url(url)` | Set broker URL |
| `with_client_id(id)` | Set explicit MQTT client ID |
| `with_protocol_version(version)` | Set `V5` or `V3_1_1` |
| `with_route(query_id, config)` | Add one per-query route |
| `with_routes(routes)` | Replace all routes |
| `with_routes_extend(routes)` | Extend route map |
| `with_default_template(template)` | Set fallback operation templates |
| `with_identity_provider(provider)` | Set credentials provider |
| `with_tls(tls)` | Set TLS configuration |
| `with_event_channel_capacity(n)` | Set MQTT event channel capacity |
| `with_max_inflight(n)` | Set max outgoing inflight messages |
| `with_keep_alive(seconds)` | Set keep-alive interval |
| `with_clean_start(bool)` | Set clean session/start behavior |
| `with_conn_timeout(ms)` | Set connection timeout |
| `with_session_expiry_interval(seconds)` | Set session expiry interval |
| `with_priority_queue_capacity(n)` | Set internal reaction priority queue capacity |
| `with_auto_start(bool)` | Enable or disable auto-start |
| `with_config(config)` | Apply full `MqttReactionConfig` |
| `build()` | Validate and build `MqttReaction` |

## Publish Payload and Template Variables

When a query diff arrives, the reaction selects the matching template, renders topic and payload, validates topic safety, and publishes to the broker.

### Template Variables

| Variable | Description | Available Operations |
|----------|-------------|---------------------|
| `after` | New/current result state | ADD, UPDATE |
| `before` | Previous result state | UPDATE, DELETE |
| `data` | Raw update payload field when present | UPDATE |
| `operation` | `ADD`, `UPDATE`, `DELETE`, or `AGGREGATION` | All |
| `query_name` | Query ID that produced the diff | All |
| `timestamp` | RFC3339 timestamp derived from query result metadata | All |

### Template Priority

1. Query-specific `routes[query_id]` template for the operation
2. `default_template` for the operation
3. Skip publish when no template exists for that operation

### Helper

- `{{json value}}` serializes nested values as valid JSON

## Execution Model

```
Query result diff
        |
        v
Select operation template (route/default)
        |
        v
Render topic + payload (Handlebars)
        |
        v
Validate rendered topic safety
        |
        v
Publish via MQTT client (QoS/retain/expiry settings)
        |
        v
Event loop handles ACKs/reconnect logic and status transitions
```

## Topic Safety

Configured and rendered topics are validated before publish:

- Topic cannot be empty
- Topic cannot include MQTT wildcards `+` or `#`
- Topic cannot include null characters
- Topic cannot include empty levels such as `a//b`
- Rendered interpolations cannot introduce additional `/` levels
- Topic UTF-8 length must not exceed 65,535 bytes

## Broker URL and TLS

Transport is selected by URL scheme:

| Scheme | Transport |
|---|---|
| `mqtt://` | TCP |
| `mqtts://` | TLS over TCP |
| `ws://` | WebSocket |
| `wss://` | TLS over WebSocket |

Notes:

- URL must include host and port.
- `tls` is required for `mqtts` and `wss`.
- `tls` must be `None` for `mqtt` and `ws`.
- `client_auth` in TLS config is not supported in v1 (validation error).

## Authentication

Username/password authentication is configured via identity provider:

```rust
use drasi_lib::identity::PasswordIdentityProvider;
use drasi_reaction_mqtt::MqttReaction;

let reaction = MqttReaction::builder("mqtt-reaction")
    .with_query("temperature-alerts")
    .with_url("mqtt://broker.example.com:1883")
    .with_identity_provider(Box::new(PasswordIdentityProvider::new("user", "pass")))
    .build()?;
```

In v1, credential types other than username/password are rejected.

## Observability

The reaction exposes configuration/runtime properties via `Reaction::properties()`:

- `url`
- `id`
- `protocol_version`
- `routes_count`
- `has_default_template`
- `has_identity_provider`
- `has_tls`
- `event_channel_capacity`
- `max_inflight`
- `keep_alive`
- `clean_start`
- `conn_timeout`
- `session_expiry_interval`

## Validation

Configuration is validated at build/creation time:

- `event_channel_capacity` must be greater than 0
- `max_inflight` must be greater than 0 when set
- `keep_alive` must be at least 5 when set
- `conn_timeout` must be greater than 0 when set
- `session_expiry_interval` must be greater than 0 when set
- URL scheme must be one of `mqtt`, `mqtts`, `ws`, `wss`
- URL must include host and port
- TLS settings must match URL scheme expectations
- `tls.client_auth` must be `None` in v1
- Route keys must reference subscribed query IDs
- Topic templates must pass topic validation rules

## Running Checks

Unit tests:

```bash
cargo test -p drasi-reaction-mqtt
```

Integration tests (requires Docker and Mosquitto):

```bash
cargo test -p drasi-reaction-mqtt --test integration_tests -- --ignored --nocapture
```

## Limitations

- QoS 2 (`ExactlyOnce`) is not supported
- mTLS client certificates are not supported in v1
- One MQTT message is published per diff event

## Plugin Packaging

This reaction is built as a dynamic plugin (`cdylib`) for runtime loading by drasi-server.

Key files:

- `Cargo.toml` (includes `crate-type = ["lib", "cdylib"]`)
- `src/descriptor.rs` (`ReactionPluginDescriptor` implementation and OpenAPI schema)
- `src/lib.rs` (`drasi_plugin_sdk::export_plugin!` entry point)

Build command:

```bash
cargo build -p drasi-reaction-mqtt --features dynamic-plugin
```
