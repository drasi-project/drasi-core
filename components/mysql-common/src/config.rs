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

//! Shared configuration types for MySQL plugins.

/// Table key configuration for MySQL sources and bootstrappers.
///
/// Maps a table name to the columns that form its primary key,
/// used to generate deterministic element IDs.
#[derive(Debug, Clone, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// Validates that a string is a safe SQL identifier (alphanumeric + underscore only).
pub fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_identifiers() {
        assert!(is_valid_identifier("users"));
        assert!(is_valid_identifier("order_items"));
        assert!(is_valid_identifier("Table1"));
        assert!(is_valid_identifier("_private"));
    }

    #[test]
    fn test_invalid_identifiers() {
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("my table"));
        assert!(!is_valid_identifier("table;DROP"));
        assert!(!is_valid_identifier("my-table"));
        assert!(!is_valid_identifier("table.name"));
    }
}
