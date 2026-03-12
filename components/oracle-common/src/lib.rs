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

//! Shared types for Oracle source and bootstrap plugins.

pub mod config;
pub mod connection;
pub mod error;
pub mod keys;
pub mod logminer;
pub mod scn;
pub mod types;

pub use config::{
    validate_sql_identifier, AuthMode, OracleSourceConfig, SslMode, StartPosition, TableKeyConfig,
    ORACLE_BOOTSTRAP_SCN_CONTEXT_PROPERTY,
};
pub use connection::OracleConnection;
pub use error::{OracleError, OracleErrorKind};
pub use keys::{split_table_name, PrimaryKeyCache};
pub use logminer::{parse_sql_undo_insert, parse_sql_undo_update, split_sql_values, LogMinerGuard};
pub use scn::Scn;
pub use types::{
    extract_column_value, extract_row_properties, sql_literal_to_element_value, value_to_string,
};
