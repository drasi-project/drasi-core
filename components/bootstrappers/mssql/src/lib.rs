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

//! Microsoft SQL Server Bootstrap Provider for Drasi
//!
//! This provider reads initial data snapshots from MS SQL Server tables
//! to bootstrap continuous queries before CDC streaming begins.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_mssql::MsSqlBootstrapProvider;
//!
//! # fn example() -> anyhow::Result<()> {
//! let provider = MsSqlBootstrapProvider::builder()
//!     .with_host("localhost")
//!     .with_database("production")
//!     .with_user("drasi_user")
//!     .with_password("secure_password")
//!     .with_tables(vec!["orders".to_string()])
//!     .build()?;
//! # Ok(())
//! # }
//! ```

pub mod descriptor;
pub mod mssql;

pub use drasi_mssql_common::{AuthMode, EncryptionMode, MsSqlSourceConfig, TableKeyConfig};
pub use mssql::{MsSqlBootstrapProvider, MsSqlBootstrapProviderBuilder};

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "mssql-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::MsSqlBootstrapDescriptor],
);
