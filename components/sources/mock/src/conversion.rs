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

/// Safely convert JSON values to Element values.
///
/// This function provides safe conversion from JSON to Element values
/// with proper error handling instead of using unwrap().
///
/// # Arguments
///
/// * `json_value` - The JSON value to convert
///
/// # Returns
///
/// Returns the converted Element value or an error with context.
#[allow(dead_code)]
pub fn safe_json_to_element_value(json_value: &Value) -> Result<drasi_core::models::ElementValue> {
    drasi_lib::sources::convert_json_to_element_value(json_value).with_context(|| {
        format!(
            "Failed to convert JSON value to Element value: {:?}",
            json_value
        )
    })
}

/// Convert JSON to Element value with a default fallback.
///
/// This function attempts to convert a JSON value to an Element value,
/// returning a default value if the conversion fails.
///
/// # Arguments
///
/// * `json_value` - The JSON value to convert
/// * `default` - The default Element value to use if conversion fails
///
/// # Returns
///
/// Returns the converted Element value or the provided default.
pub fn json_to_element_value_or_default(
    json_value: &Value,
    default: drasi_core::models::ElementValue,
) -> drasi_core::models::ElementValue {
    match drasi_lib::sources::convert_json_to_element_value(json_value) {
        Ok(value) => value,
        Err(e) => {
            log::warn!(
                "Failed to convert JSON to Element value, using default: {}",
                e
            );
            default
        }
    }
}

/// Batch convert multiple JSON values to Element values.
///
/// This function converts multiple JSON values, collecting successful conversions
/// and logging failures.
///
/// # Arguments
///
/// * `values` - Iterator of JSON values to convert
///
/// # Returns
///
/// Returns a vector of successfully converted Element values.
/// Failed conversions are logged but do not stop the process.
#[allow(dead_code)]
pub fn batch_json_to_element_values<'a>(
    values: impl Iterator<Item = &'a Value>,
) -> Vec<drasi_core::models::ElementValue> {
    values
        .filter_map(|json_value| {
            match drasi_lib::sources::convert_json_to_element_value(json_value) {
                Ok(value) => Some(value),
                Err(e) => {
                    log::error!(
                        "Skipping invalid JSON value during batch conversion: {:?}, error: {}",
                        json_value,
                        e
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
