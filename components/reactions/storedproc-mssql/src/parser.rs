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

//! Parameter parser for stored procedure commands.

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

static PARAM_REGEX: OnceLock<Regex> = OnceLock::new();

fn get_param_regex() -> &'static Regex {
    PARAM_REGEX.get_or_init(|| Regex::new(r"@([\w.]+)").expect("Valid regex pattern"))
}

/// Parameter parser for extracting and substituting query result fields
#[derive(Debug, Clone)]
pub struct ParameterParser;

impl ParameterParser {
    /// Create a new parameter parser
    pub fn new() -> Self {
        Self
    }

    /// Parse a stored procedure command and extract parameters
    ///
    /// # Arguments
    /// * `command` - The stored procedure command with @parameter syntax
    /// * `data` - Query result data (JSON object)
    ///
    /// # Returns
    /// A tuple of (procedure_name, ordered_parameters)
    ///
    /// # Example
    /// ```ignore
    /// let parser = ParameterParser::new();
    /// let command = "CALL add_user(@id, @name, @email)";
    /// let data = json!({"id": 1, "name": "Alice", "email": "alice@example.com"});
    /// let (proc_name, params) = parser.parse_command(command, &data)?;
    /// // proc_name = "add_user"
    /// // params = [("id", 1), ("name", "Alice"), ("email", "alice@example.com")]
    /// ```
    pub fn parse_command(&self, command: &str, data: &Value) -> Result<(String, Vec<Value>)> {
        // Extract procedure name
        let proc_name = self.extract_procedure_name(command)?;

        // Extract parameters in order of appearance
        let params = self.extract_parameters(command, data)?;

        Ok((proc_name, params))
    }

    /// Extract the procedure name from the command
    fn extract_procedure_name(&self, command: &str) -> Result<String> {
        let trimmed = command.trim();
        let upper = trimmed.to_uppercase();
        let name_part = if upper.starts_with("CALL ") || upper.starts_with("EXEC ") {
            &trimmed[5..] // Skip "CALL " or "EXEC " (case-insensitive)
        } else {
            trimmed
        };

        // Validate that parentheses exist
        if !name_part.contains('(') {
            anyhow::bail!("Invalid procedure format: {command}");
        }

        // Extract everything before the first parenthesis
        let name = name_part
            .split('(')
            .next()
            .ok_or_else(|| anyhow!("Invalid procedure format: {command}"))?
            .trim()
            .to_string();

        if name.is_empty() {
            anyhow::bail!("Procedure name is empty in command: {command}");
        }

        Ok(name)
    }

    /// Extract parameters from the command in order
    fn extract_parameters(&self, command: &str, data: &Value) -> Result<Vec<Value>> {
        let regex = get_param_regex();
        let mut params = Vec::new();

        for cap in regex.captures_iter(command) {
            let param_name = &cap[1];
            let value = self.get_field_value(param_name, data)?;
            params.push(value);
        }

        Ok(params)
    }

    /// Get a field value from the JSON data
    fn get_field_value(&self, field_name: &str, data: &Value) -> Result<Value> {
        // Support nested field access with dot notation
        // e.g., @user.name or @address.city
        let parts: Vec<&str> = field_name.split('.').collect();
        let mut current = data;

        for part in parts {
            current = current
                .get(part)
                .ok_or_else(|| anyhow!("Field '{field_name}' not found in query result"))?;
        }

        Ok(current.clone())
    }
}

