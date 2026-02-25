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

//! Mock Source Plugin for drasi-lib.
//!
//! This plugin provides a mock data generator for testing and development purposes.
//! It generates synthetic data at configurable intervals, allowing you to test
//! query behavior and reaction processing without connecting to real data sources.
//!
//! # Data Types
//!
//! The mock source supports three data generation modes:
//!
//! ## Counter Mode (`data_type: DataType::Counter`)
//!
//! Generates sequentially numbered nodes with the label `Counter`. Each event
//! is always an INSERT with a unique element ID (`counter_1`, `counter_2`, etc.):
//!
//! ```text
//! Element {
//!     id: "counter_1",
//!     labels: ["Counter"],
//!     properties: {
//!         value: 1,              // Sequential integer starting at 1
//!         timestamp: "2024-01-15T10:30:00.123Z"  // RFC 3339 string
//!     },
//!     effective_from: 1705318200123  // Milliseconds since Unix epoch
//! }
//! ```
//!
//! ## Sensor Reading Mode (`data_type: DataType::SensorReading { sensor_count }`)
//!
//! Generates simulated IoT sensor readings with randomized temperature and humidity
//! values from a configurable number of sensors (default: 5). Each node has the
//! label `SensorReading`.
//!
//! **Key Behavior**: The first reading for each sensor generates an INSERT event;
//! subsequent readings for the same sensor generate UPDATE events. This simulates
//! real sensor behavior where devices are discovered once then continuously report.
//!
//! ```text
//! Element {
//!     id: "sensor_2",
//!     labels: ["SensorReading"],
//!     properties: {
//!         sensor_id: "sensor_2",
//!         temperature: 25.7,     // Random in range [20.0, 30.0)
//!         humidity: 48.3,        // Random in range [40.0, 60.0)
//!         timestamp: "2024-01-15T10:30:00.123Z"
//!     },
//!     effective_from: 1705318200123
//! }
//! ```
//!
//! ## Generic Mode (`data_type: DataType::Generic`) - Default
//!
//! Generates generic nodes with random values and the label `Generic`. Each event
//! is always an INSERT with a unique element ID:
//!
//! ```text
//! Element {
//!     id: "generic_1",
//!     labels: ["Generic"],
//!     properties: {
//!         value: 12345,          // Random i32
//!         message: "Generic mock data",
//!         timestamp: "2024-01-15T10:30:00.123Z"
//!     },
//!     effective_from: 1705318200123456789  // Nanoseconds since Unix epoch
//! }
//! ```
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `data_type` | [`DataType`] | `Generic` | Type of data to generate |
//! | `interval_ms` | `u64` | `5000` | Interval between events in milliseconds (must be > 0) |
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: mock
//! properties:
//!   data_type:
//!     type: sensor_reading
//!     sensor_count: 10
//!   interval_ms: 1000
//! ```
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use drasi_source_mock::{MockSource, MockSourceConfig, DataType};
//!
//! // Create configuration for sensor data at 1-second intervals
//! let config = MockSourceConfig {
//!     data_type: DataType::sensor_reading(10),
//!     interval_ms: 1000,
//! };
//!
//! // Create the source
//! let source = MockSource::new("my-mock", config)?;
//!
//! // Add to DrasiLib (ownership is transferred)
//! drasi.add_source(source).await?;
//! ```
//!
//! # Testing
//!
//! The mock source provides methods for unit testing without a full DrasiLib setup:
//!
//! - [`MockSource::inject_event()`] - Inject custom [`SourceChange`](drasi_core::models::SourceChange)
//!   events (INSERT, UPDATE, DELETE) for deterministic testing scenarios
//! - [`MockSource::test_subscribe()`] - Subscribe to receive events directly from the source,
//!   bypassing DrasiLib's subscription mechanism

mod config;
mod conversion;
pub mod descriptor;
mod mock;
mod time;

#[cfg(test)]
mod tests;

pub use config::{DataType, MockSourceConfig};
pub use mock::{MockSource, MockSourceBuilder};

/// Dynamic plugin entry point.
///
/// # Safety
/// The caller must ensure this is only called once and takes ownership of the
/// returned pointer via `Box::from_raw`.
#[no_mangle]
pub extern "C" fn drasi_source_mock_plugin_init() -> *mut drasi_plugin_sdk::PluginRegistration {
    let registration = drasi_plugin_sdk::PluginRegistration::new()
        .with_source(Box::new(descriptor::MockSourceDescriptor));
    Box::into_raw(Box::new(registration))
}
