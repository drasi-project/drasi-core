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

//! Bootstrap provider architecture for Drasi
//!
//! This module provides a pluggable bootstrap system that separates bootstrap
//! concerns from source streaming logic. Bootstrap providers can be reused
//! across different source types while maintaining access to their parent
//! source configuration.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::config::SourceConfig;

pub mod providers;
pub mod script_reader;
pub mod script_types;

/// Request for bootstrap data from a query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRequest {
    pub query_id: String,
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    pub request_id: String,
}

/// Context passed to bootstrap providers containing source configuration
/// Bootstrap happens through dedicated channels created in source.subscribe().
#[derive(Clone)]
pub struct BootstrapContext {
    /// Unique server ID for logging and tracing
    pub server_id: String,
    /// The parent source configuration
    pub source_config: Arc<SourceConfig>,
    /// Source ID for labeling bootstrap events
    pub source_id: String,
    /// Sequence counter for bootstrap events
    pub sequence_counter: Arc<AtomicU64>,
}

impl BootstrapContext {
    pub fn new(
        server_id: String,
        source_config: Arc<SourceConfig>,
        source_id: String,
    ) -> Self {
        Self {
            server_id,
            source_config,
            source_id,
            sequence_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the next sequence number for bootstrap events
    pub fn next_sequence(&self) -> u64 {
        self.sequence_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Get a property from the source configuration
    pub fn get_property(&self, key: &str) -> Option<&serde_json::Value> {
        self.source_config.properties.get(key)
    }

    /// Get a typed property from the source configuration
    pub fn get_typed_property<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        match self.get_property(key) {
            Some(value) => Ok(Some(serde_json::from_value(value.clone())?)),
            None => Ok(None),
        }
    }
}

use crate::channels::{BootstrapEventSender};

/// Trait for bootstrap providers that handle initial data delivery
/// for newly subscribed queries
#[async_trait]
pub trait BootstrapProvider: Send + Sync {
    /// Perform bootstrap operation for the given request
    /// Sends bootstrap events to the provided channel
    /// Returns the number of elements sent
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize>;
}

/// Configuration for different types of bootstrap providers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BootstrapProviderConfig {
    /// PostgreSQL bootstrap provider
    Postgres {
        /// Additional configuration specific to PostgreSQL bootstrap
        #[serde(flatten)]
        config: HashMap<String, serde_json::Value>,
    },
    /// Application-based bootstrap provider
    Application {
        /// Additional configuration for application bootstrap
        #[serde(flatten)]
        config: HashMap<String, serde_json::Value>,
    },
    /// Script file bootstrap provider
    ScriptFile {
        /// List of JSONL files to read (in order)
        file_paths: Vec<String>,
    },
    /// Platform bootstrap provider for remote Drasi sources
    /// Bootstraps data from a Query API service running in a remote Drasi environment
    Platform {
        /// URL of the Query API service (e.g., "http://my-source-query-api:8080")
        /// If not specified, falls back to `query_api_url` property from source config
        #[serde(default)]
        query_api_url: Option<String>,
        /// Timeout for HTTP requests in seconds (default: 300)
        #[serde(default)]
        timeout_seconds: Option<u64>,
        /// Additional configuration
        #[serde(flatten)]
        config: HashMap<String, serde_json::Value>,
    },
    /// No-op bootstrap provider (returns no data)
    Noop,
}

/// Factory for creating bootstrap providers from configuration
pub struct BootstrapProviderFactory;

impl BootstrapProviderFactory {
    /// Create a bootstrap provider from configuration
    pub fn create_provider(config: &BootstrapProviderConfig) -> Result<Box<dyn BootstrapProvider>> {
        match config {
            BootstrapProviderConfig::Postgres { .. } => Ok(Box::new(
                providers::postgres::PostgresBootstrapProvider::new(),
            )),
            BootstrapProviderConfig::Application { .. } => Ok(Box::new(
                providers::application::ApplicationBootstrapProvider::new(),
            )),
            BootstrapProviderConfig::ScriptFile { file_paths } => Ok(Box::new(
                providers::script_file::ScriptFileBootstrapProvider::new(file_paths.clone()),
            )),
            BootstrapProviderConfig::Platform {
                query_api_url,
                timeout_seconds,
                ..
            } => {
                // Use provided query_api_url or will be extracted from source properties later
                let url = query_api_url.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "query_api_url must be provided in bootstrap config or source properties"
                    )
                })?;

                let timeout = timeout_seconds.unwrap_or(300);

                Ok(Box::new(
                    providers::platform::PlatformBootstrapProvider::new(url, timeout)?,
                ))
            }
            BootstrapProviderConfig::Noop => {
                Ok(Box::new(providers::noop::NoOpBootstrapProvider::new()))
            }
        }
    }
}
