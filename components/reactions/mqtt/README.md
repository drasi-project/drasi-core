# MQTT Reaction

MQTT Reaction publishes Drasi continuous query result diffs to an MQTT broker.
It is useful for IoT, edge, automation, and event-driven integrations where
`ADD`, `UPDATE`, and `DELETE` changes should be delivered as MQTT messages.

## Overview

The MQTT reaction subscribes to one or more Drasi queries and publishes each
result diff to a configured MQTT topic.

Key capabilities:

- MQTT v5 by default, with MQTT v3.1.1 support
- Plain TCP and TLS broker URLs
- Per-query routing with `routes` and fallback `default_template`
- Handlebars templates for MQTT topics and payloads
- QoS 0 and QoS 1 publishing
- Retained messages and empty retained payloads
- Username/password authentication through an identity provider
- Priority queue processing in timestamp order

## Configuration

### Builder usage

```rust
use drasi_lib::identity::PasswordIdentityProvider;
use drasi_lib::reactions::common::TemplateSpec;
use drasi_reaction_mqtt::config::{MqttExtension, MqttQoS, QueryConfig};
use drasi_reaction_mqtt::MqttReaction;

fn mqtt_extension(topic: &str) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos: MqttQoS::AtLeastOnce,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
        slashes_count: topic.matches('/').count(),
    }
}

let default_template = QueryConfig {
    added: Some(TemplateSpec::with_extension(
        r#"{"op":"{{operation}}","query":"{{query_name}}","data":{{json after}}}"#,
        mqtt_extension("drasi/{{query_name}}/added"),
    )),
    updated: Some(TemplateSpec::with_extension(
        r#"{"op":"{{operation}}","before":{{json before}},"after":{{json after}}}"#,
        mqtt_extension("drasi/{{query_name}}/updated"),
    )),
    deleted: Some(TemplateSpec::with_extension(
        r#"{"op":"{{operation}}","data":{{json before}}}"#,
        mqtt_extension("drasi/{{query_name}}/deleted"),
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

### Config struct usage

```rust
use std::collections::HashMap;

use drasi_lib::reactions::common::TemplateSpec;
use drasi_reaction_mqtt::config::{
    MqttExtension, MqttProtocolVersion, MqttQoS, MqttReactionConfig, QueryConfig,
};
use drasi_reaction_mqtt::MqttReaction;

fn mqtt_extension(topic: &str) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos: MqttQoS::AtLeastOnce,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
        slashes_count: topic.matches('/').count(),
    }
}

