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

//! Shared types for MS SQL source and bootstrap plugins
//!
//! This crate contains configuration, connection, error, and type conversion
//! types used by both `drasi-source-mssql` and `drasi-bootstrap-mssql`.

pub mod config;
pub mod connection;
pub mod error;
pub mod keys;
pub mod types;

// Re-export main types
pub use config::{
    validate_sql_identifier, AuthMode, EncryptionMode, MsSqlSourceConfig, StartPosition,
    TableKeyConfig,
};
pub use connection::MsSqlConnection;
pub use error::{ConnectionError, LsnError, MsSqlError, MsSqlErrorKind, PrimaryKeyError};
pub use keys::PrimaryKeyCache;
pub use types::{extract_column_value, value_to_string};
