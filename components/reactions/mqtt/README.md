# MQTT Reaction

The MQTT reaction publishes Drasi continuous query changes to an MQTT broker. It is designed for IoT and event-driven integrations where query diffs (ADD/UPDATE/DELETE) should be emitted to MQTT topics in near real time.

## Overview

This component subscribes to one or more Drasi queries and publishes each result diff as an MQTT message.

Supported capabilities:

- Builder-based setup (`MqttReactionBuilder`) and struct-based setup (`MqttReactionConfig`)
- Per-query and per-operation routing (`added`, `updated`, `deleted`)
- Topic/body templating via Handlebars
- QoS and retain controls per call
- Username/password authentication
- TCP and TLS transport modes
- Optional adaptive batching for higher throughput workloads

## Quick IoT Smoke Test

If you only want to verify MQTT publishing quickly:

1. Run a local broker (for example, Mosquitto) on `localhost:1883`.
2. Subscribe with a topic wildcard, for example `#`.
3. Configure this reaction with one query and `default_topic`.
4. Trigger data changes that produce query diffs.
5. Confirm messages appear in your MQTT subscriber.

Minimal reaction setup:

```rust,ignore
use drasi_reaction_mqtt::MqttReaction;

let reaction = MqttReaction::builder("iot-mqtt")
	.with_query("temperature-alerts")
	.with_broker_addr("localhost")
	.with_port(1883)
	.with_default_topic("iot/alerts")
	.build()?;
```

When no explicit `query_configs` route exists, the reaction falls back to the default topic route for ADD/UPDATE/DELETE.

## Configuration Approaches

### Builder Pattern (Recommended)

```rust,ignore
use drasi_reaction_mqtt::{MqttReaction, config::MqttAuthMode};

let reaction = MqttReaction::builder("mqtt-reaction")
	.with_query("stock-prices")
	.with_broker_addr("mqtt.example.com")
	.with_port(1883)
	.with_default_topic("stocks/updates")
	.with_keep_alive(60)
	.with_auth_mode(MqttAuthMode::UsernamePassword {
		username: "user".to_string(),
		password: "pass".to_string(),
	})
	.build()?;
```

### Config Struct Approach

```rust,ignore
use drasi_reaction_mqtt::{MqttReaction, MqttReactionConfig};
use drasi_reaction_mqtt::config::{MqttAuthMode, MqttTransportMode};

let config = MqttReactionConfig {
	broker_addr: "mqtt.example.com".to_string(),
	port: 1883,
	transport_mode: MqttTransportMode::TCP,
	keep_alive: 60,
	clean_session: true,
	auth_mode: MqttAuthMode::UsernamePassword {
		username: "user".to_string(),
		password: "pass".to_string(),
	},
	request_channel_capacity: 20,
	event_channel_capacity: 40,
	pending_throttle: 0,
	connection_timeout: 30,
	max_packet_size: 10 * 1024,
	max_inflight: 100,
	default_topic: "stocks/updates".to_string(),
	query_configs: Default::default(),
	adaptive: None,
};

let reaction = MqttReaction::new(
	"mqtt-reaction",
	vec!["stock-prices".to_string()],
	config,
);
```

## Builder API Reference

`MqttReaction::builder(id)` returns `MqttReactionBuilder`.

| Method | Purpose |
|------|------|
| `with_query(query)` | Add one subscribed query ID. |
| `with_queries(queries)` | Replace all subscribed query IDs. |
| `with_broker_addr(addr)` | Broker host or IP. |
| `with_port(port)` | Broker port. |
| `with_transport_mode(mode)` | Use `MqttTransportMode::TCP` or `MqttTransportMode::TLS { .. }`. |
| `with_keep_alive(seconds)` | MQTT keep-alive interval. |
| `with_clean_session(clean)` | MQTT clean start/session behavior. |
| `with_auth_mode(auth)` | `MqttAuthMode::None` or `UsernamePassword`. |
| `with_request_channel_capacity(cap)` | Outgoing request channel size. |
| `with_event_channel_capacity(cap)` | MQTT event loop channel size, used in eventloop initialization. |
| `with_pending_throttle(throttle)` | Delay between retransmits (`Duration::from_micros`). |
| `with_connection_timeout(timeout)` | Connection timeout seconds. |
| `with_max_packet_size(bytes)` | Max incoming packet size fallback. |
| `with_max_inflight(count)` | Upper bound for in-flight outgoing publishes. |
| `with_default_topic(topic)` | Default topic base for fallback routing. |
| `with_query_config(query_name, config)` | Set route for a single query. (`*` to select all queries, default if a specific query is not found.). |
| `with_query_configs(map)` | Replace route map. |
| `with_query_configs_extend(iter)` | Extend route map. |
| `with_priority_queue_capacity(cap)` | Override reaction priority queue capacity. |
| `with_auto_start(bool)` | Auto-start when added to Drasi runtime. |
| `with_adaptive_config(cfg)` | Provide full adaptive batching config. |
| `with_min_adaptive_batch_size(size)` | Adaptive min batch size helper. |
| `with_max_adaptive_batch_size(size)` | Adaptive max batch size helper. |
| `with_adaptive_window_size(size)` | Adaptive throughput window size helper. |
| `with_adaptive_batch_timeout_ms(ms)` | Adaptive batch timeout helper. |
| `with_config(config)` | Apply full `MqttReactionConfig` at once. |
| `build()` | Build `MqttReaction` (`anyhow::Result<MqttReaction>`). |

## `MqttReactionConfig` Reference

Defaults are taken from `config.rs`.

