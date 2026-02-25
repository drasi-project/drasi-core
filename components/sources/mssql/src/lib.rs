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

//! Microsoft SQL Server CDC Source for Drasi
//!
//! This source plugin captures data changes from MS SQL Server databases using
//! Change Data Capture (CDC). It monitors CDC change tables and converts changes
//! to Drasi source events.
//!
//! # Features
//!
//! - Real-time change capture via CDC polling
//! - LSN-based progress tracking with StateStore persistence
//! - Automatic LSN validation and recovery
//! - Support for INSERT, UPDATE, DELETE operations
//! - Transaction grouping
//! - Primary key discovery and custom key configuration
//!
//! # Prerequisites
//!
//! Before using this source, you must:
//!
//! 1. Enable CDC on the database:
//!    ```sql
//!    EXEC sys.sp_cdc_enable_db;
//!    ```
//!
//! 2. Enable CDC on tables you want to monitor:
//!    ```sql
//!    EXEC sys.sp_cdc_enable_table
//!        @source_schema = N'dbo',
//!        @source_name = N'YourTable',
//!        @role_name = NULL;
//!    ```
//!
//! 3. Ensure SQL Server Agent is running (required for CDC)
//!
//! # Example
//!
//! ```no_run
//! use drasi_source_mssql::{MsSqlSource, StartPosition};
//! use drasi_lib::Source;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let source = MsSqlSource::builder("mssql-source")
//!     .with_host("localhost")
//!     .with_database("production")
//!     .with_user("drasi_user")
//!     .with_password("secure_password")
//!     .with_tables(vec!["orders".to_string(), "customers".to_string()])
//!     .with_start_position(StartPosition::Beginning)  // Capture all retained changes
//!     .build()?;
//!
//! source.start().await?;
//! # Ok(())
//! # }
//! ```

pub mod decoder;
pub mod descriptor;
pub mod lsn;
pub mod stream;
pub mod types;

// Re-export from drasi-mssql-common
pub use drasi_mssql_common::config;
pub use drasi_mssql_common::connection;
pub use drasi_mssql_common::error;
pub use drasi_mssql_common::keys;

// Re-export main types
pub use drasi_mssql_common::{
    validate_sql_identifier, AuthMode, ConnectionError, EncryptionMode, LsnError, MsSqlConnection,
    MsSqlError, MsSqlErrorKind, MsSqlSourceConfig, PrimaryKeyCache, PrimaryKeyError, StartPosition,
    TableKeyConfig,
};
pub use decoder::CdcOperation;
pub use lsn::Lsn;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::Source;
use drasi_lib::state_store::StateStoreProvider;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::sync::RwLock;

/// MS SQL CDC Source
///
/// Monitors MS SQL Server CDC change tables and emits source change events.
pub struct MsSqlSource {
    /// Source identifier
    source_id: String,

    /// Configuration
    config: MsSqlSourceConfig,

    /// Base source implementation (handles dispatching, status, etc.)
    base: SourceBase,

    /// State store for LSN persistence
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,

    /// CDC polling task handle
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,

    /// Shutdown signal sender for graceful shutdown
    shutdown_tx: watch::Sender<bool>,

    /// Shutdown signal receiver (cloned for each task)
    shutdown_rx: watch::Receiver<bool>,
}

