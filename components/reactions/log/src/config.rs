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

//! Configuration types for the log reaction.

use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Re-export the common template types so callers can build configuration
// without depending on `drasi-lib` directly.
pub use common::{QueryConfig, TemplateSpec};

/// Log reaction configuration.
///
/// Supports Handlebars templates for formatting each event type. When no
/// template applies, the reaction falls back to a human-readable line showing
/// the operation and the raw row JSON.
///
/// Templates can be configured at two levels:
/// 1. **Default template** ([`default_template`](Self::default_template)) —
///    applied to every query unless a route overrides it.
/// 2. **Per-query templates** ([`routes`](Self::routes)) — override the
///    default for a specific query.
///
/// ## Template context
///
/// Every render is given the following standard context keys (when
/// applicable to the operation):
/// - `query_id` / `query_name` — the id of the query that produced the result
/// - `operation` — `"ADD"`, `"UPDATE"`, or `"DELETE"`
/// - `timestamp` — RFC 3339 timestamp of the emission
/// - `sequence_id` — monotonic per-query sequence number
/// - `metadata` — the query result metadata object
/// - `after` — the row after the change (ADD and UPDATE)
/// - `before` — the row before the change (UPDATE and DELETE)
/// - `data` — the raw data field (UPDATE)
///
/// The `{{json value}}` helper is registered for embedding a value as JSON.
///
/// ## Route resolution
///
/// For a given query id and operation, a template is resolved in this order:
/// 1. an exact [`routes`](Self::routes) entry for the query id,
/// 2. a [`routes`](Self::routes) entry matching the last dotted segment of the
///    query id (e.g. route `sensors` matches query `source.sensors`),
/// 3. the [`default_template`](Self::default_template),
/// 4. the built-in human-readable default line.
///
/// ## Example with a default template
///
/// ```rust
/// use drasi_reaction_log::{LogReactionConfig, QueryConfig, TemplateSpec};
/// use std::collections::HashMap;
///
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
/// ## Example with a per-query template
///
/// ```rust
/// use drasi_reaction_log::{LogReactionConfig, QueryConfig, TemplateSpec};
/// use std::collections::HashMap;
///
/// let mut routes = HashMap::new();
/// routes.insert(
///     "sensor-query".to_string(),
///     QueryConfig {
///         added: Some(TemplateSpec::new("[SENSOR] New: {{after.id}}")),
///         updated: None, // Falls back to the default template
///         deleted: None, // Falls back to the default template
///     },
/// );
///
/// let config = LogReactionConfig {
///     routes,
///     default_template: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LogReactionConfig {
    /// Query-specific template configurations.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Default template configuration used when no query-specific route is
    /// defined. If not set, falls back to the built-in human-readable line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
}

impl LogReactionConfig {
    /// Validate the configuration against the set of subscribed query ids.
    ///
    /// This checks that every configured Handlebars template compiles and that
    /// every route key corresponds to a subscribed query (either by exact match
    /// or by matching the last dotted segment of a query id).
    ///
    /// # Errors
    ///
    /// Returns an error if any template has invalid Handlebars syntax, or if a
    /// route key does not match any subscribed query.
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        // Validate every template in the routes.
        for (query_id, route_config) in &self.routes {
            validate_query_config(route_config)
                .map_err(|e| anyhow::anyhow!("Invalid template in route '{query_id}': {e}"))?;
        }

        // Validate the default template, if provided.
        if let Some(default_template) = &self.default_template {
            validate_query_config(default_template)
                .map_err(|e| anyhow::anyhow!("Invalid default template: {e}"))?;
        }

        // Validate that every route corresponds to a subscribed query.
        if !self.routes.is_empty() && !query_ids.is_empty() {
            for route_query in self.routes.keys() {
                let dotted_route = format!(".{route_query}");
                let matches = query_ids
                    .iter()
                    .any(|q| q == route_query || q.ends_with(&dotted_route));
                if !matches {
                    return Err(anyhow::anyhow!(
                        "Route '{route_query}' does not match any subscribed query. Subscribed queries: {query_ids:?}"
                    ));
                }
            }
        }

        Ok(())
    }
}

impl TemplateRouting for LogReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}

/// Validate a single Handlebars template string by compiling it.
///
/// An empty template is valid; it signals that the built-in default output
/// should be used for that operation.
fn validate_template(template: &str) -> anyhow::Result<()> {
    if template.is_empty() {
        return Ok(());
    }
    handlebars::Template::compile(template)
        .map_err(|e| anyhow::anyhow!("Invalid template: {e}"))?;
    Ok(())
}

/// Validate every template in a [`QueryConfig`].
fn validate_query_config(config: &QueryConfig) -> anyhow::Result<()> {
    if let Some(added) = &config.added {
        validate_template(&added.template)?;
    }
    if let Some(updated) = &config.updated {
        validate_template(&updated.template)?;
    }
    if let Some(deleted) = &config.deleted {
        validate_template(&deleted.template)?;
    }
    Ok(())
}
