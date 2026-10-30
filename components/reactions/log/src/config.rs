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

use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Re-export common template types for backward compatibility
pub use common::{QueryConfig, TemplateSpec};

/// Log reaction configuration
///
/// Supports Handlebars templates for formatting each event type.
/// When a template is not provided, the full JSON representation is shown.
///
/// Templates can be configured at two levels:
/// 1. **Default template**: Applied to all queries unless overridden
/// 2. **Per-query templates**: Override default for specific queries
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
/// ## Example with Default Template
///
/// ```rust,ignore
/// let default_template = QueryConfig {
///     added: Some(TemplateSpec::new("[NEW] {{after.id}}")),
///     updated: Some(TemplateSpec::new("[CHG] {{after.id}}")),
///     deleted: Some(TemplateSpec::new("[DEL] {{before.id}}")),
/// };
///
/// let config = LogReactionConfig {
///     routes: HashMap::new(),
///     default_template: Some(default_template),
/// };
/// ```
///
/// ## Example with Per-Query Templates
///
/// ```rust,ignore
/// use std::collections::HashMap;
///
/// let mut routes = HashMap::new();
/// routes.insert("sensor-query".to_string(), QueryConfig {
///     added: Some(TemplateSpec {
///         template: "[SENSOR] New: {{after.id}}".to_string(),
///         extension: (),
///     }),
///     updated: None,  // Falls back to default
///     deleted: None,  // Falls back to default
/// });
///
/// let config = LogReactionConfig {
///     routes,
///     default_template: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LogReactionConfig {
    /// Query-specific template configurations
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Default template configuration used when no query-specific route is defined.
    /// If not set, falls back to raw JSON output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
}

impl TemplateRouting for LogReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}
