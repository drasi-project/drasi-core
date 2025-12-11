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

//! Parameter parser using sqlparser for robust SQL parsing

use anyhow::{anyhow, Result};
use serde_json::Value;
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, Ident, ObjectName, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Parameter parser using sqlparser for SQL validation and parsing
#[derive(Debug, Clone)]
pub struct ParameterParser {
    dialect: PostgreSqlDialect,
}

impl ParameterParser {
    /// Create a new parameter parser
    pub fn new() -> Self {
        Self {
            dialect: PostgreSqlDialect {},
        }
    }

    /// Parse a stored procedure command and extract parameters
    ///
    /// # Example
    /// ```ignore
    /// let parser = ParameterParser::new();
    /// let command = "CALL add_user(@id, @name, @email)";
    /// let data = json!({"id": 1, "name": "Alice", "email": "alice@example.com"});
    /// let (proc_name, params) = parser.parse_command(command, &data)?;
    /// ```
    pub fn parse_command(&self, command: &str, data: &Value) -> Result<(String, Vec<Value>)> {
        // First, replace @parameters with placeholder tokens that sqlparser can handle
        // We'll use $1, $2, etc. temporarily for parsing
        let (normalized_sql, param_names) = self.normalize_parameters(command)?;

        // Parse the SQL statement
        let statements = Parser::parse_sql(&self.dialect, &normalized_sql)
            .map_err(|e| anyhow!("Failed to parse SQL command: {}", e))?;

        if statements.is_empty() {
            return Err(anyhow!("No SQL statement found in command"));
        }

        if statements.len() > 1 {
            return Err(anyhow!("Multiple statements not supported"));
        }

        // Extract procedure name from the parsed statement
        let proc_name = self.extract_procedure_name(&statements[0])?;

        // Extract parameter values from data based on the original parameter names
        let params = param_names
            .iter()
            .map(|name| self.get_field_value(name, data))
            .collect::<Result<Vec<_>>>()?;

        Ok((proc_name, params))
    }

    /// Normalize @parameters to $1, $2, etc. for sqlparser
    /// Returns (normalized_sql, parameter_names)
    fn normalize_parameters(&self, command: &str) -> Result<(String, Vec<String>)> {
        let mut normalized = command.to_string();
        let mut param_names = Vec::new();
        let mut param_index = 1;

        // Find all @parameter references
        let re = regex::Regex::new(r"@([\w.]+)").unwrap();

        // Collect all matches first to avoid mutating while iterating
        let matches: Vec<_> = re
            .captures_iter(command)
            .map(|cap| (cap.get(0).unwrap().as_str(), cap[1].to_string()))
            .collect();

        for (full_match, param_name) in matches {
            param_names.push(param_name);
            normalized = normalized.replace(full_match, &format!("${}", param_index));
            param_index += 1;
        }

        Ok((normalized, param_names))
    }

    /// Extract procedure name from parsed SQL statement
    fn extract_procedure_name(&self, statement: &Statement) -> Result<String> {
        match statement {
            // PostgreSQL CALL statement
            Statement::Call(call) => {
                let name = self.object_name_to_string(&call.function_name);
                Ok(name)
            }
            // EXECUTE statement (alternative syntax)
            Statement::Execute { name, .. } => {
                let proc_name = self.ident_to_string(name);
                Ok(proc_name)
            }
            _ => Err(anyhow!(
                "Expected CALL or EXECUTE statement, got: {:?}",
                statement
            )),
        }
    }

    /// Convert ObjectName to string (handles schema.procedure format)
    fn object_name_to_string(&self, name: &ObjectName) -> String {
        name.0
            .iter()
            .map(|ident| ident.value.clone())
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Convert Ident to string
    fn ident_to_string(&self, ident: &Ident) -> String {
        ident.value.clone()
    }

    /// Get a field value from JSON data, supporting nested access
    fn get_field_value(&self, field_name: &str, data: &Value) -> Result<Value> {
        // Support nested field access with dot notation
        let parts: Vec<&str> = field_name.split('.').collect();
        let mut current = data;

        for part in &parts {
            current = current
                .get(part)
                .ok_or_else(|| anyhow!("Field '{}' not found in query result", field_name))?;
        }

        Ok(current.clone())
    }

    /// Validate that a command is a valid CALL statement
    pub fn validate_command(&self, command: &str) -> Result<()> {
        let (normalized_sql, _) = self.normalize_parameters(command)?;

        let statements = Parser::parse_sql(&self.dialect, &normalized_sql)
            .map_err(|e| anyhow!("Invalid SQL syntax: {}", e))?;

        if statements.is_empty() {
            return Err(anyhow!("No SQL statement found"));
        }

        if statements.len() > 1 {
            return Err(anyhow!("Multiple statements not allowed"));
        }

        match &statements[0] {
            Statement::Call(_) | Statement::Execute { .. } => Ok(()),
            _ => Err(anyhow!("Only CALL or EXECUTE statements are allowed")),
        }
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
    fn test_parse_simple_call() {
        let parser = ParameterParser::new();
        let command = "CALL add_user(@id, @name)";
        let data = json!({"id": 1, "name": "Alice"});

        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "add_user");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], json!(1));
        assert_eq!(params[1], json!("Alice"));
    }

    #[test]
    fn test_parse_schema_qualified() {
        let parser = ParameterParser::new();
        let command = "CALL public.add_user(@id, @name)";
        let data = json!({"id": 42, "name": "Bob"});

        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "public.add_user");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_parse_nested_fields() {
        let parser = ParameterParser::new();
        let command = "CALL update_address(@user.id, @address.city)";
        let data = json!({
            "user": {"id": 10, "name": "Charlie"},
            "address": {"city": "Seattle", "zip": "98101"}
        });

        let (proc_name, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(proc_name, "update_address");
        assert_eq!(params[0], json!(10));
        assert_eq!(params[1], json!("Seattle"));
    }

    #[test]
    fn test_validate_valid_command() {
        let parser = ParameterParser::new();
        assert!(parser.validate_command("CALL test(@id)").is_ok());
        assert!(parser
            .validate_command("CALL schema.proc(@a, @b)")
            .is_ok());
    }

    #[test]
    fn test_validate_invalid_command() {
        let parser = ParameterParser::new();

        // Not a CALL statement
        assert!(parser.validate_command("SELECT * FROM users").is_err());

        // Invalid SQL syntax
        assert!(parser.validate_command("CALL test(@id").is_err());

        // Multiple statements
        assert!(parser
            .validate_command("CALL test(@id); DROP TABLE users;")
            .is_err());
    }

    #[test]
    fn test_missing_field() {
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
    fn test_duplicate_params() {
        let parser = ParameterParser::new();
        let command = "CALL test(@id, @name, @id)";
        let data = json!({"id": 1, "name": "Alice"});

        let (_, params) = parser.parse_command(command, &data).unwrap();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], json!(1));
        assert_eq!(params[1], json!("Alice"));
        assert_eq!(params[2], json!(1)); // Same value repeated
    }
}