impl MsSqlSource {
    /// Create a new MS SQL source with configuration
    ///
    /// # Arguments
    /// * `id` - Unique identifier for this source
    /// * `config` - MS SQL source configuration
    pub fn new(id: impl Into<String>, config: MsSqlSourceConfig) -> Result<Self> {
        let source_id = id.into();

        // Create base source parameters
        let params = SourceBaseParams::new(&source_id);

        // Create shutdown channel (false = running, true = shutdown)
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(Self {
            source_id,
            config,
            base: SourceBase::new(params)?,
            state_store: Arc::new(RwLock::new(None)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx,
            shutdown_rx,
        })
    }

    /// Create a builder for configuring the MS SQL source
    ///
    /// # Arguments
    /// * `id` - Unique identifier for this source
    ///
    /// # Example
    /// ```no_run
    /// use drasi_source_mssql::MsSqlSource;
    ///
    /// # fn example() -> anyhow::Result<()> {
    /// let source = MsSqlSource::builder("my-source")
    ///     .with_host("localhost")
    ///     .with_database("mydb")
    ///     .with_user("user")
    ///     .with_password("password")
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn builder(id: impl Into<String>) -> MsSqlSourceBuilder {
        MsSqlSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for MsSqlSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mssql"
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        let mut props = std::collections::HashMap::new();
        props.insert(
            "host".to_string(),
            serde_json::Value::String(self.config.host.clone()),
        );
        props.insert(
            "port".to_string(),
            serde_json::Value::Number(self.config.port.into()),
        );
        props.insert(
            "database".to_string(),
            serde_json::Value::String(self.config.database.clone()),
        );
        props.insert(
            "user".to_string(),
            serde_json::Value::String(self.config.user.clone()),
        );
        // Don't expose password
        props.insert(
            "tables".to_string(),
            serde_json::Value::Array(
                self.config
                    .tables
                    .iter()
                    .map(|t| serde_json::Value::String(t.clone()))
                    .collect(),
            ),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn status(&self) -> drasi_lib::channels::ComponentStatus {
        self.base.get_status().await
    }

    async fn start(&self) -> Result<()> {
        use drasi_lib::channels::ComponentStatus;

        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting).await;
        log::info!("Starting MS SQL CDC source: {}", self.base.id);

        let config = self.config.clone();
        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let state_store = self.state_store.read().await.clone();
        let shutdown_rx = self.shutdown_rx.clone();

        // Spawn CDC polling task
        let task_handle = tokio::spawn(async move {
            if let Err(e) = stream::run_cdc_stream(
                source_id.clone(),
                config,
                dispatchers,
                state_store,
                shutdown_rx,
            )
            .await
            {
                log::error!("CDC stream task failed for {source_id}: {e}");
            }
        });

        // Store task handle for shutdown
        *self.task_handle.write().await = Some(task_handle);

        self.base.set_status(ComponentStatus::Running).await;

        log::info!("MS SQL source '{}' started CDC polling", self.base.id);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        use drasi_lib::channels::ComponentStatus;

        log::info!("MS SQL source '{}' stopping", self.base.id);

        // Signal the CDC polling loop to stop gracefully
        if let Err(e) = self.shutdown_tx.send(true) {
            log::warn!("Failed to send shutdown signal: {e}");
        }

        // Wait for the task to complete (with timeout)
        if let Some(handle) = self.task_handle.write().await.take() {
            // Give the task a chance to shut down gracefully
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    log::debug!("CDC polling task stopped gracefully");
                }
                Ok(Err(e)) => {
                    log::warn!("CDC polling task panicked: {e}");
                }
                Err(_) => {
                    log::warn!("CDC polling task did not stop within timeout, it will be dropped");
                }
            }
        }

        self.base.set_status(ComponentStatus::Stopped).await;

        Ok(())
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<drasi_lib::channels::SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "MS SQL")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;

