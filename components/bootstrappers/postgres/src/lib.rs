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

//! PostgreSQL bootstrap plugin for Drasi
//!
//! This plugin provides the PostgreSQL bootstrap provider implementation following
//! the instance-based plugin architecture.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_postgres::{PostgresBootstrapProvider, PostgresSourceConfig};
//! use drasi_lib::config::common::SslMode;
//!
//! // Create the bootstrap provider with typed configuration
//! let config = PostgresSourceConfig {
//!     host: "localhost".to_string(),
//!     port: 5432,
//!     database: "mydb".to_string(),
//!     user: "user".to_string(),
//!     password: "password".to_string(),
//!     tables: vec!["users".to_string()],
//!     slot_name: "drasi_slot".to_string(),
//!     publication_name: "drasi_pub".to_string(),
//!     ssl_mode: SslMode::Disable,
//!     table_keys: vec![],
//! };
//!
//! let provider = PostgresBootstrapProvider::new(config);
//! ```

pub mod postgres;

pub use postgres::{PostgresBootstrapProvider, PostgresSourceConfig};
