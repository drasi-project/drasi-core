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

//! Configuration types for the file reaction.

use anyhow::anyhow;
use drasi_lib::reactions::common::{self, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use common::{QueryConfig, TemplateSpec};

/// File write behavior for emitted query result diffs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Appends each rendered record to the destination file.
    #[default]
    Append,
    /// Replaces destination file content with the latest rendered record.
    Overwrite,
    /// Writes each rendered record to a unique file.
    PerChange,
}

impl WriteMode {
    /// Returns the wire/string representation of the write mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            WriteMode::Append => "append",
            WriteMode::Overwrite => "overwrite",
            WriteMode::PerChange => "per_change",
        }
    }

    /// Returns the default Handlebars filename template for this write mode.
    pub fn default_filename_template(&self) -> &'static str {
        match self {
            WriteMode::Append => "{{query_name}}.log",
            WriteMode::Overwrite => "{{query_name}}.json",
            WriteMode::PerChange => "{{query_name}}_{{operation}}_{{uuid}}.json",
        }
    }
}

fn default_output_path() -> String {
    ".".to_string()
}

/// Configuration for the file reaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReactionConfig {
    /// Base directory for all output files.
    #[serde(default = "default_output_path")]
    pub output_path: String,

    /// How output should be persisted.
    #[serde(default)]
    pub write_mode: WriteMode,

    /// Handlebars filename template.
    ///
    /// If omitted, the reaction uses a default template based on `write_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename_template: Option<String>,

    /// Query-specific templates.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Default templates used when no query-specific route is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
}

impl Default for FileReactionConfig {
    fn default() -> Self {
        Self {
            output_path: default_output_path(),
            write_mode: WriteMode::Append,
            filename_template: None,
            routes: HashMap::new(),
            default_template: None,
        }
    }
}

impl FileReactionConfig {
    /// Validates the configuration's internal invariants.
    ///
    /// Confirms that `output_path` is non-empty and that every configured
    /// Handlebars template (filename, routes, and default) compiles. Route
    /// coverage against the subscribed query list is validated separately by
    /// the builder, where the query list is available.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.output_path.trim().is_empty() {
            return Err(anyhow!("output_path must not be empty"));
        }

        if let Some(filename_template) = &self.filename_template {
            validate_template(filename_template)
                .map_err(|e| anyhow!("Invalid filename template: {e}"))?;
        } else {
            validate_template(self.write_mode.default_filename_template())
                .map_err(|e| anyhow!("Invalid default filename template: {e}"))?;
        }

        for (query_id, route_config) in &self.routes {
            validate_query_config(route_config)
                .map_err(|e| anyhow!("Invalid template in route '{query_id}': {e}"))?;
        }

        if let Some(default_template) = &self.default_template {
            validate_query_config(default_template)
                .map_err(|e| anyhow!("Invalid default template: {e}"))?;
        }

        Ok(())
    }
}

/// Compiles a single Handlebars template, returning an error on invalid syntax.
///
/// An empty template is treated as valid (the reaction falls back to its
/// default output structure).
pub(crate) fn validate_template(template: &str) -> anyhow::Result<()> {
    if template.is_empty() {
        return Ok(());
    }

    handlebars::Template::compile(template).map_err(|e| anyhow!("Invalid template: {e}"))?;
    Ok(())
}

/// Compiles every template in a [`QueryConfig`].
pub(crate) fn validate_query_config(config: &QueryConfig) -> anyhow::Result<()> {
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

impl TemplateRouting for FileReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}
