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

use drasi_lib::identity::IdentityProvider;
use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Re-export common template types for public API
pub use common::{QueryConfig, TemplateSpec};

/// Configuration for the MySQL Stored Procedure reaction
///
/// Stored procedure commands are Handlebars templates. Field values are bound
/// as positional parameters with the `{{param <path>}}` helper — they are never
/// substituted into the SQL text — so commands are safe against SQL injection.
///
/// Commands can be configured at two levels:
/// 1. **Default template**: Applied to any query without a route (using `default_template`)
/// 2. **Per-query templates**: Override the default for specific queries (using `routes`)
///
/// ## Template context
///
/// Every command template is rendered against a context with these keys:
/// - `after` - The row after the change (ADD and UPDATE)
/// - `before` - The row before the change (UPDATE and DELETE)
/// - `data` - The raw `data` payload of an UPDATE diff
/// - `query_id` / `query_name` - The id of the query that produced the result
/// - `operation` - The operation type (`"ADD"`, `"UPDATE"`, or `"DELETE"`)
/// - `timestamp` - The result timestamp (RFC3339)
/// - `metadata` - The result metadata map
///
/// ## Template helpers
///
/// - `{{param <path>}}` binds the value at `<path>` (for example
///   `after.id`) as a positional parameter and emits a `?` placeholder.
///   Objects and arrays are bound whole and stored as JSON.
/// - `{{json <path>}}` writes the JSON serialization of a value inline.
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
///         added: Some(TemplateSpec::new(
///             "CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})",
///         )),
///         updated: Some(TemplateSpec::new(
///             "CALL update_user({{param after.id}}, {{param after.name}}, {{param after.email}})",
///         )),
///         deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
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
///     added: Some(TemplateSpec::new(
///         "CALL add_user({{param after.id}}, {{param after.name}})",
///     )),
///     updated: Some(TemplateSpec::new(
///         "CALL update_user({{param after.id}}, {{param after.name}})",
///     )),
///     deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
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
#[derive(Clone, Serialize, Deserialize)]
pub struct MySqlStoredProcReactionConfig {
    /// Database hostname or IP address
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Database port (default: 3306)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    /// Identity provider for authentication (takes precedence over user/password)
    #[serde(skip)]
    pub identity_provider: Option<Box<dyn IdentityProvider>>,

    /// Database user (used when no identity provider is configured)
    #[serde(default)]
    pub user: String,

    /// Database password (used when no identity provider is configured)
    #[serde(default)]
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

// Manual Debug implementation to avoid trait object issues
impl std::fmt::Debug for MySqlStoredProcReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlStoredProcReactionConfig")
            .field("hostname", &self.hostname)
            .field("port", &self.port)
            .field("identity_provider", &self.identity_provider.is_some())
            .field("user", &self.user)
            .field("password", &"***")
            .field("database", &self.database)
            .field("ssl", &self.ssl)
            .field("routes", &self.routes)
            .field("default_template", &self.default_template)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("retry_attempts", &self.retry_attempts)
            .finish()
    }
}

impl Default for MySqlStoredProcReactionConfig {
    fn default() -> Self {
        Self {
            hostname: default_hostname(),
            port: None,
            identity_provider: None,
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

    /// Resolve the command template for a specific query and operation type.
    ///
    /// Resolution order (per the Reaction Developer Guide §11):
    /// 1. `routes[query_id]` for the operation.
    /// 2. `routes[last dotted segment of query_id]` for the operation.
    /// 3. `default_template` for the operation.
    ///
    /// Empty templates are treated as "not configured" and skipped.
    pub fn resolve_command_template(
        &self,
        query_id: &str,
        operation: common::OperationType,
    ) -> Option<String> {
        // 1. Full query id route.
        if let Some(template) = self
            .routes
            .get(query_id)
            .and_then(|qc| spec_for_operation(qc, operation))
        {
            return Some(template);
        }

        // 2. Last dotted segment route (e.g. `routes.my_query` for `source.my_query`).
        if let Some(segment) = query_id.rsplit('.').next() {
            if segment != query_id {
                if let Some(template) = self
                    .routes
                    .get(segment)
                    .and_then(|qc| spec_for_operation(qc, operation))
                {
                    return Some(template);
                }
            }
        }

        // 3. Default template.
        self.default_template
            .as_ref()
            .and_then(|qc| spec_for_operation(qc, operation))
    }

    /// Validate the configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        // A user is only required when no identity provider is configured;
        // identity-provider authentication resolves credentials at runtime.
        if self.identity_provider.is_none() && self.user.is_empty() {
            anyhow::bail!("Database user is required when no identity provider is configured");
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

        // Compile every configured template so invalid templates are rejected
        // at construction time rather than on the first inbound event.
        for (query_id, query_config) in &self.routes {
            validate_query_config_templates(query_config)
                .map_err(|e| anyhow::anyhow!("route '{query_id}': {e}"))?;
        }
        if let Some(default_template) = &self.default_template {
            validate_query_config_templates(default_template)
                .map_err(|e| anyhow::anyhow!("default_template: {e}"))?;
        }

        Ok(())
    }

    /// Validate that every `routes` key corresponds to a subscribed query id
    /// (or to the last dotted segment of one).
    pub fn validate_routes(&self, query_ids: &[String]) -> anyhow::Result<()> {
        for key in self.routes.keys() {
            let matches = query_ids.iter().any(|id| {
                id == key || id.rsplit('.').next().map(|seg| seg == key).unwrap_or(false)
            });
            if !matches {
                anyhow::bail!(
                    "route key '{key}' does not match any subscribed query id; \
                     configured queries: {query_ids:?}"
                );
            }
        }
        Ok(())
    }
}

/// Return the (non-empty) template for an operation from a `QueryConfig`.
fn spec_for_operation(qc: &QueryConfig, operation: common::OperationType) -> Option<String> {
    let spec = match operation {
        common::OperationType::Add => qc.added.as_ref(),
        common::OperationType::Update => qc.updated.as_ref(),
        common::OperationType::Delete => qc.deleted.as_ref(),
    }?;
    if spec.template.trim().is_empty() {
        None
    } else {
        Some(spec.template.clone())
    }
}

/// Compile-validate every template present in a `QueryConfig`.
fn validate_query_config_templates(qc: &QueryConfig) -> anyhow::Result<()> {
    for spec in [qc.added.as_ref(), qc.updated.as_ref(), qc.deleted.as_ref()]
        .into_iter()
        .flatten()
    {
        crate::render::CommandRenderer::validate_template(&spec.template)?;
    }
    Ok(())
}
