#![allow(unexpected_cfgs)]
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

//! PostgreSQL Replication Source Plugin for Drasi
//!
//! This plugin captures data changes from PostgreSQL databases using logical replication.
//! It connects to PostgreSQL as a replication client and decodes Write-Ahead Log (WAL)
//! messages in real-time, converting them to Drasi source change events.
//!
//! # Prerequisites
//!
//! Before using this source, you must configure PostgreSQL for logical replication:
//!
//! 1. **Enable logical replication** in `postgresql.conf`:
//!    ```text
//!    wal_level = logical
//!    max_replication_slots = 10
//!    max_wal_senders = 10
//!    ```
//!
//! 2. **Create a publication** for the tables you want to monitor:
//!    ```sql
//!    CREATE PUBLICATION drasi_publication FOR TABLE users, orders;
//!    ```
//!
//! 3. **Create a replication slot** (optional - the source can create one automatically):
//!    ```sql
//!    SELECT pg_create_logical_replication_slot('drasi_slot', 'pgoutput');
//!    ```
//!
//! 4. **Grant replication permissions** to the database user:
//!    ```sql
//!    ALTER ROLE drasi_user REPLICATION;
//!    GRANT SELECT ON TABLE users, orders TO drasi_user;
//!    ```
//!
//! # Architecture
//!
//! The source has two main components:
//!
//! - **Bootstrap Handler**: Performs an initial snapshot of table data when a query
//!   subscribes with bootstrap enabled. Uses the replication slot's snapshot LSN to
//!   ensure consistency.
//!
//! - **Streaming Handler**: Continuously reads WAL messages and decodes them using
//!   the `pgoutput` protocol. Handles INSERT, UPDATE, and DELETE operations.
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `host` | string | `"localhost"` | PostgreSQL host |
//! | `port` | u16 | `5432` | PostgreSQL port |
//! | `database` | string | *required* | Database name |
//! | `user` | string | *required* | Database user (must have replication permission) |
//! | `password` | string | `""` | Database password |
//! | `tables` | string[] | `[]` | Tables to replicate |
//! | `slot_name` | string | `"drasi_slot"` | Replication slot name |
//! | `publication_name` | string | `"drasi_publication"` | Publication name |
//! | `ssl_mode` | string | `"prefer"` | SSL mode: disable, prefer, require |
//! | `table_keys` | TableKeyConfig[] | `[]` | Primary key configuration for tables |
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: postgres
//! properties:
//!   host: db.example.com
//!   port: 5432
//!   database: production
//!   user: replication_user
//!   password: secret
//!   tables:
//!     - users
//!     - orders
//!   slot_name: drasi_slot
//!   publication_name: drasi_publication
//!   table_keys:
//!     - table: users
//!       key_columns: [id]
//!     - table: orders
//!       key_columns: [order_id]
//! ```
//!
//! # Data Format
//!
//! The PostgreSQL source decodes WAL messages and converts them to Drasi source changes.
//! Each row change is mapped as follows:
//!
//! ## Node Mapping
//!
//! - **Element ID**: `{schema}:{table}:{primary_key_value}` (e.g., `public:users:123`)
//! - **Labels**: `[{table_name}]` (e.g., `["users"]`)
//! - **Properties**: All columns from the row (column names become property keys)
//!
//! ## WAL Message to SourceChange
//!
//! | WAL Operation | SourceChange |
//! |---------------|--------------|
//! | INSERT | `SourceChange::Insert { element: Node }` |
//! | UPDATE | `SourceChange::Update { element: Node }` |
//! | DELETE | `SourceChange::Delete { metadata }` |
//!
//! ## Example Mapping
//!
//! Given a PostgreSQL table:
//!
//! ```sql
//! CREATE TABLE users (
//!     id SERIAL PRIMARY KEY,
//!     name VARCHAR(100),
//!     email VARCHAR(255),
//!     age INTEGER
//! );
//!
//! INSERT INTO users (name, email, age) VALUES ('Alice', 'alice@example.com', 30);
//! ```
//!
//! Produces a SourceChange equivalent to:
//!
//! ```json
//! {
//!     "type": "Insert",
//!     "element": {
//!         "metadata": {
//!             "element_id": "public:users:1",
//!             "source_id": "pg-source",
//!             "labels": ["users"],
//!             "effective_from": 1699900000000000
//!         },
//!         "properties": {
//!             "id": 1,
//!             "name": "Alice",
//!             "email": "alice@example.com",
//!             "age": 30
//!         }
//!     }
//! }
//! ```
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use drasi_source_postgres::{PostgresReplicationSource, PostgresSourceBuilder};
//! use std::sync::Arc;
//!
//! let config = PostgresSourceBuilder::new()
//!     .with_host("db.example.com")
//!     .with_database("production")
//!     .with_user("replication_user")
//!     .with_password("secret")
//!     .with_tables(vec!["users".to_string(), "orders".to_string()])
//!     .build();
//!
//! let source = Arc::new(PostgresReplicationSource::new("pg-source", config)?);
//! drasi.add_source(source).await?;
//! ```

