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

//! Key formatting and identifier helpers for MySQL plugins.

use drasi_core::models::ElementValue;

/// Formats an `ElementValue` into a string suitable for use as part of an element ID key.
pub fn format_value_for_key(value: &ElementValue) -> String {
    match value {
        ElementValue::Null => "null".to_string(),
        ElementValue::Bool(b) => b.to_string(),
        ElementValue::Float(f) => f.to_string(),
        ElementValue::Integer(i) => i.to_string(),
        ElementValue::String(s) => s.to_string(),
        ElementValue::List(l) => l
            .iter()
            .map(format_value_for_key)
            .collect::<Vec<_>>()
            .join("-"),
        ElementValue::Object(_) => "object".to_string(),
    }
}

/// Escapes backticks in an identifier for safe use in MySQL queries.
pub fn escape_identifier(value: &str) -> String {
    value.replace('`', "``")
}

/// Quotes a MySQL identifier with backticks, escaping any internal backticks.
pub fn quote_identifier(value: &str) -> String {
    format!("`{}`", escape_identifier(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_format_value_for_key_null() {
        assert_eq!(format_value_for_key(&ElementValue::Null), "null");
    }

    #[test]
    fn test_format_value_for_key_bool() {
        assert_eq!(format_value_for_key(&ElementValue::Bool(true)), "true");
        assert_eq!(format_value_for_key(&ElementValue::Bool(false)), "false");
    }

    #[test]
    fn test_format_value_for_key_integer() {
        assert_eq!(format_value_for_key(&ElementValue::Integer(42)), "42");
        assert_eq!(format_value_for_key(&ElementValue::Integer(-1)), "-1");
    }

    #[test]
    fn test_format_value_for_key_string() {
        assert_eq!(
            format_value_for_key(&ElementValue::String(Arc::from("hello"))),
            "hello"
        );
    }

    #[test]
    fn test_format_value_for_key_list() {
        let list = ElementValue::List(vec![
            ElementValue::Integer(1),
            ElementValue::Integer(2),
            ElementValue::Integer(3),
        ]);
        assert_eq!(format_value_for_key(&list), "1-2-3");
    }

    #[test]
    fn test_escape_identifier_no_backticks() {
        assert_eq!(escape_identifier("users"), "users");
    }

    #[test]
    fn test_escape_identifier_with_backticks() {
        assert_eq!(escape_identifier("my`table"), "my``table");
    }

    #[test]
    fn test_quote_identifier() {
        assert_eq!(quote_identifier("users"), "`users`");
        assert_eq!(quote_identifier("my`table"), "`my``table`");
    }
}
