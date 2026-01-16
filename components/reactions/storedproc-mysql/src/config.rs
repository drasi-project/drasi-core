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

//! Configuration for the MySQL Stored Procedure reaction.

use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Re-export common template types for public API
pub use common::{QueryConfig, TemplateSpec};

/// Configuration for the MySQL Stored Procedure reaction
///
/// Supports Handlebars templates for stored procedure commands.
/// Commands use @after.fieldName and @before.fieldName syntax to reference query result fields.
///
/// Commands can be configured at two levels:
/// 1. **Default templates**: Applied to all queries unless overridden (using `default_template`)
/// 2. **Per-query templates**: Override default for specific queries (using `routes`)
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
/// let config = MySqlStoredProcReactionConfig {
///     hostname: "localhost".to_string(),
///     database: "mydb".to_string(),
///     user: "root".to_string(),
///     password: "password".to_string(),
///     default_template: Some(QueryConfig {
///         added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
///         updated: Some(TemplateSpec::new("CALL update_user(@after.id, @after.name, @after.email)")),
///         deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
///     }),
///     ..Default::default()
/// };
/// ```
///
/// ## Example with Per-Query Templates
///
/// ```rust,ignore
/// use std::collections::HashMap;
///
/// let mut routes = HashMap::new();
/// routes.insert("user-query".to_string(), QueryConfig {
///     added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
///     updated: Some(TemplateSpec::new("CALL update_user(@after.id, @after.name, @after.email)")),
///     deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
/// });
///
/// let config = MySqlStoredProcReactionConfig {
///     hostname: "localhost".to_string(),
///     database: "mydb".to_string(),
///     user: "root".to_string(),
///     password: "password".to_string(),
///     routes,
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySqlStoredProcReactionConfig {
    /// Database hostname or IP address
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Database port (default: 3306)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    /// Database user
    pub user: String,

    /// Database password
    pub password: String,

    /// Database name
    pub database: String,

    /// Enable SSL/TLS
    #[serde(default)]
    pub ssl: bool,

    /// Query-specific template configurations for stored procedure commands
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Default template configuration used when no query-specific route is defined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,

    /// Command timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub command_timeout_ms: u64,

    /// Number of retry attempts on failure
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
}

impl Default for MySqlStoredProcReactionConfig {
    fn default() -> Self {
        Self {
            hostname: default_hostname(),
            port: None,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            ssl: false,
            routes: HashMap::new(),
            default_template: None,
            command_timeout_ms: default_timeout_ms(),
            retry_attempts: default_retry_attempts(),
        }
    }
}

impl TemplateRouting for MySqlStoredProcReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}

fn default_hostname() -> String {
    // DevSkim: ignore DS137138
    "localhost".to_string()
}

fn default_timeout_ms() -> u64 {
    30000
}

fn default_retry_attempts() -> u32 {
    3
}

impl MySqlStoredProcReactionConfig {
    /// Get the port for the database, using the default if not specified
    pub fn get_port(&self) -> u16 {
        self.port.unwrap_or(3306)
    }

    /// Get the command template for a specific query and operation type
    ///
    /// Priority order:
    /// 1. Query-specific route template
    /// 2. Default template
    pub fn get_command_template(
        &self,
        query_id: &str,
        operation: common::OperationType,
    ) -> Option<String> {
        // Try to get from routes or default_template first
        let spec = self.get_template_spec(query_id, operation);
        if let Some(spec) = spec {
            return Some(spec.template.clone());
        }
        None
    }

    /// Validate the configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.user.is_empty() {
            anyhow::bail!("Database user is required");
        }
        if self.database.is_empty() {
            anyhow::bail!("Database name is required");
        }

        // Check if at least one command is configured
        let has_routes = !self.routes.is_empty();
        let has_default_template = self.default_template.is_some();

        if !has_routes && !has_default_template {
            anyhow::bail!(
                "At least one command must be specified (via routes or default_template)"
            );
        }

        Ok(())
    }
}
