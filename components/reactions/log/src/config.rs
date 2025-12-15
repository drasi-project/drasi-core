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

//! Configuration types for log reaction.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for query-specific templates.
///
/// Defines different template strings for each operation type (added, updated, deleted).
/// Each operation type can have its own formatting template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct QueryTemplates {
    /// Template for ADD operations (new rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,

    /// Template for UPDATE operations (modified rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,

    /// Template for DELETE operations (removed rows from query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<String>,
}

/// Log reaction configuration
///
/// Supports Handlebars templates for formatting each event type.
/// When a template is not provided, the full JSON representation is shown.
///
/// Templates can be configured at two levels:
/// 1. **Default templates**: Applied to all queries unless overridden
/// 2. **Per-query templates**: Override defaults for specific queries
///
/// ## Template Variables
///
/// Templates have access to the following variables:
/// - `after` - The data after the change (available for ADD and UPDATE)
/// - `before` - The data before the change (available for UPDATE and DELETE)
/// - `data` - The raw data field (available for UPDATE)
/// - `query_name` - The name of the query that produced the result
/// - `operation` - The operation type ("ADD", "UPDATE", or "DELETE")
///
/// ## Example with Default Templates
///
/// ```rust,ignore
/// let config = LogReactionConfig {
///     added_template: Some("[NEW] Sensor {{after.id}}: temp={{after.temperature}}".to_string()),
///     updated_template: Some("[CHG] {{after.id}}: {{before.temperature}} -> {{after.temperature}}".to_string()),
///     deleted_template: Some("[DEL] Sensor {{before.id}} removed".to_string()),
///     query_templates: HashMap::new(),
/// };
/// ```
///
/// ## Example with Per-Query Templates
///
/// ```rust,ignore
/// use std::collections::HashMap;
///
/// let mut query_templates = HashMap::new();
/// query_templates.insert("sensor-query".to_string(), QueryTemplates {
///     added: Some("[SENSOR] New: {{after.id}}".to_string()),
///     updated: None,  // Falls back to default
///     deleted: None,  // Falls back to default
/// });
///
/// let config = LogReactionConfig {
///     added_template: Some("[DEFAULT] {{after.id}}".to_string()),
///     updated_template: None,
///     deleted_template: None,
///     query_templates,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LogReactionConfig {
    /// Default Handlebars template for ADD events across all queries.
    /// Available variables: `after`, `query_name`, `operation`
    /// Falls back to JSON output if not provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_template: Option<String>,

    /// Default Handlebars template for UPDATE events across all queries.
    /// Available variables: `before`, `after`, `data`, `query_name`, `operation`
    /// Falls back to JSON output if not provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_template: Option<String>,

    /// Default Handlebars template for DELETE events across all queries.
    /// Available variables: `before`, `query_name`, `operation`
    /// Falls back to JSON output if not provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_template: Option<String>,

    /// Per-query template overrides.
    /// Maps query IDs to their specific template configurations.
    /// Query-specific templates override the default templates.
    #[serde(default)]
    pub query_templates: HashMap<String, QueryTemplates>,
}