let mut routes = HashMap::new();
routes.insert(
    "temperature-alerts".to_string(),
    QueryConfig {
        added: Some(TemplateSpec::with_extension(
            "{{json after}}",
            mqtt_extension("alerts/temperature/added"),
        )),
        updated: Some(TemplateSpec::with_extension(
            r#"{"before":{{json before}},"after":{{json after}}}"#,
            mqtt_extension("alerts/temperature/updated"),
        )),
        deleted: Some(TemplateSpec::with_extension(
            "{{json before}}",
            mqtt_extension("alerts/temperature/deleted"),
        )),
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

## `MqttReactionConfig`

| Field | Type | Required | Default | Description |
|---|---|---:|---|---|
| `url` | `String` | Yes | `mqtt://localhost:1883` in builder | Broker URL. Supported schemes: `mqtt`, `mqtts`. |
| `client_id` | `Option<String>` | No | `drasi-mqtt-{reaction_id}` | MQTT client ID. Set this when using persistent sessions. |
| `protocol_version` | `MqttProtocolVersion` | No | `V5` | MQTT protocol version: `V5` or `V3_1_1`. |
| `routes` | `HashMap<String, QueryConfig<MqttExtension>>` | No | `{}` | Query-specific templates. Keys must match subscribed query IDs. |
| `default_template` | `Option<QueryConfig<MqttExtension>>` | No | `None` | Fallback templates used when a query route or operation template is missing. |
| `identity_provider` | `Option<Box<dyn IdentityProvider>>` | No | `None` | Username/password credentials provider. |
| `tls` | `Option<MqttTlsConfig>` | For `mqtts` | `None` | TLS configuration. Must be `None` for `mqtt`. |
| `event_channel_capacity` | `usize` | No | `100` | Internal MQTT event channel capacity. Must be greater than `0`. |
| `max_inflight` | `Option<u16>` | No | rumqttc default | Maximum outgoing inflight QoS 1 messages. |
| `keep_alive` | `Option<u64>` | No | rumqttc default | Keep-alive interval in seconds. Must be at least `5` when set. |
| `clean_start` | `Option<bool>` | No | rumqttc default | Clean session/start flag. |
| `conn_timeout` | `Option<u64>` | No | rumqttc default | Connection timeout in milliseconds for MQTT v5. |
| `session_expiry_interval` | `Option<u32>` | No | `None` | MQTT v5 session expiry interval. Meaningful with `clean_start: false`. |

## `MqttExtension`

`MqttExtension` adds MQTT-specific fields to the shared
`TemplateSpec<MqttExtension>` type.

| Field | Type | Default | Description |
|---|---|---|---|
| `topic` | `String` | empty | MQTT topic template. Must render to a valid publish topic. |
| `qos` | `MqttQoS` | `AtLeastOnce` | `AtMostOnce` for QoS 0 or `AtLeastOnce` for QoS 1. |
| `retain` | `bool` | `false` | Sets the MQTT retain flag. |
| `empty_payload` | `bool` | `false` | Publishes a zero-byte payload instead of the rendered template. Useful for clearing retained messages. |
| `message_expiry_interval` | `Option<u32>` | `None` | MQTT v5 message expiry interval in seconds. |
| `slashes_count` | `usize` | `0` | Internal topic-template slash count used to reject unsafe rendered topics. Set to `topic.matches('/').count()` when constructing `MqttExtension` directly. |

## Routing

Routing follows the shared reaction template pattern:

1. Use `routes[query_id]` for the matching query and operation.
2. Fall back to `default_template` for the operation.
3. If no template is found, skip the diff and log a warning.

`QueryConfig<MqttExtension>` can configure separate templates for:

- `added`
- `updated`
- `deleted`

Aggregation diffs use the `updated` template and set `operation` to
`"AGGREGATION"`.

## Template Variables

Payload and topic templates use Handlebars. Templates can reference:

| Variable | ADD | UPDATE | DELETE | Description |
|---|---|---|---|---|
| `after` | Yes | Yes | No | New/current row state. |
| `before` | No | Yes | Yes | Previous row state. |
| `data` | No | Yes | No | Raw `data` field when present in update payloads. |
| `query_name` | Yes | Yes | Yes | Query ID that produced the diff. |
| `operation` | Yes | Yes | Yes | `ADD`, `UPDATE`, `DELETE`, or `AGGREGATION`. |
| `timestamp` | Yes | Yes | Yes | Query result timestamp as RFC3339. |

Helper:

- `{{json value}}` serializes a nested value as JSON.

If `template` is empty and `empty_payload` is `false`, the reaction publishes
the raw JSON value for the diff.

## Topic Safety

Configured and rendered topics are validated before publishing:

- Topics cannot be empty.
- Topics cannot contain MQTT wildcards `+` or `#`.
- Topics cannot contain null characters.
- Topics cannot contain empty levels such as `alerts//temperature`.
- Rendered template values cannot introduce additional `/` topic levels.
- Topic UTF-8 length cannot exceed the MQTT limit of 65,535 bytes.

## Broker URL and TLS

Transport is selected by the `url` scheme:

| Scheme | Transport | Default port |
|---|---|---:|
| `mqtt://` | TCP | `1883` |
| `mqtts://` | TLS over TCP | `8883` |

For `mqtts`, provide `MqttTlsConfig` with `with_tls(...)` or in
`MqttReactionConfig.tls`. Public-CA brokers can use `MqttTlsConfig` with
`ca: None` to load the system CA store. Custom CA bundles use PEM bytes in
`ca`.

`client_auth` is present in the type but is not supported in v1. If it is set,
startup validation fails.

## Authentication

Username/password authentication is configured with an identity provider:

```rust
use drasi_lib::identity::PasswordIdentityProvider;
use drasi_reaction_mqtt::MqttReaction;

let reaction = MqttReaction::builder("mqtt-reaction")
    .with_query("temperature-alerts")
    .with_url("mqtt://broker.example.com:1883")
    .with_identity_provider(Box::new(PasswordIdentityProvider::new("user", "pass")))
    .build()?;
```

Other credential types are rejected in this version.

## Builder Methods

| Method | Description |
|---|---|
| `with_query(query)` | Add one subscribed query ID. |
| `with_queries(queries)` | Replace all subscribed query IDs. |
| `with_url(url)` | Set broker URL. |
| `with_client_id(client_id)` | Set MQTT client ID. |
| `with_protocol_version(version)` | Set `MqttProtocolVersion::V5` or `V3_1_1`. |
| `with_route(query_name, route)` | Add a query-specific route. |
| `with_routes(routes)` | Replace all query routes. |
| `with_routes_extend(routes)` | Extend the route map. |
| `with_default_template(template)` | Set fallback operation templates. |
| `with_identity_provider(provider)` | Set credentials provider. |
| `with_tls(tls)` | Set TLS configuration. |
| `with_event_channel_capacity(capacity)` | Set MQTT event channel capacity. |
| `with_max_inflight(max_inflight)` | Set max outgoing inflight messages. |
| `with_keep_alive(seconds)` | Set keep-alive seconds. |
| `with_clean_start(clean_start)` | Set clean session/start behavior. |
| `with_conn_timeout(milliseconds)` | Set connection timeout. |
| `with_session_expiry_interval(seconds)` | Set MQTT v5 session expiry interval. |
| `with_priority_queue_capacity(capacity)` | Set reaction priority queue capacity. |
| `with_auto_start(auto_start)` | Enable or disable automatic startup. |
| `with_config(config)` | Apply a full `MqttReactionConfig`. |
| `build()` | Validate and build the `MqttReaction`. |

## Running Checks

Unit tests:

```bash
cargo test -p drasi-reaction-mqtt
```

Integration tests require Docker and a Mosquitto container:

```bash
cargo test -p drasi-reaction-mqtt --test integrations_tests -- --ignored --nocapture
```

## Limitations

- QoS 2 (`ExactlyOnce`) is not supported.
- mTLS client certificates are not supported in v1.
- Message expiry is defined in configuration but currently not applied to
  publish properties.
- The reaction publishes one MQTT message per diff.
