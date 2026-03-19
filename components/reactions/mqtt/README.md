# MQTT Reaction

The MQTT reaction publishes Drasi query result changes to an MQTT broker. It is intended for integrations where continuous query output needs to be delivered to IoT devices, event consumers, workflow engines, or downstream services that already speak MQTT.

## Overview

This component subscribes to one or more Drasi queries and publishes messages when query rows are added, updated, or deleted.

It supports:

- broker configuration through `MqttReactionConfig` or `MqttReactionBuilder`
- per-query routing with separate MQTT topics for add, update, and delete operations
- MQTT authentication with username and password
- TCP transport mode (TLS not yet)

## Builder Example

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

## Configuration Example

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
	pending_throttle: 100,
	connection_timeout: 5,
	max_packet_size: 2 * 1024,
	max_inflight: 30,
	default_topic: "stocks/updates".to_string(),
	query_configs: Default::default(),
};

let reaction = MqttReaction::new(
	"mqtt-reaction",
	vec!["stock-prices".to_string()],
	config,
);
```

## Query Routing

Use `query_configs` when different queries or result operations should publish to different topics or use different payload templates. Each query can define separate MQTT call specifications for:

- `added`
- `updated`
- `deleted`

If no custom template body is provided, the reaction publishes the JSON representation of the result payload.

## Notes

- `keep_alive` and `connection_timeout` are expressed in seconds.
- `default_topic` is available as the base topic for routing strategies, but operation-specific routing is defined through `query_configs`.
- Runtime wiring is handled when the reaction is added to Drasi through the library builder.