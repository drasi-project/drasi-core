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

//! Type conversion from MS SQL to Drasi ElementValue

use anyhow::Result;
use drasi_core::models::ElementPropertyMap;
use tiberius::Row;

// Re-export from common for backward compatibility
pub use drasi_mssql_common::types::{extract_column_value, value_to_string};

/// Extract all properties from a CDC row, skipping metadata columns
///
/// # Arguments
/// * `row` - The Tiberius row containing both data and CDC metadata columns
///
/// # Returns
/// ElementPropertyMap with all non-CDC columns converted to ElementValue
pub fn extract_properties_from_cdc_row(row: &Row) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();

    for (idx, column) in row.columns().iter().enumerate() {
        let col_name = column.name();

        // Skip CDC metadata columns (those starting with __$)
        if crate::decoder::cdc_columns::is_metadata_column(col_name) {
            continue;
        }

        // Extract value and convert to ElementValue
        let value = drasi_mssql_common::types::extract_column_value(row, idx)?;
        properties.insert(col_name, value);
    }

    Ok(properties)
}