        // Store state store if provided
        if let Some(state_store) = context.state_store {
            *self.state_store.write().await = Some(state_store);
            log::debug!("State store injected into MS SQL source '{}'", self.base.id);
        }
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for MS SQL source
///
/// Provides a fluent API for constructing an MS SQL source with validation.
pub struct MsSqlSourceBuilder {
    id: String,
    config: MsSqlSourceConfig,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
}

impl MsSqlSourceBuilder {
    /// Create a new builder
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: MsSqlSourceConfig::default(),
            bootstrap_provider: None,
        }
    }

    /// Set the MS SQL server hostname
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.config.host = host.into();
        self
    }

    /// Set the MS SQL server port
    pub fn with_port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    /// Set the database name
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.config.database = database.into();
        self
    }

    /// Set the database user
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.config.user = user.into();
        self
    }

    /// Set the database password
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.config.password = password.into();
        self
    }

    /// Set the authentication mode
    pub fn with_auth_mode(mut self, auth_mode: AuthMode) -> Self {
        self.config.auth_mode = auth_mode;
        self
    }

    /// Set the list of tables to monitor
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.config.tables = tables;
        self
    }

    /// Add a single table to monitor
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.config.tables.push(table.into());
        self
    }

    /// Set the CDC polling interval in milliseconds
    pub fn with_poll_interval_ms(mut self, ms: u64) -> Self {
        self.config.poll_interval_ms = ms;
        self
    }

    /// Set the encryption mode
    pub fn with_encryption(mut self, encryption: EncryptionMode) -> Self {
        self.config.encryption = encryption;
        self
    }

    /// Set whether to trust the server certificate
    pub fn with_trust_server_certificate(mut self, trust: bool) -> Self {
        self.config.trust_server_certificate = trust;
        self
    }

    /// Add a table key configuration
    pub fn with_table_key(mut self, table: impl Into<String>, key_columns: Vec<String>) -> Self {
        self.config.table_keys.push(TableKeyConfig {
            table: table.into(),
            key_columns,
        });
        self
    }

    /// Set the starting position when no LSN is found in state store
    pub fn with_start_position(mut self, position: StartPosition) -> Self {
        self.config.start_position = position;
        self
    }

    /// Set the bootstrap provider for initial data delivery
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Build the MS SQL source
    ///
    /// # Errors
    /// Returns error if required configuration is missing
    pub fn build(self) -> Result<MsSqlSource> {
        // Validate required fields
        if self.config.database.is_empty() {
            return Err(anyhow::anyhow!("Database name is required"));
        }
        if self.config.user.is_empty() {
            return Err(anyhow::anyhow!("Database user is required"));
        }

        let source_id = self.id.clone();

        // Create base source parameters
        let mut params = SourceBaseParams::new(&source_id);

        // Add bootstrap provider if configured
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        // Create shutdown channel (false = running, true = shutdown)
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(MsSqlSource {
            source_id,
            config: self.config,
            base: SourceBase::new(params)?,
            state_store: Arc::new(RwLock::new(None)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx,
            shutdown_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_basic() {
        let source = MsSqlSource::builder("test-source")
            .with_host("localhost")
            .with_database("testdb")
            .with_user("testuser")
            .with_password("testpass")
            .build()
            .unwrap();

        assert_eq!(source.id(), "test-source");
        assert_eq!(source.type_name(), "mssql");
        assert_eq!(source.config.host, "localhost");
        assert_eq!(source.config.database, "testdb");
    }

    #[test]
    fn test_builder_with_tables() {
        let source = MsSqlSource::builder("test-source")
            .with_database("testdb")
            .with_user("testuser")
            .with_tables(vec!["table1".to_string(), "table2".to_string()])
            .build()
            .unwrap();

        assert_eq!(source.config.tables.len(), 2);
    }

    #[test]
    fn test_builder_missing_required_fields() {
        let result = MsSqlSource::builder("test-source")
            .with_host("localhost")
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_builder_table_keys() {
        let source = MsSqlSource::builder("test-source")
            .with_database("testdb")
            .with_user("testuser")
            .with_table_key("orders", vec!["order_id".to_string()])
            .build()
            .unwrap();

        assert_eq!(source.config.table_keys.len(), 1);
        assert_eq!(source.config.table_keys[0].table, "orders");
    }
}

/// Dynamic plugin entry point.
///
/// # Safety
/// The caller must ensure this is only called once and takes ownership of the
/// returned pointer via `Box::from_raw`.
#[no_mangle]
pub extern "C" fn drasi_source_mssql_plugin_init() -> *mut drasi_plugin_sdk::PluginRegistration {
    let registration = drasi_plugin_sdk::PluginRegistration::new()
        .with_source(Box::new(descriptor::MsSqlSourceDescriptor));
    Box::into_raw(Box::new(registration))
}