impl Default for ParameterParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_procedure_name_call() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("CALL add_user(@id, @name)");
        assert_eq!(result.unwrap(), "add_user");
    }

    #[test]
    fn test_extract_procedure_name_exec() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("EXEC update_user(@id, @name)");
        assert_eq!(result.unwrap(), "update_user");
    }

    #[test]
    fn test_extract_procedure_name_direct() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("delete_user(@id)");
        assert_eq!(result.unwrap(), "delete_user");
    }

    #[test]
    fn test_extract_procedure_name_with_schema() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("CALL public.add_user(@id, @name)");
        assert_eq!(result.unwrap(), "public.add_user");
    }

    #[test]
    fn test_parse_command_simple() {
        let parser = ParameterParser::new();
        let command = "CALL add_user(@id, @name, @email)";
        let data = json!({
            "id": 1,
            "name": "Alice",
            "email": "alice@example.com"
        });

        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "add_user");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], json!(1));
        assert_eq!(params[1], json!("Alice"));
        assert_eq!(params[2], json!("alice@example.com"));
    }

    #[test]
    fn test_parse_command_nested_fields() {
        let parser = ParameterParser::new();
        let command = "CALL update_address(@user.id, @address.city, @address.zip)";
        let data = json!({
            "user": {"id": 42, "name": "Bob"},
            "address": {"city": "Seattle", "zip": "98101"}
        });

        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "update_address");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], json!(42));
        assert_eq!(params[1], json!("Seattle"));
        assert_eq!(params[2], json!("98101"));
    }

    #[test]
    fn test_parse_command_missing_field() {
        let parser = ParameterParser::new();
        let command = "CALL add_user(@id, @missing)";
        let data = json!({"id": 1, "name": "Alice"});

        let result = parser.parse_command(command, &data);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Field 'missing' not found"));
    }

    #[test]
    fn test_parse_command_duplicate_params() {
        let parser = ParameterParser::new();
        let command = "CALL update_user(@id, @name, @id)";
        let data = json!({"id": 1, "name": "Alice"});

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], json!(1));
        assert_eq!(params[1], json!("Alice"));
        assert_eq!(params[2], json!(1)); // Same value repeated
    }

    #[test]
    fn test_parse_command_no_params() {
        let parser = ParameterParser::new();
        let command = "CALL refresh_materialized_view()";
        let data = json!({"id": 1});

        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "refresh_materialized_view");
        assert_eq!(params.len(), 0);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_parse_command_various_types() {
        let parser = ParameterParser::new();
        let command = "CALL test_types(@str, @num, @bool, @null, @float, @arr, @obj)";
        let data = json!({
            "str": "hello",
            "num": 42,
            "bool": true,
            "null": null,
            "float": 3.14,
            "arr": [1, 2, 3],
            "obj": {"key": "value"}
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params.len(), 7);
        assert_eq!(params[0], json!("hello"));
        assert_eq!(params[1], json!(42));
        assert_eq!(params[2], json!(true));
        assert_eq!(params[3], json!(null));
        assert_eq!(params[4], json!(3.14));
        assert_eq!(params[5], json!([1, 2, 3]));
        assert_eq!(params[6], json!({"key": "value"}));
    }

    #[test]
    fn test_extract_procedure_name_no_parentheses() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("CALL add_user");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid procedure format"));
    }

    #[test]
    fn test_extract_procedure_name_empty() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("CALL ()");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_extract_procedure_name_whitespace() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("  CALL   add_user  ( @id )  ");
        assert_eq!(result.unwrap(), "add_user");
    }

    #[test]
    fn test_extract_procedure_name_multi_schema() {
        let parser = ParameterParser::new();
        let result = parser.extract_procedure_name("CALL db1.schema1.add_user(@id)");
        assert_eq!(result.unwrap(), "db1.schema1.add_user");
    }

    #[test]
    fn test_parse_command_deeply_nested_fields() {
        let parser = ParameterParser::new();
        let command = "CALL test(@a.b.c.d)";
        let data = json!({
            "a": {
                "b": {
                    "c": {
                        "d": "deep_value"
                    }
                }
            }
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params[0], json!("deep_value"));
    }

    #[test]
    fn test_parse_command_nested_field_not_found() {
        let parser = ParameterParser::new();
        let command = "CALL test(@user.profile.avatar)";
        let data = json!({
            "user": {
                "id": 1,
                "name": "Alice"
            }
        });

        let result = parser.parse_command(command, &data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_parse_command_special_characters_in_values() {
        let parser = ParameterParser::new();
        let command = "CALL add_user(@name, @email)";
        let data = json!({
            "name": "O'Brien",
            "email": "test@example.com; DROP TABLE users;"
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params[0], json!("O'Brien"));
        assert_eq!(params[1], json!("test@example.com; DROP TABLE users;"));
        // Values are returned as-is, SQL injection prevention happens at executor level
    }

    #[test]
    fn test_parse_command_case_insensitive_call() {
        let parser = ParameterParser::new();

        let result1 = parser.extract_procedure_name("CALL add_user(@id)");
        let result2 = parser.extract_procedure_name("call add_user(@id)");
        let result3 = parser.extract_procedure_name("Call add_user(@id)");

        assert_eq!(result1.unwrap(), "add_user");
        assert_eq!(result2.unwrap(), "add_user");
        assert_eq!(result3.unwrap(), "add_user");
    }

    #[test]
    fn test_parse_command_exec_vs_call() {
        let parser = ParameterParser::new();
        let data = json!({"id": 1});

        let (name1, params1) = parser.parse_command("CALL test_proc(@id)", &data).unwrap();
        let (name2, params2) = parser.parse_command("EXEC test_proc(@id)", &data).unwrap();

        assert_eq!(name1, "test_proc");
        assert_eq!(name2, "test_proc");
        assert_eq!(params1, params2);
    }

    #[test]
    fn test_parse_command_parameter_names_with_underscores() {
        let parser = ParameterParser::new();
        let command = "CALL test(@user_id, @first_name, @last_name)";
        let data = json!({
            "user_id": 123,
            "first_name": "John",
            "last_name": "Doe"
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], json!(123));
        assert_eq!(params[1], json!("John"));
        assert_eq!(params[2], json!("Doe"));
    }

    #[test]
    fn test_parse_command_numeric_field_names() {
        let parser = ParameterParser::new();
        let command = "CALL test(@field1, @field2)";
        let data = json!({
            "field1": "value1",
            "field2": "value2"
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], json!("value1"));
        assert_eq!(params[1], json!("value2"));
    }

    #[test]
    fn test_parse_command_empty_string_value() {
        let parser = ParameterParser::new();
        let command = "CALL test(@name, @description)";
        let data = json!({
            "name": "",
            "description": ""
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params[0], json!(""));
        assert_eq!(params[1], json!(""));
    }

    #[test]
    fn test_parse_command_large_numbers() {
        let parser = ParameterParser::new();
        let command = "CALL test(@big_int, @big_float)";
        let data = json!({
            "big_int": 9223372036854775807i64,
            "big_float": 1.7976931348623157e308
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params[0], json!(9223372036854775807i64));
        assert_eq!(params[1], json!(1.7976931348623157e308));
    }

    #[test]
    fn test_parse_command_unicode_values() {
        let parser = ParameterParser::new();
        let command = "CALL add_user(@name, @city)";
        let data = json!({
            "name": "李明",
            "city": "北京"
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params[0], json!("李明"));
        assert_eq!(params[1], json!("北京"));
    }

    #[test]
    fn test_get_field_value_array_access_not_supported() {
        let parser = ParameterParser::new();
        let data = json!({
            "users": [
                {"id": 1, "name": "Alice"},
                {"id": 2, "name": "Bob"}
            ]
        });

        // Array indexing is not supported by design
        let result = parser.get_field_value("users.0.name", &data);
        assert!(result.is_err());
    }

    #[test]
    fn test_parameter_parser_default() {
        let parser1 = ParameterParser::new();
        let parser2 = ParameterParser;

        // Both should work the same way
        let command = "CALL test(@id)";
        let data = json!({"id": 42});

        let result1 = parser1.parse_command(command, &data).unwrap();
        let result2 = parser2.parse_command(command, &data).unwrap();

        assert_eq!(result1, result2);
    }

    #[test]
    fn test_parse_command_trailing_commas_not_allowed() {
        let parser = ParameterParser::new();
        let command = "CALL test(@id,)"; // Trailing comma
        let data = json!({"id": 1});

        // Parser doesn't validate SQL syntax, it just extracts params
        // So this will succeed in parsing but fail at execution
        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "test");
        assert_eq!(params.len(), 1); // Only one valid parameter
    }

    #[test]
    fn test_parse_command_mixed_spacing() {
        let parser = ParameterParser::new();
        let command = "CALL test(@id,@name,  @email  )";
        let data = json!({
            "id": 1,
            "name": "Alice",
            "email": "alice@example.com"
        });

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params.len(), 3);
    }
}
