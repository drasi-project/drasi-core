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

#[cfg(test)]
mod error_handling_tests {
    use crate::utils::{conversion, time};
    use serde_json::json;

    #[test]
    fn test_timestamp_overflow_handling() {
        // Test normal timestamp
        let result = time::get_current_timestamp_nanos();
        assert!(result.is_ok());

        // Test with fallback
        let result = time::get_timestamp_with_fallback(Some(42));
        assert!(result.is_ok());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn test_system_time_handling() {
        // Normal system time should work
        let result = time::get_system_time_nanos();
        assert!(result.is_ok());

        // Result should be a reasonable timestamp
        let timestamp = result.unwrap();
        assert!(timestamp > 1_000_000_000_000_000_000); // After year 2000 in nanos
    }

    #[test]
    fn test_json_conversion_error_handling() {
        // Valid conversions
        let number = json!(42);
        let result = conversion::safe_json_to_element_value(&number);
        assert!(result.is_ok());

        let string = json!("test");
        let result = conversion::safe_json_to_element_value(&string);
        assert!(result.is_ok());

        // With default fallback
        let valid = json!(true);
        let default = drasi_core::models::ElementValue::Null;
        let result = conversion::json_to_element_value_or_default(&valid, default.clone());
        assert!(!matches!(result, drasi_core::models::ElementValue::Null));
    }

    #[test]
    fn test_batch_conversion() {
        let values =
            vec![json!(1), json!("string"), json!(true), json!(null), json!({"key": "value"})];

        let results = conversion::batch_json_to_element_values(values.iter());
        assert!(!results.is_empty());
        // Should convert at least some values successfully
        assert!(results.len() >= 4); // null, number, string, bool should work
    }
}
