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

//! Configuration for the mock source.
//!
//! The mock source generates synthetic data for testing and development purposes.

use serde::{Deserialize, Serialize};
use std::fmt;

fn default_sensor_count() -> u32 {
    5
}

/// Type of data to generate from the mock source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataType {
    /// Sequential counter values
    Counter,
    /// Simulated sensor readings with temperature and humidity
    SensorReading {
        /// Number of sensors to simulate
        #[serde(default = "default_sensor_count")]
        sensor_count: u32,
    },
    /// Generic random data (default)
    #[default]
    Generic,
}

impl DataType {
    /// Create a SensorReading data type with the specified number of sensors.
    pub fn sensor_reading(sensor_count: u32) -> Self {
        DataType::SensorReading { sensor_count }
    }

    /// Get the sensor count if this is a SensorReading data type.
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

/// Mock source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockSourceConfig {
    /// Type of data to generate
    #[serde(default)]
    pub data_type: DataType,

    /// Interval between data generation in milliseconds
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
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Interval is 0 (would cause continuous generation without pause)
    /// - Sensor count is 0 (must have at least one sensor) for SensorReading mode
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
