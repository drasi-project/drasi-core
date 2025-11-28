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
//! This plugin provides a mock data generator for testing and development.
//!
//! ## Instance-based Usage
//!
//! ```rust,ignore
//! use drasi_plugin_mock::{MockSource, MockSourceConfig};
//! use drasi_lib::channels::ComponentEventSender;
//! use std::sync::Arc;
//!
//! // Create configuration
//! let config = MockSourceConfig {
//!     data_type: "counter".to_string(),
//!     interval_ms: 1000,
//! };
//!
//! // Create instance and add to DrasiLib
//! let source = Arc::new(MockSource::new("my-mock", config, event_tx)?);
//! drasi.add_source(source).await?;
//! ```

mod config;
mod mock;

#[cfg(test)]
mod tests;

pub use config::MockSourceConfig;
pub use mock::MockSource;
