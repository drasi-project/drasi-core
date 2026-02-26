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

use anyhow::Result;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::channels::DispatchMode;
use drasi_lib::sources::base::SourceBaseParams;
use serde::{Deserialize, Serialize};

use crate::SqliteSource;

/// Element ID key configuration for a SQLite table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableKeyConfig {
    /// Table name.
    pub table: String,
    /// Ordered key column names used to build element IDs.
    pub key_columns: Vec<String>,
}

/// Runtime configuration for the optional REST API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestApiConfig {
    /// Address to bind.
    #[serde(default = "default_rest_host")]
    pub host: String,
    /// Port to bind.
    #[serde(default = "default_rest_port")]
    pub port: u16,
}

impl Default for RestApiConfig {
    fn default() -> Self {
        Self {
            host: default_rest_host(),
            port: default_rest_port(),
        }
    }
}

fn default_rest_host() -> String {
    "0.0.0.0".to_string()
}

fn default_rest_port() -> u16 {
    8080
}

/// Initial start behavior. SQLite source does not use persisted cursors, but this
/// is exposed for API consistency with other source plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StartFrom {
    /// Start immediately (default behavior).
    #[default]
    Beginning,
    /// Semantically equivalent to `Beginning` for embedded SQLite source.
    Now,
    /// Semantically equivalent to `Beginning` for embedded SQLite source.
    Timestamp(i64),
}

/// SQLite source configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SqliteSourceConfig {
    /// SQLite file path. When `None`, an in-memory database is used.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional explicit table allow-list. `None` means all user tables.
    #[serde(default)]
    pub tables: Option<Vec<String>>,
    /// Optional explicit key config for element ID generation.
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
    /// Optional REST API configuration.
    #[serde(default)]
    pub rest_api: Option<RestApiConfig>,
    /// Initial start behavior.
    #[serde(default)]
    pub start_from: StartFrom,
}

impl Default for SqliteSourceConfig {
    fn default() -> Self {
        Self {
            path: None,
            tables: None,
            table_keys: Vec::new(),
            rest_api: None,
            start_from: StartFrom::Beginning,
        }
    }
}

/// Builder for [`SqliteSource`].
pub struct SqliteSourceBuilder {
    pub(crate) id: String,
    path: Option<String>,
    tables: Option<Vec<String>>,
    table_keys: Vec<TableKeyConfig>,
    rest_api: Option<RestApiConfig>,
    start_from: StartFrom,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl SqliteSourceBuilder {
    /// Create a new builder with defaults.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            path: None,
            tables: None,
            table_keys: Vec::new(),
            rest_api: None,
            start_from: StartFrom::Beginning,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Use file-backed SQLite.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Use in-memory SQLite.
    pub fn in_memory(mut self) -> Self {
        self.path = None;
        self
    }

    /// Set monitored tables.
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = Some(tables);
        self
    }

    /// Set configured table keys.
    pub fn with_table_keys(mut self, table_keys: Vec<TableKeyConfig>) -> Self {
        self.table_keys = table_keys;
        self
    }

    /// Enable REST API.
    pub fn with_rest_api(mut self, config: RestApiConfig) -> Self {
        self.rest_api = Some(config);
        self
    }

    /// Set initial start behavior.
    pub fn start_from(mut self, start_from: StartFrom) -> Self {
        self.start_from = start_from;
        self
    }

    /// Set dispatch mode.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set bootstrap provider.
    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set source auto-start.
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Build source.
    pub fn build(self) -> Result<SqliteSource> {
        let config = SqliteSourceConfig {
            path: self.path,
            tables: self.tables,
            table_keys: self.table_keys,
            rest_api: self.rest_api,
            start_from: self.start_from,
        };

        let mut params = SourceBaseParams::new(self.id);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }
        params = params.with_auto_start(self.auto_start);

        let base = drasi_lib::sources::base::SourceBase::new(params)?;
        SqliteSource::from_parts(base, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Source;

    #[test]
    fn sqlite_source_config_defaults_are_stable() {
        let config = SqliteSourceConfig::default();
        assert!(config.path.is_none());
        assert!(config.tables.is_none());
        assert!(config.table_keys.is_empty());
        assert!(config.rest_api.is_none());
        assert_eq!(config.start_from, StartFrom::Beginning);
    }

    #[test]
    fn builder_applies_rest_and_dispatch_configuration() {
        let source = SqliteSource::builder("test-source")
            .with_path("/tmp/test.db")
            .with_tables(vec!["sensors".to_string()])
            .with_table_keys(vec![TableKeyConfig {
                table: "sensors".to_string(),
                key_columns: vec!["id".to_string()],
            }])
            .with_rest_api(RestApiConfig {
                host: "127.0.0.1".to_string(),
                port: 9001,
            })
            .with_dispatch_mode(DispatchMode::Channel)
            .with_dispatch_buffer_capacity(42)
            .auto_start(false)
            .build()
            .expect("failed to build sqlite source");

        let properties = source.properties();
        assert_eq!(
            properties.get("path"),
            Some(&serde_json::json!("/tmp/test.db"))
        );
        assert_eq!(
            properties.get("tables"),
            Some(&serde_json::json!(["sensors"]))
        );
        assert_eq!(
            properties.get("rest_api_enabled"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(!source.auto_start());
    }
}
