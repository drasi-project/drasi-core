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

//! Shared PostgreSQL type conversion for Drasi source and bootstrap plugins.
//!
//! Both the CDC source and the bootstrap provider convert column values through
//! [`PostgresValue::to_element_value`] so snapshot and live rows emit identical
//! [`drasi_core::models::ElementValue`]s.

#![allow(unexpected_cfgs)]

pub mod decode;
pub mod oid;
pub mod value;

pub use decode::{
    decode_column_value_text, decode_text_to_element_value, decode_text_to_postgres_value,
    string_element,
};
pub use oid::{
    array_element_oid, is_array_oid, oid_from_information_schema, BOOL, BYTEA, CHAR, DATE, FLOAT4,
    FLOAT8, INT2, INT4, INT8, JSON, JSONB, NAME, NUMERIC, TEXT, TIME, TIMESTAMP, TIMESTAMPTZ, UUID,
    VARCHAR,
};
pub use value::{encode_base64, parse_bytea_text, PostgresValue};

/// Unambiguous key encoding for complete-transaction sources and their snapshots.
/// The legacy underscore-joined encoding remains unchanged.
pub fn transaction_element_id(
    namespace: &str,
    table: &str,
    keys: &[(String, String)],
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !namespace.is_empty() && !table.is_empty() && !keys.is_empty(),
        "transaction row identity requires a table and complete key"
    );
    let mut keys = keys.iter().collect::<Vec<_>>();
    keys.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    anyhow::ensure!(
        keys.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "transaction row identity contains duplicate key columns"
    );
    Ok(format!(
        "pg:v1:{}",
        serde_json::to_string(&(namespace, table, keys))?
    ))
}
