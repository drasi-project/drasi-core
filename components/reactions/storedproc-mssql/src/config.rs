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

//! Configuration for the MS SQL Server Stored Procedure reaction.

use drasi_lib::identity::IdentityProvider;
use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Re-export common template types for public API
pub use common::{OperationType, QueryConfig, TemplateSpec};

/// Configuration for the MS SQL Server Stored Procedure reaction
///
/// Commands are Handlebars templates that render to a SQL batch (typically an
/// `EXEC` statement). Row data is passed to the procedure with the `{{param}}`
/// helper, which binds each value as a positional parameter (`@P1`, `@P2`, …)
/// through the driver rather than interpolating it into the SQL text.
///
/// Commands can be configured at two levels:
/// 1. **Default templates**: Applied to all queries unless overridden (using `default_template`)
/// 2. **Per-query templates**: Override default for specific queries (using `routes`)
///
/// ## Template context
///
/// Templates have access to the following keys:
/// - `after` - The row after the change (available for ADD and UPDATE)
/// - `before` - The row before the change (available for UPDATE and DELETE)
/// - `data` - The raw data field (available for UPDATE)
/// - `query_name` / `query_id` - The id of the query that produced the result
/// - `operation` - The operation type ("ADD", "UPDATE", or "DELETE")
/// - `timestamp` - The result timestamp (RFC3339)
/// - `metadata` - The result metadata map
///
/// ## Template helpers
///
/// - `{{param <path>}}` binds the value at `<path>` as a positional SQL
///   parameter and renders the matching placeholder.
/// - `{{json <path>}}` renders the value at `<path>` as a JSON string; combine
///   it with `param` (e.g. `{{param (json after)}}`) to bind a whole object.
///
/// Rendering runs in strict mode: a template referencing a field that is absent
/// from the current row fails to render, and the event is skipped rather than a
/// command being executed with missing data.
///
/// ## Example with Default Template
///
/// ```rust,ignore
/// let config = MsSqlStoredProcReactionConfig {
///     hostname: "localhost".to_string(),
///     database: "mydb".to_string(),
///     user: "sa".to_string(),
///     password: "password".to_string(),
///     default_template: Some(QueryConfig {
///         added: Some(TemplateSpec::new("EXEC add_user {{param after.id}}, {{param after.name}}, {{param after.email}}")),
///         updated: Some(TemplateSpec::new("EXEC update_user {{param after.id}}, {{param after.name}}, {{param after.email}}")),
///         deleted: Some(TemplateSpec::new("EXEC delete_user {{param before.id}}")),
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
///     added: Some(TemplateSpec::new("EXEC add_user {{param after.id}}, {{param after.name}}, {{param after.email}}")),
///     updated: Some(TemplateSpec::new("EXEC update_user {{param after.id}}, {{param after.name}}, {{param after.email}}")),
///     deleted: Some(TemplateSpec::new("EXEC delete_user {{param before.id}}")),
/// });
///
/// let config = MsSqlStoredProcReactionConfig {
///     hostname: "localhost".to_string(),
///     database: "mydb".to_string(),
///     user: "sa".to_string(),
///     password: "password".to_string(),
///     routes,
///     ..Default::default()
/// };
/// ```
#[derive(Clone, Serialize, Deserialize)]
pub struct MsSqlStoredProcReactionConfig {
    /// Database hostname or IP address
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Database port (default: 1433)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    /// Identity provider for authentication (takes precedence over user/password)
    #[serde(skip)]
    pub identity_provider: Option<Box<dyn IdentityProvider>>,

    /// Database user (deprecated: use identity_provider instead)
    #[serde(default)]
    pub user: String,

    /// Database password (deprecated: use identity_provider instead)
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
impl std::fmt::Debug for MsSqlStoredProcReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsSqlStoredProcReactionConfig")
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

impl Default for MsSqlStoredProcReactionConfig {
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

impl TemplateRouting for MsSqlStoredProcReactionConfig {
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

impl MsSqlStoredProcReactionConfig {
    /// Get the port for the database, using the default if not specified
    pub fn get_port(&self) -> u16 {
        self.port.unwrap_or(1433)
    }

    /// Get the command template for a specific query and operation type
    ///
    /// Resolution order (developer-guide §11):
    /// 1. `routes[query_id]` for the operation (full wire id).
    /// 2. `routes[last dotted segment of query_id]` for the operation.
    /// 3. `default_template` for the operation.
    pub fn get_command_template(&self, query_id: &str, operation: OperationType) -> Option<String> {
        self.resolve_template_spec(query_id, operation)
            .map(|spec| spec.template.clone())
    }

    /// Resolve the [`TemplateSpec`] for a `(query_id, operation)` pair following
    /// the full three-step resolution order (per-query id → last dotted segment
    /// → default template). The [`TemplateRouting`] trait default only covers
    /// steps 1 and 3, so this adds the last-segment lookup in between.
    pub fn resolve_template_spec(
        &self,
        query_id: &str,
        operation: OperationType,
    ) -> Option<&TemplateSpec> {
        if let Some(spec) = self
            .routes
            .get(query_id)
            .and_then(|qc| Self::get_spec_from_config(qc, operation))
        {
            return Some(spec);
        }

        let segment = last_segment(query_id);
        if segment != query_id {
            if let Some(spec) = self
                .routes
                .get(segment)
                .and_then(|qc| Self::get_spec_from_config(qc, operation))
            {
                return Some(spec);
            }
        }

        self.default_template
            .as_ref()
            .and_then(|qc| Self::get_spec_from_config(qc, operation))
    }

    /// Validate the configuration against the reaction's subscribed queries.
    ///
    /// Called by the builder's `build()` so misconfiguration fails at
    /// construction rather than on the first inbound event. Checks:
    ///
    /// 1. `user` and `database` are set.
    /// 2. At least one command is configured (routes or default template).
    /// 3. Every command template compiles as a Handlebars template.
    /// 4. Every `routes` key matches a subscribed query id (or its last dotted
    ///    segment).
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
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

        // Compile every template at construction time.
        if let Some(default) = &self.default_template {
            validate_query_config(default)
                .map_err(|e| anyhow::anyhow!("invalid default template: {e}"))?;
        }
        for (key, qc) in &self.routes {
            validate_query_config(qc)
                .map_err(|e| anyhow::anyhow!("invalid template for route '{key}': {e}"))?;

            let matches = query_ids.iter().any(|q| q == key || last_segment(q) == key);
            if !matches {
                anyhow::bail!(
                    "route '{key}' does not match any subscribed query id (or its last dotted segment)"
                );
            }
        }

        Ok(())
    }
}

/// Last dotted segment of a query id (`source.q1` → `q1`).
fn last_segment(query_id: &str) -> &str {
    query_id.rsplit('.').next().unwrap_or(query_id)
}

/// Compile every command template in a per-query config.
fn validate_query_config(config: &QueryConfig) -> anyhow::Result<()> {
    for (op, spec) in [
        ("added", &config.added),
        ("updated", &config.updated),
        ("deleted", &config.deleted),
    ] {
        if let Some(spec) = spec {
            crate::render::validate_template(&spec.template)
                .map_err(|e| anyhow::anyhow!("'{op}': {e}"))?;
        }
    }
    Ok(())
}