pub mod config;
pub mod connection;
pub mod decoder;
pub mod descriptor;
pub mod protocol;
pub mod scram;
pub mod stream;
pub mod types;

pub use config::{PostgresSourceConfig, SslMode, TableKeyConfig};

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::{DispatchMode, *};
use drasi_lib::component_graph::ComponentStatusHandle;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use tracing::Instrument;

/// PostgreSQL replication source that captures changes via logical replication.
///
/// This source connects to PostgreSQL using the replication protocol and decodes
/// WAL messages in real-time, converting them to Drasi source change events.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: PostgreSQL connection and replication configuration
pub struct PostgresReplicationSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// PostgreSQL source configuration
    config: PostgresSourceConfig,
}

impl PostgresReplicationSource {
    /// Create a builder for PostgresReplicationSource
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_postgres::PostgresReplicationSource;
    ///
    /// let source = PostgresReplicationSource::builder("pg-source")
    ///     .with_host("db.example.com")
    ///     .with_database("production")
    ///     .with_user("replication_user")
    ///     .with_password("secret")
    ///     .with_tables(vec!["users".to_string(), "orders".to_string()])
    ///     .with_bootstrap_provider(my_provider)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> PostgresSourceBuilder {
        PostgresSourceBuilder::new(id)
    }

    /// Create a new PostgreSQL replication source.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - PostgreSQL source configuration
    ///
    /// # Returns
    ///
    /// A new `PostgresReplicationSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_postgres::{PostgresReplicationSource, PostgresSourceBuilder};
    ///
    /// let config = PostgresSourceBuilder::new()
    ///     .with_host("db.example.com")
    ///     .with_database("mydb")
    ///     .with_user("replication_user")
    ///     .build();
    ///
    /// let source = PostgresReplicationSource::new("my-pg-source", config)?;
    /// ```
    pub fn new(id: impl Into<String>, config: PostgresSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    /// Create a new PostgreSQL replication source with custom dispatch settings
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    pub fn with_dispatch(
        id: impl Into<String>,
        config: PostgresSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        let id = id.into();
        let mut params = SourceBaseParams::new(id);
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }
}

#[async_trait]
impl Source for PostgresReplicationSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "postgres"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::PostgresSourceConfigDto;

        self.base
            .properties_or_serialize(&PostgresSourceConfigDto::from(&self.config))
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting, None).await;
        info!("Starting PostgreSQL replication source: {}", self.base.id);

        let config = self.config.clone();
        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let reporter = self.base.status_handle();

        // Get instance_id from context for log routing isolation
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        // Create span for spawned task so log::info!, log::error! etc are routed
        let source_id_for_span = source_id.clone();
        let span = tracing::info_span!(
            "postgres_replication_task",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );

        let task = tokio::spawn(
            async move {
                if let Err(e) =
                    run_replication(source_id.clone(), config, dispatchers, reporter.clone()).await
                {
                    error!("Replication task failed for {source_id}: {e}");
                    reporter
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Replication failed: {e}")),
                        )
                        .await;
                }
            }
            .instrument(span),
        );

        *self.base.task_handle.write().await = Some(task);
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("PostgreSQL replication started".to_string()),
            )
            .await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await != ComponentStatus::Running {
            return Ok(());
        }

        info!("Stopping PostgreSQL replication source: {}", self.base.id);

        self.base.set_status(ComponentStatus::Stopping, None).await;

        // Cancel the replication task
        if let Some(task) = self.base.task_handle.write().await.take() {
            task.abort();
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("PostgreSQL replication stopped".to_string()),
            )
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "PostgreSQL")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

