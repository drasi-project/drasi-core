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

/// Log reaction configuration
///
/// Supports Handlebars templates for formatting each event type.
/// When a template is not provided, the full JSON representation is shown.
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
/// ## Example
///
/// ```rust,ignore
/// let config = LogReactionConfig {
///     added_template: Some("[NEW] Sensor {{after.id}}: temp={{after.temperature}}".to_string()),
///     updated_template: Some("[CHG] {{after.id}}: {{before.temperature}} -> {{after.temperature}}".to_string()),
///     deleted_template: Some("[DEL] Sensor {{before.id}} removed".to_string()),
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LogReactionConfig {
    /// Handlebars template for ADD events.
    /// Available variables: `after`, `query_name`, `operation`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_template: Option<String>,

    /// Handlebars template for UPDATE events.
    /// Available variables: `before`, `after`, `data`, `query_name`, `operation`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_template: Option<String>,

    /// Handlebars template for DELETE events.
    /// Available variables: `before`, `query_name`, `operation`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_template: Option<String>,
}
