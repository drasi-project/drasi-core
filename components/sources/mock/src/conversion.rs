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

use anyhow::{Context, Result};
use serde_json::Value;

/// Converts a JSON value to an Element value with error context.
///
/// This is the "strict" conversion that returns a `Result`, allowing the caller
/// to handle conversion failures explicitly.
///
/// # Arguments
///
/// * `json_value` - The JSON value to convert
///
/// # Returns
///
/// The converted [`ElementValue`](drasi_core::models::ElementValue), or an error
/// with context describing what failed.
///
/// # Errors
///
/// Returns an error if the JSON value cannot be converted to an Element value
/// (e.g., unsupported JSON structure).
#[allow(dead_code)]
pub fn safe_json_to_element_value(json_value: &Value) -> Result<drasi_core::models::ElementValue> {
    drasi_lib::sources::convert_json_to_element_value(json_value)
        .with_context(|| format!("Failed to convert JSON value to Element value: {json_value:?}"))
}

/// Converts a JSON value to an Element value, returning a default on failure.
///
/// This is the "lenient" conversion that never fails. Useful when you have a
/// sensible default and don't want to handle errors explicitly.
///
/// # Arguments
///
/// * `json_value` - The JSON value to convert
/// * `default` - The fallback value to use if conversion fails
///
/// # Returns
///
/// The converted [`ElementValue`](drasi_core::models::ElementValue), or `default`
/// if conversion fails (with a warning logged).
pub fn json_to_element_value_or_default(
    json_value: &Value,
    default: drasi_core::models::ElementValue,
) -> drasi_core::models::ElementValue {
    match drasi_lib::sources::convert_json_to_element_value(json_value) {
        Ok(value) => value,
        Err(e) => {
            log::warn!("Failed to convert JSON to Element value, using default: {e}");
            default
        }
    }
}

/// Converts multiple JSON values to Element values, filtering failures.
///
/// This is useful for batch processing where some values may be invalid.
/// Invalid values are logged and skipped rather than causing the entire
/// operation to fail.
///
/// # Arguments
///
/// * `values` - Iterator of JSON values to convert
///
/// # Returns
///
/// A vector containing only the successfully converted values.
/// Failed conversions are logged at `debug` level and excluded from the result.
#[allow(dead_code)]
pub fn batch_json_to_element_values<'a>(
    values: impl Iterator<Item = &'a Value>,
) -> Vec<drasi_core::models::ElementValue> {
    values
        .filter_map(|json_value| {
            match drasi_lib::sources::convert_json_to_element_value(json_value) {
                Ok(value) => Some(value),
                Err(e) => {
                    log::debug!(
                        "Skipping unconvertible JSON value during batch conversion: {json_value:?}, error: {e}"
                    );
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_safe_json_to_element_value() {
        // Test with valid values
        let number = json!(42);
        let result = safe_json_to_element_value(&number);
        assert!(result.is_ok());

        let string = json!("test");
        let result = safe_json_to_element_value(&string);
        assert!(result.is_ok());

        let boolean = json!(true);
        let result = safe_json_to_element_value(&boolean);
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_to_element_value_or_default() {
        let valid = json!(42);
        let default = drasi_core::models::ElementValue::Null;
        let result = json_to_element_value_or_default(&valid, default.clone());
        // Result should not be the default for valid input
        assert!(matches!(
            result,
            drasi_core::models::ElementValue::Integer(_)
        ));
    }

    #[test]
    fn test_batch_json_to_element_values() {
        let values = [json!(1), json!("test"), json!(true), json!(null)];
        let results = batch_json_to_element_values(values.iter());
        // Should have converted all valid values
        assert!(!results.is_empty());
    }
}