async fn run_replication(
    source_id: String,
    config: PostgresSourceConfig,
    dispatchers: Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    status_handle: ComponentStatusHandle,
) -> Result<()> {
    info!("Starting replication for source {source_id}");

    let mut stream = stream::ReplicationStream::new(config, source_id, dispatchers, status_handle);

    stream.run().await
}

/// Builder for PostgreSQL source configuration.
///
/// Provides a fluent API for constructing PostgreSQL source configurations
/// with sensible defaults.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_postgres::PostgresReplicationSource;
///
/// let source = PostgresReplicationSource::builder("pg-source")
///     .with_host("db.example.com")
///     .with_database("production")
///     .with_user("replication_user")
///     .with_password("secret")
///     .with_tables(vec!["users".to_string(), "orders".to_string()])
///     .with_slot_name("my_slot")
///     .build()?;
/// ```
pub struct PostgresSourceBuilder {
    id: String,
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    tables: Vec<String>,
    slot_name: String,
    publication_name: String,
    ssl_mode: SslMode,
    table_keys: Vec<TableKeyConfig>,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl PostgresSourceBuilder {
    /// Create a new PostgreSQL source builder with the given ID and default values
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            host: "localhost".to_string(),
            port: 5432,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            tables: Vec::new(),
            slot_name: "drasi_slot".to_string(),
            publication_name: "drasi_publication".to_string(),
            ssl_mode: SslMode::default(),
            table_keys: Vec::new(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the PostgreSQL host
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the PostgreSQL port
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the database name
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    /// Set the database user
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Set the database password
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    /// Set the tables to replicate
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = tables;
        self
    }

    /// Add a table to replicate
    pub fn add_table(mut self, table: impl Into<String>) -> Self {
        self.tables.push(table.into());
        self
    }

    /// Set the replication slot name
    pub fn with_slot_name(mut self, slot_name: impl Into<String>) -> Self {
        self.slot_name = slot_name.into();
        self
    }

    /// Set the publication name
    pub fn with_publication_name(mut self, publication_name: impl Into<String>) -> Self {
        self.publication_name = publication_name.into();
        self
    }

    /// Set the SSL mode
    pub fn with_ssl_mode(mut self, ssl_mode: SslMode) -> Self {
        self.ssl_mode = ssl_mode;
        self
    }

    /// Set the table key configurations
    pub fn with_table_keys(mut self, table_keys: Vec<TableKeyConfig>) -> Self {
        self.table_keys = table_keys;
        self
    }

    /// Add a table key configuration
    pub fn add_table_key(mut self, table_key: TableKeyConfig) -> Self {
        self.table_keys.push(table_key);
        self
    }

    /// Set the dispatch mode for this source
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity for this source
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for this source
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set whether this source should auto-start when DrasiLib starts.
    ///
    /// Default is `true`. Set to `false` if this source should only be
    /// started manually via `start_source()`.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: PostgresSourceConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.database = config.database;
        self.user = config.user;
        self.password = config.password;
        self.tables = config.tables;
        self.slot_name = config.slot_name;
        self.publication_name = config.publication_name;
        self.ssl_mode = config.ssl_mode;
        self.table_keys = config.table_keys;
        self
    }

