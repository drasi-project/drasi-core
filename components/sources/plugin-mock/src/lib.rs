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

//! Mock Source Plugin for drasi-lib
//!
//! This plugin provides a mock data generator for testing and development purposes.
//! It generates synthetic data at configurable intervals, allowing you to test
//! query behavior and reaction processing without connecting to real data sources.
//!
//! # Data Types
//!
//! The mock source supports three data generation modes:
//!
//! ## Counter Mode (`data_type: "counter"`)
//!
//! Generates sequentially numbered nodes with the label `Counter`:
//!
//! ```json
//! {
//!     "id": "counter_1",
//!     "labels": ["Counter"],
//!     "properties": {
//!         "value": 1,
//!         "timestamp": "2024-01-15T10:30:00Z"
//!     }
//! }
//! ```
//!
//! ## Sensor Mode (`data_type: "sensor"`)
//!
//! Generates simulated IoT sensor readings with randomized temperature and humidity
//! values from 5 different sensors. Each node has the label `SensorReading`:
//!
//! ```json
//! {
//!     "id": "reading_2_42",
//!     "labels": ["SensorReading"],
//!     "properties": {
//!         "sensor_id": "sensor_2",
//!         "temperature": 25.7,
//!         "humidity": 48.3,
//!         "timestamp": "2024-01-15T10:30:00Z"
//!     }
//! }
//! ```
//!
//! ## Generic Mode (`data_type: "generic"`) - Default
//!
//! Generates generic nodes with random values and the label `Generic`:
//!
//! ```json
//! {
//!     "id": "generic_1",
//!     "labels": ["Generic"],
//!     "properties": {
//!         "value": 12345,
//!         "message": "Generic mock data",
//!         "timestamp": "2024-01-15T10:30:00Z"
//!     }
//! }
//! ```
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `data_type` | string | `"generic"` | Type of data to generate: `counter`, `sensor`, or `generic` |
//! | `interval_ms` | u64 | `5000` | Interval between data generation in milliseconds |
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: mock
//! properties:
//!   data_type: sensor
//!   interval_ms: 1000
//! ```
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use drasi_plugin_mock::{MockSource, MockSourceConfig};
//! use std::sync::Arc;
//!
//! // Create configuration for sensor data at 1-second intervals
//! let config = MockSourceConfig {
//!     data_type: "sensor".to_string(),
//!     interval_ms: 1000,
//! };
//!
//! // Create and add to DrasiLib
//! let source = Arc::new(MockSource::new("my-mock", config)?);
//! drasi.add_source(source).await?;
//! ```
//!
//! # Testing
//!
//! The mock source provides methods for testing:
//!
//! - `inject_event()` - Manually inject specific events for testing
//! - `test_subscribe()` - Create a test subscription to receive events

mod config;
mod mock;

#[cfg(test)]
mod tests;

pub use config::MockSourceConfig;
pub use mock::MockSource;