| Field | Type | Default | Notes |
|------|------|------|------|
| `broker_addr` | `String` | `"localhost"` | MQTT broker host. |
| `port` | `u16` | `1883` | MQTT broker port. |
| `transport_mode` | `MqttTransportMode` | `TCP` | `TCP` or `TLS { ca, alpn, client_auth }`. |
| `keep_alive` | `u64` | `60` | Keep-alive in seconds. |
| `clean_session` | `bool` | `true` | Clean session/start flag. |
| `auth_mode` | `MqttAuthMode` | `None` | None or username/password. |
| `request_channel_capacity` | `usize` | `10` | Publish request channel capacity. |
| `event_channel_capacity` | `usize` | `20` | Event loop queue capacity. |
| `pending_throttle` | `u64` | `0` | Applied as microseconds. |
| `connection_timeout` | `u64` | `30` | Connection timeout in seconds. |
| `max_packet_size` | `u32` | `10240` | Incoming packet size fallback. |
| `max_inflight` | `u16` | `100` | Max inflight outgoing messages. |
| `default_topic` | `String` | `""` | Used by fallback route generation. |
| `query_configs` | `HashMap<String, MqttQueryConfig>` | `{}` | Per-query routing map. |
| `adaptive` | `Option<AdaptiveBatchConfig>` | `None` | Enables adaptive batching when set. |

## Routing and Payloads

`query_configs` controls how each query publishes. For every query, `MqttQueryConfig` can specify:

- `added: Option<MqttCallSpec>`
- `updated: Option<MqttCallSpec>`
- `deleted: Option<MqttCallSpec>`

`MqttCallSpec` fields:

- `topic: String` (required)
- `body: String` (optional Handlebars template; empty means default JSON payload)
- `retain: RetainPolicy` (`Retain` or `NoRetain`)
- `qos: QualityOfService` (`AtMostOnce`, `AtLeastOnce`, `ExactlyOnce`)

### Route Resolution Order

When processing a query result, routing is resolved in this order:

1. Exact query ID match in `query_configs`
2. Wildcard key `"*"`
3. Last segment of dotted query ID (for example `source.alerts` -> `alerts`)
4. Fallback default route using `default_topic` for all operations

### Default Payload Behavior

If `call_spec.body` is empty, the reaction publishes serialized JSON of the operation payload.

### Template Context Variables

Handlebars templates can use:

- `query_name`: query ID string
- `operation`: `ADD`, `UPDATE`, `DELETE`
- `after`: ADD data, UPDATE after state
- `before`: UPDATE before state, DELETE data
- `data`: available for UPDATE payload shape

Helper:

- `{{json value}}` serializes a value as JSON

## Example: Per-Operation Topics and Bodies

```rust,ignore
use std::collections::HashMap;
use drasi_reaction_mqtt::{MqttReaction, config::{
	MqttCallSpec, MqttQueryConfig, QualityOfService, RetainPolicy
}};

let sensor_query = MqttQueryConfig {
	added: Some(MqttCallSpec {
		topic: "iot/sensors/added".to_string(),
		body: r#"{"op":"{{operation}}","query":"{{query_name}}","payload":{{json after}}}"#.to_string(),
		retain: RetainPolicy::NoRetain,
		qos: QualityOfService::AtLeastOnce,
	}),
	updated: Some(MqttCallSpec {
		topic: "iot/sensors/updated".to_string(),
		body: r#"{"before":{{json before}},"after":{{json after}}}"#.to_string(),
		retain: RetainPolicy::NoRetain,
		qos: QualityOfService::AtLeastOnce,
	}),
	deleted: Some(MqttCallSpec {
		topic: "iot/sensors/deleted".to_string(),
		body: r#"{"op":"{{operation}}","payload":{{json before}}}"#.to_string(),
		retain: RetainPolicy::NoRetain,
		qos: QualityOfService::AtLeastOnce,
	}),
};

let reaction = MqttReaction::builder("iot-routes")
	.with_query("sensor-alerts")
	.with_query_config("sensor-alerts", sensor_query)
	.build()?;
```

## Authentication and Transport

### Authentication

- `MqttAuthMode::None`
- `MqttAuthMode::UsernamePassword { username, password }`

### Transport

- `MqttTransportMode::TCP`
- `MqttTransportMode::TLS { ca, alpn, client_auth }`

TLS mode expects raw certificate/key bytes in config (`Vec<u8>` fields), which is convenient for loading from files or secrets before constructing the reaction.

## Adaptive Batching

If `adaptive` is configured, the reaction switches from one-by-one processing to adaptive batching. The batcher adjusts batch size and wait times according to observed throughput to balance latency and throughput.

High-level behavior:

- low traffic: smaller batches, lower wait
- high traffic: larger batches, longer wait

Builder helpers:

- `with_min_adaptive_batch_size`
- `with_max_adaptive_batch_size`
- `with_adaptive_window_size`
- `with_adaptive_batch_timeout_ms`

## Runtime and Lifecycle Notes

- Add the reaction to Drasi runtime; runtime wiring (channels/context) is handled there.
- `auto_start` defaults to `true` and can be disabled via builder.
- `priority_queue_capacity` can be tuned for high-volume query diff streams.

## Troubleshooting

- No messages published:
  Check broker address/port, query subscription IDs, and `query_configs` matching keys.
- Messages published to unexpected topic:
  Verify route resolution order and `default_topic` fallback behavior.
- Template render errors:
  Validate Handlebars syntax and variable names (`before`, `after`, `operation`, `query_name`).
- Connection instability:
  Tune `keep_alive`, `connection_timeout`, `max_inflight`, and channel capacities.