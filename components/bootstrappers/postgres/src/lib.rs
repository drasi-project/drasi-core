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
//! use drasi_bootstrap_postgres::PostgresBootstrapProvider;
//!
//! // Using the builder
//! let provider = PostgresBootstrapProvider::builder()
//!     .with_host("localhost")
//!     .with_port(5432)
//!     .with_database("mydb")
//!     .with_user("user")
//!     .with_password("password")
//!     .with_tables(vec!["users".to_string()])
//!     .build();
//!
//! // Or using configuration
//! use drasi_bootstrap_postgres::{PostgresBootstrapConfig, SslMode};
//!
//! let config = PostgresBootstrapConfig {
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
//! let provider = PostgresBootstrapProvider::new(config);
//! ```

pub mod config;
pub mod descriptor;
pub mod postgres;

pub use config::{PostgresBootstrapConfig, SslMode, TableKeyConfig};
pub use postgres::{PostgresBootstrapProvider, PostgresBootstrapProviderBuilder};

/// Dynamic plugin entry point.
///
/// # Safety
/// The caller must ensure this is only called once and takes ownership of the
/// returned pointer via `Box::from_raw`.
#[no_mangle]
pub extern "C" fn drasi_bootstrap_postgres_plugin_init() -> *mut drasi_plugin_sdk::PluginRegistration {
    let registration = drasi_plugin_sdk::PluginRegistration::new()
        .with_bootstrapper(Box::new(descriptor::PostgresBootstrapDescriptor));
    Box::into_raw(Box::new(registration))
}
