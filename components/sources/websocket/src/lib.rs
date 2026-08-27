#![allow(unexpected_cfgs)]
// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Generic outbound WebSocket source for Drasi.
//!
//! The source connects to one `ws://` or `wss://` endpoint, adds configured
//! upgrade headers, sends fixed JSON messages after each handshake, and maps
//! JSON text messages into Drasi graph changes with `drasi-source-mapping`.
//!
//! # Configuration
//!
//! [`WebSocketSourceConfig`] controls the endpoint, upgrade headers, initial
//! messages, mapping rules, message and channel bounds, and reconnect policy.
//! Cleartext `ws://` endpoints require an explicit `allow_insecure` opt-in.
//! `reconnect.delay_ms` is the initial exponential-backoff delay;
//! `reconnect.max_delay_ms` optionally caps it.
//!
//! # Data format
//!
//! Each text message must contain one JSON value. The selected payload is exposed
//! to mapping templates as `payload`; the complete message value is exposed as
//! `envelope`. For example:
//!
//! ```json
//! {
//!   "id": "sensor-1",
//!   "value": 42,
//!   "timestamp": 1770000000000
//! }
//! ```
//!
//! # Delivery
//!
//! The source is volatile and returns `supports_replay() == false`. A bounded
//! internal frame queue lets the socket process control frames during temporary
//! downstream backpressure. Socket reads stop when that queue fills. Shutdown
//! may abandon queued work or partially fan out an in-flight event if a
//! subscriber remains blocked beyond the bounded shutdown grace period.
//!
//! # Embedded use
//!
//! ```rust,ignore
//! use drasi_source_websocket::{
//!     ElementTemplate, ElementType, OperationType, SourceMapping,
//!     WebSocketSource, WebSocketSourceConfig,
//! };
//!
//! let source = WebSocketSource::new(
//!     "sensors",
//!     WebSocketSourceConfig {
//!         url: "wss://feed.example.com/events".to_string(),
//!         mappings: vec![SourceMapping {
//!             when: None,
//!             operation: Some(OperationType::Insert),
//!             operation_from: None,
//!             operation_map: None,
//!             element_type: ElementType::Node,
//!             effective_from: None,
//!             template: ElementTemplate {
//!                 id: "{{payload.id}}".to_string(),
//!                 labels: vec!["Sensor".to_string()],
//!                 properties: None,
//!                 from: None,
//!                 to: None,
//!             },
//!         }],
//!         ..Default::default()
//!     },
//! )?;
//! # Ok::<(), anyhow::Error>(())
//! ```

mod config;
pub mod descriptor;
mod mapping;
mod source;
mod transport;

pub use config::{HeaderConfig, ReconnectConfig, WebSocketSourceConfig};
pub use source::{WebSocketSource, WebSocketSourceBuilder};

pub use drasi_source_mapping::{
    EffectiveFromConfig, ElementTemplate, ElementType, MappingCondition, OperationType,
    SourceMapping, TimestampFormat,
};

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "websocket-source",
    core_version = "0.5.8",
    lib_version = "0.9.0",
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::WebSocketSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);

#[cfg(all(test, feature = "dynamic-plugin"))]
mod dynamic_plugin_tests {
    #[test]
    fn dynamic_plugin_metadata_exports_expected_versions() {
        let metadata = unsafe { &*super::drasi_plugin_metadata() };

        assert_eq!(unsafe { metadata.core_version.as_str() }, "0.5.8");
        assert_eq!(unsafe { metadata.lib_version.as_str() }, "0.9.0");
        assert_eq!(
            unsafe { metadata.plugin_version.as_str() },
            env!("CARGO_PKG_VERSION")
        );
    }
}
