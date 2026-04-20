# MQTT Source

A Drasi source crate that subscribes to MQTT topics and converts broker messages into Drasi source changes.

## Overview

The MQTT source connects to an MQTT broker, subscribes to one or more topics, maps topic and payload data into graph elements, and forwards them into Drasi with optional configurable adaptive batching.

**Key capabilities**:

- MQTT topic subscription with per-topic QoS.
- Topic-to-graph mapping using topic pattern placeholders.
- Two payload mapping modes for structured or raw payload ingestion.
- Optional TCP or TLS transport.
- Static credentials or identity-provider-based authentication
- Adaptive batching for higher-throughput streams
- Optional bootstrap provider when building the source programmatically

## Configuration

### Builder Pattern

```rust
use drasi_source_mqtt::{MqttQoS, MqttSource};
use drasi_source_mqtt::config::{
    InjectId, MappingEntity, MappingMode, MappingNode, MappingProperties, TopicMapping,
};
use std::collections::HashMap;

let source = MqttSource::builder("mqtt-source")
    .with_host("localhost")
    .with_port(1883)
    .with_topic_config("building/+/+/+", MqttQoS::ONE)
    .with_topic_mappings(vec![TopicMapping {
        pattern: "building/{floor}/{room}/{device}".to_string(),
        entity: MappingEntity {
            label: "DEVICE".to_string(),
            id: "{room}:{device}".to_string(),
        },
        properties: MappingProperties {
            mode: MappingMode::PayloadAsField,
            field_name: Some("payload".to_string()),
            inject_id: Some(InjectId::True),
            inject: vec![HashMap::from([("room".to_string(), "{room}".to_string())])],
        },
        nodes: vec![MappingNode {
            label: "ROOM".to_string(),
            id: "{room}".to_string(),
        }],
        relations: vec![],
    }])
    .with_adaptive_enabled(true)
    .build()?;
```

  ### Builder Options

  The builder supports all of the options below.

  | Method | Purpose |
  |------|---------|
  | `with_host(host)` | MQTT broker hostname |
  | `with_port(port)` | MQTT broker port |
  | `with_topic(topic)` | Replace topic list with one topic using default QoS |
  | `with_topic_config(topic, qos)` | Add one topic subscription with explicit QoS |
  | `with_topics(topics)` | Set full topic subscription list |
  | `with_topic_mappings(topic_mappings)` | Set topic-to-graph mappings |
  | `with_timeout_ms(timeout)` | Set MQTT v5 connection timeout |
  | `with_channel_capacity(capacity)` | Event channel capacity |
  | `with_transport(transport)` | TCP or TLS transport configuration |
  | `with_request_channel_capacity(capacity)` | Internal request channel capacity |
  | `with_max_inflight(max_inflight)` | Max in-flight outgoing messages |
  | `with_keep_alive_secs(keep_alive)` | Keep-alive interval in seconds |
  | `with_clean_start(clean_start)` | Clean start/session behavior |
  | `with_max_incoming_packet_size(size)` | MQTT v3 max incoming packet size |
  | `with_max_outgoing_packet_size(size)` | MQTT v3 max outgoing packet size |
  | `with_connect_properties(props)` | MQTT v5 connect properties |
  | `with_subscribe_properties(props)` | MQTT v5 subscribe properties |
  | `with_identity_provider(provider)` | Identity-provider-based auth (takes precedence over static credentials and TLS-based certs and keys) |
  | `with_username(username)` | Static MQTT username |
  | `with_password(password)` | Static MQTT password |
  | `with_adaptive_max_batch_size(size)` | Adaptive batching max batch size |
  | `with_adaptive_min_batch_size(size)` | Adaptive batching min batch size |
  | `with_adaptive_max_wait_ms(ms)` | Adaptive batching max wait |
  | `with_adaptive_min_wait_ms(ms)` | Adaptive batching min wait |
  | `with_adaptive_window_secs(secs)` | Adaptive throughput window |
  | `with_adaptive_enabled(enabled)` | Toggle adaptive batching |
  | `with_dispatch_mode(mode)` | Source dispatch mode |
  | `with_dispatch_buffer_capacity(capacity)` | Dispatch buffer capacity |
  | `with_bootstrap_provider(provider)` | Optional bootstrap provider |
  | `with_auto_start(auto_start)` | Auto-start behavior |
  | `with_config(config)` | Load values from `MqttSourceConfig` |

### YAML Configuration

```yaml
sources:
  - id: mqtt-source
    source_type: mqtt
    properties:
      host: localhost
      port: 1883
      topics:
        - topic: building/+/+/+
          qos: 1
      topic_mappings:
        - pattern: building/{floor}/{room}/{device}
          entity:
            label: DEVICE
            id: "{room}:{device}"
          properties:
            mode: payload_as_field
            field_name: payload
            inject_id: true
            inject:
              - room: "{room}"
              - floor: "{floor}"
          nodes:
            - label: ROOM
              id: "{room}"
      adaptive_enabled: true
      adaptive_max_batch_size: 1000
      adaptive_min_batch_size: 10
      adaptive_max_wait_ms: 100
      adaptive_min_wait_ms: 1
      adaptive_window_secs: 5
```

### Core Options

