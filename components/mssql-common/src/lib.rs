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

#![allow(unexpected_cfgs)]

//! Shared SQL Server connection, mapping and command helpers.
//!
//! This crate contains configuration, connection, error, and type conversion
//! types shared by SQL Server sources and bootstrap providers. The `sql`
//! feature also exposes stored-procedure rendering, routing and parameter
//! binding. These use shared value types and contracts, not a query engine or
//! component runtime. Select `native-tls` (default) or `rustls` for connections.

pub mod config;
pub mod decoder;
pub mod connection;
pub mod error;
pub mod keys;
pub mod lsn;
pub mod types;

#[cfg(feature = "sql")]
pub mod sql;

#[cfg(feature = "native")]
pub mod native;

// Re-export main types
pub use config::{
    validate_sql_identifier, AuthMode, EncryptionMode, MsSqlSourceConfig, StartPosition,
    TableKeyConfig,
};
pub use connection::MsSqlConnection;
pub use error::{ConnectionError, LsnError, MsSqlError, MsSqlErrorKind, PrimaryKeyError};
pub use keys::PrimaryKeyCache;
pub use lsn::Lsn;
pub use types::{extract_column_value, value_to_string};
