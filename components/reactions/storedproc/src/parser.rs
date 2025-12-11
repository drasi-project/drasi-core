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
    PARAM_REGEX.get_or_init(|| Regex::new(r"@(\w+)").unwrap())
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

        // Handle different formats:
        // "CALL procedure_name(...)"
        // "EXEC procedure_name(...)"
        // "procedure_name(...)"
        // "schema.procedure_name(...)"

        let name_part = if let Some(stripped) = trimmed.strip_prefix("CALL ") {
            stripped
        } else if let Some(stripped) = trimmed.strip_prefix("EXEC ") {
            stripped
        } else {
            trimmed
        };

        // Extract everything before the first parenthesis
        let name = name_part
            .split('(')
            .next()
            .ok_or_else(|| anyhow!("Invalid procedure format: {}", command))?
            .trim()
            .to_string();

        if name.is_empty() {
            anyhow::bail!("Procedure name is empty in command: {}", command);
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

        for part in &parts {
            current = current
                .get(part)
                .ok_or_else(|| anyhow!("Field '{}' not found in query result", field_name))?;
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
}