| Name | Description | Default |
|------|-------------|---------|
| `host` | MQTT broker hostname | `localhost` |
| `port` | MQTT broker port | `1883` |
| `topics` | Subscriptions with per-topic QoS | none |
| `topic_mappings` | Topic-to-graph mapping rules | none |
| `event_channel_capacity` | Event channel capacity | `20` |
| `transport` | `tcp` or `tls` transport settings | `tcp` |
| `request_channel_capacity` | Internal request channel capacity | none |
| `max_inflight` | Max in-flight outgoing messages | none |
| `keep_alive` | Keep-alive interval in seconds | none |
| `clean_start` | MQTT clean session/start behavior | none |
| `max_incoming_packet_size` | MQTT v3 max incoming packet size | none |
| `max_outgoing_packet_size` | MQTT v3 max outgoing packet size | none |
| `conn_timeout` | MQTT v5 connection timeout in ms | `5000` |
| `connect_properties` | MQTT v5 connect properties | none |
| `subscribe_properties` | MQTT v5 subscribe properties | none |
| `identity_provider` | Identity-provider authentication | none |
| `username` / `password` | Static MQTT credentials | none |
| `adaptive_enabled` | Enable adaptive batching | crate default |
| `adaptive_max_batch_size` | Max events per batch | crate default |
| `adaptive_min_batch_size` | Min events per batch | crate default |
| `adaptive_max_wait_ms` | Max batch wait time | crate default |
| `adaptive_min_wait_ms` | Min batch wait time | crate default |
| `adaptive_window_secs` | Throughput measurement window | crate default |

Validation notes:
- `keep_alive` must be at least `5` seconds when set.

## Mapping Modes

Each `topic_mapping` uses a topic `pattern` with placeholders such as `building/{floor}/{room}/{device}`. Those placeholders can then be reused in IDs and injected properties.

The source supports two property mapping modes:

- `payload_as_field`: stores the incoming payload under a single property. This mode requires `field_name`.
- `payload_spread`: spreads object payload fields into graph properties. This mode must not set `field_name`.

Additional mapping behavior:

- `inject`: copies topic placeholder values into properties
- `inject_id: true`: includes the mapped entity ID in properties
- `nodes` and `relations`: defines related graph elements created from the same topic match

Example:

```yaml
topic_mappings:
  - pattern: sensors/{site}/{device}
    entity:
      label: SENSOR
      id: "{site}:{device}"
    properties:
      mode: payload_spread
      inject:
        - site: "{site}"
        - device: "{device}"
    nodes:
      - label: SITE
        id: "{site}"
```

## TLS

Set `transport.mode: tls` to enable TLS.

TLS mode should use port 8883 as industry standard.

The source supports either inline certificate material or file-based paths:

- `ca` or `ca_path` for the CA certificate
- `client_auth` for inline client certificate and key
- `client_cert_path` plus `client_key_path` for file-based client authentication
- optional `alpn`

Rules enforced by configuration validation:

- specify either `ca` or `ca_path`, not both
- specify either inline `client_auth` or file-based client cert/key paths, not both
- if using file-based client auth, both `client_cert_path` and `client_key_path` must be set

Example:

```yaml
transport:
  mode: tls
  config:
    ca_path: /etc/drasi/certs/ca.pem
    client_cert_path: /etc/drasi/certs/client.pem
    client_key_path: /etc/drasi/certs/client.key
```

## Developer Notes

### Testing
```bash
# Unit tests
cargo test -p drasi-source-mqtt

# Integration tests
cargo test -p drasi-source-mqtt --test integration_tests -- --ignored --nocapture
```

## Usage Examples

### Raw Payload Ingestion

```yaml
topic_mappings:
  - pattern: raw/{device}
    entity:
      label: MESSAGE
      id: "{device}"
    properties:
      mode: payload_as_field
      field_name: payload
```

### Structured JSON Payload Ingestion

```yaml
topic_mappings:
  - pattern: metrics/{site}/{device}
    entity:
      label: DEVICE
      id: "{site}:{device}"
    properties:
      mode: payload_spread
      inject:
        - site: "{site}"
        - device: "{device}"
```

### Multiple options together
```yaml
host: "localhost"
port: 8883
topics:
  - topic: "sensors/temperature"
    qos: 1
  - topic: "building/+/+/+"
    qos: 0
topic_mappings:
  - pattern: "sensors/{type}"
    entity:
      label: "readings"
      id: "symbol"
    properties:
      mode: payload_as_field
      field_name: "{type}"
      inject:
      - type: "{type}"
  - pattern: "building/{floor}/{room}/{device}"
    entity:
      label: "DEVICE"
      id: "{room}:{device}"
    properties:
      mode: payload_as_field
      field_name: "reading"
      inject_id: true
      inject:
      - room: "{room}"
      - floor: "{floor}"
      - device: "{device}"
    nodes:
      - label: "FLOOR"
        id: "{floor}"
      - label: "ROOM"
        id: "{room}"
    relations:
      - label: "LOCATED_IN_FLOOR"
        from: "ROOM"
        to: "FLOOR"
        id: "{room}_located_in_{floor}"
      - label: "LOCATED_IN_ROOM"
        from: "DEVICE"
        to: "ROOM"
        id: "{device}_located_in_{room}"
event_channel_capacity: 20
keep_alive: 5
conn_timeout: 5
transport:
    mode: tls
    config:
        ca_path: "/var/certs-drasi/ca.crt"
        client_cert_path: "/var/certs-drasi/client.crt"
        client_key_path: "/var/certs-drasi/client.key"
```

## License

Copyright 2026 The Drasi Authors.

Licensed under the Apache License, Version 2.0. See LICENSE file for details.
