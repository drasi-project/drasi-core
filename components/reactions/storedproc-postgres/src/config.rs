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

//! Configuration for the PostgreSQL Stored Procedure reaction.

use drasi_lib::identity::IdentityProvider;
use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

// Re-export common template types for public API
pub use common::{QueryConfig, TemplateSpec};

/// Configuration for the PostgreSQL Stored Procedure reaction
///
/// Stored-procedure commands are configured as [Handlebars] templates. Argument
/// values are referenced with the `{{param <expr>}}` helper, which binds them as
/// positional SQL parameters (`$1`, `$2`, …) so untrusted row data can never
/// alter the command structure. The `{{json <expr>}}` helper is also available
/// for embedding a serialized JSON value into literal SQL text.
///
/// Commands can be configured at two levels:
/// 1. **Default templates**: Applied to all queries unless overridden (using `default_template`)
/// 2. **Per-query templates**: Override the default for specific queries (using `routes`)
///
/// ## Template context
///
/// Each template is rendered against a standard context with these keys:
/// - `after` - The post-change row (available for ADD and UPDATE)
/// - `before` - The pre-change row (available for UPDATE and DELETE)
/// - `data` - The raw `data` payload of an Update diff (available for UPDATE)
/// - `query_id` / `query_name` - The id of the query that produced the result
/// - `operation` - The operation type (`"ADD"`, `"UPDATE"`, or `"DELETE"`)
/// - `timestamp` - RFC3339 result timestamp
/// - `metadata` - The result's metadata map
///
/// ## Example with Default Template
///
/// ```rust,ignore
/// let config = PostgresStoredProcReactionConfig {
///     hostname: "localhost".to_string(),
///     database: "mydb".to_string(),
///     user: "postgres".to_string(),
///     password: "password".to_string(),
///     default_template: Some(QueryConfig {
///         added: Some(TemplateSpec::new("CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
///         updated: Some(TemplateSpec::new("CALL update_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
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
///     added: Some(TemplateSpec::new("CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
///     updated: Some(TemplateSpec::new("CALL update_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
///     deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
/// });
///
/// let config = PostgresStoredProcReactionConfig {
///     hostname: "localhost".to_string(),
///     database: "mydb".to_string(),
///     user: "postgres".to_string(),
///     password: "password".to_string(),
///     routes,
///     ..Default::default()
/// };
/// ```
///
/// [Handlebars]: https://crates.io/crates/handlebars
#[derive(Clone, Serialize, Deserialize)]
pub struct PostgresStoredProcReactionConfig {
    /// Database hostname or IP address
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Database port (default: 5432)
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

// Manual Debug implementation to avoid issues with trait objects
impl std::fmt::Debug for PostgresStoredProcReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStoredProcReactionConfig")
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

impl Default for PostgresStoredProcReactionConfig {
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

impl TemplateRouting for PostgresStoredProcReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }

    /// Resolve the command template for a `(query_id, operation)` pair following
    /// the developer-guide resolution order:
    ///
    /// 1. per-query route keyed by the **full** query id,
    /// 2. per-query route keyed by the **last dotted segment**,
    /// 3. the shared `default_template`.
    ///
    /// Overrides the trait default (steps 1 and 3 only) so `source.my_query`
    /// can be routed via a `my_query` key.
    fn get_template_spec(
        &self,
        query_id: &str,
        operation: common::OperationType,
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
}

/// Last dotted segment of a query id (`source.q1` -> `q1`).
fn last_segment(query_id: &str) -> &str {
    query_id.rsplit('.').next().unwrap_or(query_id)
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

impl PostgresStoredProcReactionConfig {
    /// Get the port for the database, using the default if not specified
    pub fn get_port(&self) -> u16 {
        self.port.unwrap_or(5432)
    }

    /// Validate the configuration.
    ///
    /// Checks authentication, database name, that at least one command is
    /// configured, that every template compiles, and that every `routes` key
    /// matches a subscribed query id (or its last dotted segment).
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        // Check authentication configuration
        if self.identity_provider.is_none() && self.user.is_empty() {
            anyhow::bail!("Either identity_provider or user/password must be provided");
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
            validate_query_config(default).map_err(|e| anyhow::anyhow!("default_template: {e}"))?;
        }
        for (key, qc) in &self.routes {
            validate_query_config(qc).map_err(|e| anyhow::anyhow!("route '{key}': {e}"))?;

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

/// Compile every template in a [`QueryConfig`].
fn validate_query_config(config: &QueryConfig) -> anyhow::Result<()> {
    for spec in [&config.added, &config.updated, &config.deleted]
        .into_iter()
        .flatten()
    {
        crate::render::validate_template(&spec.template)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::OperationType;

    fn base_config() -> PostgresStoredProcReactionConfig {
        PostgresStoredProcReactionConfig {
            user: "testuser".to_string(),
            database: "testdb".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn validate_rejects_invalid_template() {
        let mut config = base_config();
        config.default_template = Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_user({{param after.id)")),
            updated: None,
            deleted: None,
        });

        let err = config.validate(&[]).unwrap_err().to_string();
        assert!(err.contains("default_template"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_unknown_route_key() {
        let mut config = base_config();
        let mut routes = HashMap::new();
        routes.insert(
            "unknown-query".to_string(),
            QueryConfig {
                added: Some(TemplateSpec::new("CALL add_user({{param after.id}})")),
                updated: None,
                deleted: None,
            },
        );
        config.routes = routes;

        let err = config
            .validate(&["some-other-query".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown-query"), "unexpected error: {err}");
    }

    #[test]
    fn validate_accepts_route_key_matching_last_segment() {
        let mut config = base_config();
        let mut routes = HashMap::new();
        routes.insert(
            "my-query".to_string(),
            QueryConfig {
                added: Some(TemplateSpec::new("CALL add_user({{param after.id}})")),
                updated: None,
                deleted: None,
            },
        );
        config.routes = routes;

        assert!(config.validate(&["source.my-query".to_string()]).is_ok());
    }

    #[test]
    fn resolution_prefers_full_id_route_over_default() {
        let mut config = base_config();
        config.default_template = Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL default_add({{param after.id}})")),
            updated: None,
            deleted: None,
        });
        let mut routes = HashMap::new();
        routes.insert(
            "source.q1".to_string(),
            QueryConfig {
                added: Some(TemplateSpec::new("CALL route_add({{param after.id}})")),
                updated: None,
                deleted: None,
            },
        );
        config.routes = routes;

        let spec = config
            .get_template_spec("source.q1", OperationType::Add)
            .unwrap();
        assert_eq!(spec.template, "CALL route_add({{param after.id}})");
    }

    #[test]
    fn resolution_matches_last_segment_then_default() {
        let mut config = base_config();
        config.default_template = Some(QueryConfig {
            added: None,
            updated: Some(TemplateSpec::new("CALL default_update({{param after.id}})")),
            deleted: None,
        });
        let mut routes = HashMap::new();
        routes.insert(
            "q1".to_string(),
            QueryConfig {
                added: Some(TemplateSpec::new("CALL segment_add({{param after.id}})")),
                updated: None,
                deleted: None,
            },
        );
        config.routes = routes;

        // ADD resolves via the last-segment route.
        let add = config
            .get_template_spec("source.q1", OperationType::Add)
            .unwrap();
        assert_eq!(add.template, "CALL segment_add({{param after.id}})");

        // UPDATE has no route entry, so it falls back to the default template.
        let update = config
            .get_template_spec("source.q1", OperationType::Update)
            .unwrap();
        assert_eq!(update.template, "CALL default_update({{param after.id}})");
    }
}