    /// Build the PostgreSQL replication source
    ///
    /// # Errors
    ///
    /// Returns an error if the source cannot be constructed.
    pub fn build(self) -> Result<PostgresReplicationSource> {
        let config = PostgresSourceConfig {
            host: self.host,
            port: self.port,
            database: self.database,
            user: self.user,
            password: self.password,
            tables: self.tables,
            slot_name: self.slot_name,
            publication_name: self.publication_name,
            ssl_mode: self.ssl_mode,
            table_keys: self.table_keys,
        };

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        Ok(PostgresReplicationSource {
            base: SourceBase::new(params)?,
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod construction {
        use super::*;

        #[test]
        fn test_builder_with_valid_config() {
            let source = PostgresSourceBuilder::new("test-source")
                .with_database("testdb")
                .with_user("testuser")
                .build();
            assert!(source.is_ok());
        }

        #[test]
        fn test_builder_with_custom_config() {
            let source = PostgresSourceBuilder::new("pg-source")
                .with_host("192.168.1.100")
                .with_port(5433)
                .with_database("production")
                .with_user("admin")
                .with_password("secret")
                .build()
                .unwrap();
            assert_eq!(source.id(), "pg-source");
        }

        #[test]
        fn test_with_dispatch_creates_source() {
            let config = PostgresSourceConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "testdb".to_string(),
                user: "testuser".to_string(),
                password: String::new(),
                tables: Vec::new(),
                slot_name: "drasi_slot".to_string(),
                publication_name: "drasi_publication".to_string(),
                ssl_mode: SslMode::default(),
                table_keys: Vec::new(),
            };
            let source = PostgresReplicationSource::with_dispatch(
                "dispatch-source",
                config,
                Some(DispatchMode::Channel),
                Some(2000),
            );
            assert!(source.is_ok());
            assert_eq!(source.unwrap().id(), "dispatch-source");
        }
    }

    mod properties {
        use super::*;

        #[test]
        fn test_id_returns_correct_value() {
            let source = PostgresSourceBuilder::new("my-pg-source")
                .with_database("db")
                .with_user("user")
                .build()
                .unwrap();
            assert_eq!(source.id(), "my-pg-source");
        }

        #[test]
        fn test_type_name_returns_postgres() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .build()
                .unwrap();
            assert_eq!(source.type_name(), "postgres");
        }

        #[test]
        fn test_properties_contains_connection_info() {
            let source = PostgresSourceBuilder::new("test")
                .with_host("db.example.com")
                .with_port(5433)
                .with_database("mydb")
                .with_user("app_user")
                .with_password("secret")
                .with_tables(vec!["users".to_string()])
                .build()
                .unwrap();
            let props = source.properties();

            assert_eq!(
                props.get("host"),
                Some(&serde_json::Value::String("db.example.com".to_string()))
            );
            assert_eq!(
                props.get("port"),
                Some(&serde_json::Value::Number(5433.into()))
            );
            assert_eq!(
                props.get("database"),
                Some(&serde_json::Value::String("mydb".to_string()))
            );
            assert_eq!(
                props.get("user"),
                Some(&serde_json::Value::String("app_user".to_string()))
            );
        }

        #[test]
        fn test_properties_includes_password() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .with_password("super_secret_password")
                .build()
                .unwrap();
            let props = source.properties();

            // Password must be preserved for config persistence roundtrip
            assert_eq!(
                props.get("password"),
                Some(&serde_json::Value::String(
                    "super_secret_password".to_string()
                ))
            );
        }

        #[test]
        fn test_properties_includes_tables() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .with_tables(vec!["users".to_string(), "orders".to_string()])
                .build()
                .unwrap();
            let props = source.properties();

            let tables = props.get("tables").unwrap().as_array().unwrap();
            assert_eq!(tables.len(), 2);
            assert_eq!(tables[0], "users");
            assert_eq!(tables[1], "orders");
        }
    }

    mod lifecycle {
        use super::*;

        /// A test secret resolver that returns a fixed value for any secret name.
        struct TestSecretResolver;

        impl drasi_plugin_sdk::resolver::ValueResolver for TestSecretResolver {
            fn resolve_to_string(
                &self,
                value: &drasi_plugin_sdk::ConfigValue<String>,
            ) -> Result<String, drasi_plugin_sdk::resolver::ResolverError> {
                match value {
                    drasi_plugin_sdk::ConfigValue::Secret { name } => {
                        Ok(format!("resolved-secret-{name}"))
                    }
                    _ => Err(drasi_plugin_sdk::resolver::ResolverError::WrongResolverType),
                }
            }
        }

        fn ensure_test_secret_resolver() {
            let _ = drasi_plugin_sdk::resolver::register_secret_resolver(std::sync::Arc::new(
                TestSecretResolver,
            ));
        }

        #[tokio::test]
        async fn test_descriptor_preserves_secret_envelope() {
            use crate::descriptor::PostgresSourceDescriptor;
            use drasi_lib::sources::Source;
            use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;

            ensure_test_secret_resolver();

            let config_json = serde_json::json!({
                "host": "db.example.com",
                "port": 5432,
                "database": "mydb",
                "user": "app_user",
                "password": {
                    "kind": "Secret",
                    "name": "db-password"
                },
                "tables": ["users"],
                "slotName": "drasi_slot",
                "publicationName": "drasi_pub"
            });

            let descriptor = PostgresSourceDescriptor;
            let source = descriptor
                .create_source("pg-secret-test", &config_json, true)
                .await
                .expect("descriptor should create source");

            let props = source.properties();

            // Password must be the Secret envelope, NOT the resolved value
            let password = props.get("password").expect("password must be present");
            assert!(
                password.is_object(),
                "password should be Secret envelope, got: {password}"
            );
            assert_eq!(
                password.get("kind").and_then(|v| v.as_str()),
                Some("Secret"),
                "envelope kind must be Secret"
            );
            assert_eq!(
                password.get("name").and_then(|v| v.as_str()),
                Some("db-password"),
                "secret name must be preserved"
            );

            // Resolved value must NOT leak into persisted properties
            let props_str = serde_json::to_string(&props).unwrap();
            assert!(
                !props_str.contains("resolved-secret-db-password"),
                "resolved secret must not appear in properties"
            );

            // Keys must be camelCase (from raw_config)
            assert!(
                props.contains_key("slotName"),
                "expected camelCase 'slotName', got keys: {:?}",
                props.keys().collect::<Vec<_>>()
            );
            assert!(
                props.contains_key("publicationName"),
                "expected camelCase 'publicationName'"
            );
        }

        #[tokio::test]
        async fn test_initial_status_is_stopped() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .build()
                .unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }

        #[test]
        fn test_builder_fallback_produces_camel_case() {
            use drasi_lib::sources::Source;

            let source = PostgresSourceBuilder::new("pg-fallback")
                .with_host("myhost.example.com")
                .with_port(5433)
                .with_database("mydb")
                .with_user("admin")
                .with_password("secret123")
                .with_ssl_mode(SslMode::Require)
                .with_slot_name("custom_slot")
                .with_publication_name("custom_pub")
                .build()
                .unwrap();

            let props = source.properties();

            // Must use camelCase keys (DTO serialization)
            assert!(
                props.contains_key("slotName"),
                "expected camelCase 'slotName', got keys: {:?}",
                props.keys().collect::<Vec<_>>()
            );
            assert!(
                props.contains_key("publicationName"),
                "expected camelCase 'publicationName'"
            );
            assert!(
                props.contains_key("sslMode"),
                "expected camelCase 'sslMode'"
            );

            // Must NOT have snake_case keys
            assert!(
                !props.contains_key("slot_name"),
                "should not have snake_case 'slot_name'"
            );
            assert!(
                !props.contains_key("publication_name"),
                "should not have snake_case 'publication_name'"
            );

            // Values should be correct
            assert_eq!(
                props.get("host").and_then(|v| v.as_str()),
                Some("myhost.example.com")
            );
            assert_eq!(props.get("port").and_then(|v| v.as_u64()), Some(5433));
            assert_eq!(props.get("database").and_then(|v| v.as_str()), Some("mydb"));
            assert_eq!(
                props.get("password").and_then(|v| v.as_str()),
                Some("secret123")
            );
        }
    }

    mod builder {
        use super::*;

        #[test]
        fn test_postgres_builder_defaults() {
            let source = PostgresSourceBuilder::new("test").build().unwrap();
            assert_eq!(source.config.host, "localhost");
            assert_eq!(source.config.port, 5432);
            assert_eq!(source.config.slot_name, "drasi_slot");
            assert_eq!(source.config.publication_name, "drasi_publication");
        }

        #[test]
        fn test_postgres_builder_custom_values() {
            let source = PostgresSourceBuilder::new("test")
                .with_host("db.example.com")
                .with_port(5433)
                .with_database("production")
                .with_user("app_user")
                .with_password("secret")
                .with_tables(vec!["users".to_string(), "orders".to_string()])
                .build()
                .unwrap();

            assert_eq!(source.config.host, "db.example.com");
            assert_eq!(source.config.port, 5433);
            assert_eq!(source.config.database, "production");
            assert_eq!(source.config.user, "app_user");
            assert_eq!(source.config.password, "secret");
            assert_eq!(source.config.tables.len(), 2);
            assert_eq!(source.config.tables[0], "users");
            assert_eq!(source.config.tables[1], "orders");
        }

        #[test]
        fn test_builder_add_table() {
            let source = PostgresSourceBuilder::new("test")
                .add_table("table1")
                .add_table("table2")
                .add_table("table3")
                .build()
                .unwrap();

            assert_eq!(source.config.tables.len(), 3);
            assert_eq!(source.config.tables[0], "table1");
            assert_eq!(source.config.tables[1], "table2");
            assert_eq!(source.config.tables[2], "table3");
        }

        #[test]
        fn test_builder_slot_and_publication() {
            let source = PostgresSourceBuilder::new("test")
                .with_slot_name("custom_slot")
                .with_publication_name("custom_pub")
                .build()
                .unwrap();

            assert_eq!(source.config.slot_name, "custom_slot");
            assert_eq!(source.config.publication_name, "custom_pub");
        }

        #[test]
        fn test_builder_id() {
            let source = PostgresReplicationSource::builder("my-pg-source")
                .with_database("db")
                .with_user("user")
                .build()
                .unwrap();

            assert_eq!(source.base.id, "my-pg-source");
        }
    }

    mod config {
        use super::*;

        #[test]
        fn test_config_serialization() {
            let config = PostgresSourceConfig {
                host: "localhost".to_string(),
                port: 5432,
                database: "testdb".to_string(),
                user: "testuser".to_string(),
                password: String::new(),
                tables: Vec::new(),
                slot_name: "drasi_slot".to_string(),
                publication_name: "drasi_publication".to_string(),
                ssl_mode: SslMode::default(),
                table_keys: Vec::new(),
            };

            let json = serde_json::to_string(&config).unwrap();
            let deserialized: PostgresSourceConfig = serde_json::from_str(&json).unwrap();

            assert_eq!(config, deserialized);
        }

        #[test]
        fn test_config_deserialization_with_required_fields() {
            let json = r#"{
                "database": "mydb",
                "user": "myuser"
            }"#;
            let config: PostgresSourceConfig = serde_json::from_str(json).unwrap();

            assert_eq!(config.database, "mydb");
            assert_eq!(config.user, "myuser");
            assert_eq!(config.host, "localhost"); // default
            assert_eq!(config.port, 5432); // default
            assert_eq!(config.slot_name, "drasi_slot"); // default
        }

        #[test]
        fn test_config_deserialization_full() {
            let json = r#"{
                "host": "db.prod.internal",
                "port": 5433,
                "database": "production",
                "user": "replication_user",
                "password": "secret",
                "tables": ["accounts", "transactions"],
                "slot_name": "prod_slot",
                "publication_name": "prod_publication"
            }"#;
            let config: PostgresSourceConfig = serde_json::from_str(json).unwrap();

            assert_eq!(config.host, "db.prod.internal");
            assert_eq!(config.port, 5433);
            assert_eq!(config.database, "production");
            assert_eq!(config.user, "replication_user");
            assert_eq!(config.password, "secret");
            assert_eq!(config.tables, vec!["accounts", "transactions"]);
            assert_eq!(config.slot_name, "prod_slot");
            assert_eq!(config.publication_name, "prod_publication");
        }
    }
}

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "postgres-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::PostgresSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
