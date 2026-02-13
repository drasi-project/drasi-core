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

//! Configuration types for the mock source.
//!
//! This module defines the configuration options that control how the mock source
//! generates synthetic data.

use serde::{Deserialize, Serialize};
use std::fmt;

fn default_sensor_count() -> u32 {
    5
}

/// Specifies the type of synthetic data to generate.
///
/// Each variant produces elements with different schemas and behaviors.
/// See the [crate-level documentation](crate) for detailed examples of each type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataType {
    /// Generates sequential counter values (1, 2, 3, ...).
    ///
    /// Each event is an INSERT with a unique element ID (`counter_1`, `counter_2`, etc.).
    /// Properties include `value` (the counter) and `timestamp`.
    Counter,

    /// Generates simulated IoT sensor readings.
    ///
    /// Produces temperature (20.0-30.0) and humidity (40.0-60.0) values from
    /// a pool of simulated sensors. The first reading for each sensor is an INSERT;
    /// subsequent readings for the same sensor are UPDATEs.
    SensorReading {
        /// Number of distinct sensors to simulate. Must be at least 1.
        /// Sensor IDs will be in range `[0, sensor_count)`.
        #[serde(default = "default_sensor_count")]
        sensor_count: u32,
    },

    /// Generates generic data with random values (default).
    ///
    /// Each event is an INSERT with a unique element ID (`generic_1`, `generic_2`, etc.).
    /// Properties include `value` (random i32), `message`, and `timestamp`.
    #[default]
    Generic,
}

impl DataType {
    /// Creates a [`SensorReading`](DataType::SensorReading) data type with the specified sensor count.
    ///
    /// # Arguments
    ///
    /// * `sensor_count` - Number of distinct sensors to simulate (must be ≥ 1)
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_source_mock::DataType;
    ///
    /// let data_type = DataType::sensor_reading(10);
    /// assert_eq!(data_type.sensor_count(), Some(10));
    /// ```
    pub fn sensor_reading(sensor_count: u32) -> Self {
        DataType::SensorReading { sensor_count }
    }

    /// Returns the sensor count if this is a [`SensorReading`](DataType::SensorReading) variant.
    ///
    /// # Returns
    ///
    /// - `Some(count)` for `SensorReading` variants
    /// - `None` for `Counter` and `Generic` variants
    pub fn sensor_count(&self) -> Option<u32> {
        match self {
            DataType::SensorReading { sensor_count } => Some(*sensor_count),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Counter => write!(f, "counter"),
            DataType::SensorReading { .. } => write!(f, "sensor_reading"),
            DataType::Generic => write!(f, "generic"),
        }
    }
}

/// Configuration for a [`MockSource`](crate::MockSource) instance.
///
/// Controls what type of data is generated and how frequently.
///
/// # Example
///
/// ```rust
/// use drasi_source_mock::{MockSourceConfig, DataType};
///
/// let config = MockSourceConfig {
///     data_type: DataType::sensor_reading(10),
///     interval_ms: 1000,
/// };
/// ```
///
/// # Serialization
///
/// This type supports serde serialization for configuration files:
///
/// ```yaml
/// data_type:
///   type: sensor_reading
///   sensor_count: 10
/// interval_ms: 1000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockSourceConfig {
    /// The type of synthetic data to generate.
    ///
    /// Defaults to [`DataType::Generic`] if not specified.
    #[serde(default)]
    pub data_type: DataType,

    /// Interval between generated events in milliseconds.
    ///
    /// Must be greater than 0. Defaults to 5000 (5 seconds) if not specified.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_interval_ms() -> u64 {
    5000
}

impl Default for MockSourceConfig {
    fn default() -> Self {
        Self {
            data_type: DataType::default(),
            interval_ms: default_interval_ms(),
        }
    }
}

impl MockSourceConfig {
    /// Validates this configuration.
    ///
    /// # Errors
    ///
    /// Returns [`anyhow::Error`] if:
    /// - `interval_ms` is 0 (would cause a spin loop with no delay between events)
    /// - `data_type` is `SensorReading` with `sensor_count` of 0 (must have at least one sensor)
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_source_mock::{MockSourceConfig, DataType};
    ///
    /// let valid_config = MockSourceConfig {
    ///     data_type: DataType::Counter,
    ///     interval_ms: 1000,
    /// };
    /// assert!(valid_config.validate().is_ok());
    ///
    /// let invalid_config = MockSourceConfig {
    ///     data_type: DataType::Counter,
    ///     interval_ms: 0,  // Invalid!
    /// };
    /// assert!(invalid_config.validate().is_err());
    /// ```
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.interval_ms == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: interval_ms cannot be 0. \
                 Please specify a positive interval in milliseconds (minimum 1)"
            ));
        }

        if let DataType::SensorReading { sensor_count } = &self.data_type {
            if *sensor_count == 0 {
                return Err(anyhow::anyhow!(
                    "Validation error: sensor_count cannot be 0. \
                     Please specify at least 1 sensor"
                ));
            }
        }

        Ok(())
    }
}
