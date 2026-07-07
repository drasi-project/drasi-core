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

#![allow(unexpected_cfgs)]

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
//! ```rust,no_run
//! use drasi_source_postgres::{PostgresReplicationSource, PostgresSourceBuilder};
//! use std::sync::Arc;
//!
//! # fn main() -> anyhow::Result<()> {
//! let source = PostgresReplicationSource::builder("pg-source")
//!     .with_host("db.example.com")
//!     .with_database("production")
//!     .with_user("replication_user")
//!     .with_password("secret")
//!     .with_tables(vec!["users".to_string(), "orders".to_string()])
//!     .build()?;
//! # Ok(())
//! # }
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

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_lib::schema::{
    normalize_table_label, NodeSchema, PropertySchema, PropertyType, SourceSchema,
};
use log::{debug, error, info};
use postgres_native_tls::MakeTlsConnector;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

use drasi_lib::channels::{ComponentStatus, DispatchMode, *};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::{Source, SourceError};
use tracing::Instrument;

/// Shared state tracking WAL progress for subscribe/rewind decisions.
///
/// The replication stream atomically publishes `read_lsn` (the highest WAL LSN
/// received so far). The `subscribe()` method reads this to decide whether a
/// new subscriber can be served from the current stream position or if a rewind
/// is needed.
pub(crate) struct ReplayState {
    pub(crate) read_lsn: AtomicU64,
    /// Maximum `flush_lsn` that `send_feedback()` may report to PostgreSQL.
    ///
    /// Set to the replay start LSN during `pause_replication_for_restart()` to
    /// prevent the slot's `restart_lsn` from advancing past the position that
    /// other (not-yet-subscribed) queries will need.  `u64::MAX` means "no
    /// fence" (normal operation).
    pub(crate) flush_fence_lsn: AtomicU64,
    /// UNIX epoch seconds when the fence was last set. Used for auto-clearing
    /// the fence after [`FENCE_TIMEOUT_SECS`] as a safety fallback.
    pub(crate) fence_set_epoch_secs: AtomicU64,
}

/// Duration (in seconds) after which the flush fence auto-clears.
///
/// This is a safety fallback for deployments where no explicit
/// `on_subscriptions_complete()` signal arrives (e.g., FFI plugin usage).
/// During normal startup all queries subscribe within a few seconds; the
/// timeout must be generous enough to cover slow-starting deployments.
const FENCE_TIMEOUT_SECS: u64 = 60;

impl Default for ReplayState {
    fn default() -> Self {
        Self {
            read_lsn: AtomicU64::new(0),
            flush_fence_lsn: AtomicU64::new(u64::MAX),
            fence_set_epoch_secs: AtomicU64::new(0),
        }
    }
}

impl ReplayState {
    fn current_read_lsn(&self) -> u64 {
        self.read_lsn.load(Ordering::Acquire)
    }

    /// Set the flush fence to prevent `send_feedback()` from advancing
    /// `flush_lsn` past `lsn`.
    fn set_flush_fence(&self, lsn: u64) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.flush_fence_lsn.store(lsn, Ordering::Release);
        self.fence_set_epoch_secs.store(now_secs, Ordering::Release);
    }

    /// Clear the flush fence, allowing normal `flush_lsn` advancement.
    fn clear_flush_fence(&self) {
        self.flush_fence_lsn.store(u64::MAX, Ordering::Release);
    }

    /// Returns the effective fence value, auto-clearing if the timeout has
    /// elapsed. Returns `u64::MAX` if no fence is active.
    fn effective_flush_fence(&self) -> u64 {
        let fence = self.flush_fence_lsn.load(Ordering::Acquire);
        if fence == u64::MAX {
            return u64::MAX;
        }
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let set_secs = self.fence_set_epoch_secs.load(Ordering::Acquire);
        if now_secs.saturating_sub(set_secs) > FENCE_TIMEOUT_SECS {
            // Timeout expired — auto-clear
            self.flush_fence_lsn.store(u64::MAX, Ordering::Release);
            u64::MAX
        } else {
            fence
        }
    }
}

/// PostgreSQL replication source that captures changes via logical replication.
///
/// This source connects to PostgreSQL using the replication protocol and decodes
/// WAL messages in real-time, converting them to Drasi source change events.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: PostgreSQL connection and replication configuration
/// - `replay_state`: Shared WAL progress for subscribe/rewind decisions
pub struct PostgresReplicationSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// PostgreSQL source configuration
    config: PostgresSourceConfig,
    /// Best-effort cached schema populated from information_schema on start.
    cached_schema: Arc<std::sync::RwLock<Option<SourceSchema>>>,
    /// Shared replay progress for subscribe()/rewind decisions and WAL feedback.
    replay_state: Arc<ReplayState>,
    /// Serializes subscribe() calls that may restart the replication task.
    subscribe_lock: Mutex<()>,
}

fn postgres_type_to_property_type(data_type: &str) -> Option<PropertyType> {
    match data_type {
        "smallint" | "integer" | "bigint" => Some(PropertyType::Integer),
        "real" | "double precision" | "numeric" | "decimal" => Some(PropertyType::Float),
        "boolean" => Some(PropertyType::Boolean),
        "timestamp without time zone"
        | "timestamp with time zone"
        | "date"
        | "time without time zone"
        | "time with time zone" => Some(PropertyType::Timestamp),
        "json" | "jsonb" => Some(PropertyType::Json),
        "character" | "character varying" | "text" | "uuid" | "bytea" => Some(PropertyType::String),
        _ => None,
    }
}

async fn introspect_postgres_schema(config: &PostgresSourceConfig) -> Result<Option<SourceSchema>> {
    if config.tables.is_empty() {
        return Ok(None);
    }

    let mut pg_config = tokio_postgres::Config::new();
    pg_config.host(&config.host);
    pg_config.port(config.port);
    pg_config.dbname(&config.database);
    pg_config.user(&config.user);
    if !config.password.is_empty() {
        pg_config.password(&config.password);
    }

    let client = match config.ssl_mode {
        SslMode::Require => {
            pg_config.ssl_mode(tokio_postgres::config::SslMode::Require);
            let tls_connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_hostnames(false)
                .danger_accept_invalid_certs(false)
                .build()
                .map_err(|e| anyhow!("Failed to create TLS connector: {e}"))?;
            let connector = MakeTlsConnector::new(tls_connector);

            debug!("Schema introspection: connecting with SSL (require)");
            let (client, connection) = pg_config.connect(connector).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::warn!("PostgreSQL schema introspection connection closed: {e}");
                }
            });
            client
        }
        SslMode::Prefer => {
            // Try TLS first, fall back to plaintext
            let tls_connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_hostnames(false)
                .danger_accept_invalid_certs(false)
                .build()
                .map_err(|e| anyhow!("Failed to create TLS connector: {e}"))?;
            let connector = MakeTlsConnector::new(tls_connector);

            pg_config.ssl_mode(tokio_postgres::config::SslMode::Prefer);
            debug!("Schema introspection: connecting with SSL (prefer)");
            let (client, connection) = pg_config.connect(connector).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::warn!("PostgreSQL schema introspection connection closed: {e}");
                }
            });
            client
        }
        SslMode::Disable => {
            debug!("Schema introspection: connecting without SSL");
            let (client, connection) = pg_config.connect(tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    log::warn!("PostgreSQL schema introspection connection closed: {e}");
                }
            });
            client
        }
    };

    let mut nodes = Vec::new();

    for table in &config.tables {
        let (schema_name, table_name) = table
            .split_once('.')
            .map(|(schema, name)| (schema.to_string(), name.to_string()))
            .unwrap_or_else(|| ("public".to_string(), table.to_string()));

        let rows = client
            .query(
                "SELECT column_name, data_type \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 \
                 ORDER BY ordinal_position",
                &[&schema_name, &table_name],
            )
            .await?;

        let properties = rows
            .into_iter()
            .map(|row| PropertySchema {
                name: row.get::<_, String>(0),
                data_type: postgres_type_to_property_type(&row.get::<_, String>(1)),
                description: None,
            })
            .collect();

        nodes.push(NodeSchema {
            label: normalize_table_label(&table_name),
            properties,
        });
    }

    Ok(Some(SourceSchema {
        nodes,
        relations: Vec::new(),
    }))
}

impl PostgresReplicationSource {
    /// Create a builder for PostgresReplicationSource
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use drasi_source_postgres::PostgresReplicationSource;
    ///
    /// # fn main() -> anyhow::Result<()> {
    /// let source = PostgresReplicationSource::builder("pg-source")
    ///     .with_host("db.example.com")
    ///     .with_database("production")
    ///     .with_user("replication_user")
    ///     .with_password("secret")
    ///     .with_tables(vec!["users".to_string(), "orders".to_string()])
    ///     .build()?;
    /// # Ok(())
    /// # }
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
    /// ```rust,no_run
    /// use drasi_source_postgres::{PostgresReplicationSource, PostgresSourceConfig, SslMode};
    ///
    /// # fn main() -> anyhow::Result<()> {
    /// let config = PostgresSourceConfig {
    ///     host: "db.example.com".to_string(),
    ///     port: 5432,
    ///     database: "mydb".to_string(),
    ///     user: "replication_user".to_string(),
    ///     password: "secret".to_string(),
    ///     tables: vec!["users".to_string()],
    ///     slot_name: "drasi_slot".to_string(),
    ///     publication_name: "drasi_pub".to_string(),
    ///     ssl_mode: SslMode::Disable,
    ///     table_keys: vec![],
    /// };
    ///
    /// let source = PostgresReplicationSource::new("my-pg-source", config)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(id: impl Into<String>, config: PostgresSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            cached_schema: Arc::new(std::sync::RwLock::new(None)),
            replay_state: Arc::new(ReplayState::default()),
            subscribe_lock: Mutex::new(()),
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
            cached_schema: Arc::new(std::sync::RwLock::new(None)),
            replay_state: Arc::new(ReplayState::default()),
            subscribe_lock: Mutex::new(()),
        })
    }
}

impl PostgresReplicationSource {
    /// Spawn the background replication task, optionally starting from a specific LSN.
    ///
    /// Waits for the task to confirm successful connection before returning, except for the
    /// initial bootstrap handoff path where the task must wait for the snapshot boundary first.
    async fn spawn_replication_task(
        &self,
        start_lsn: Option<u64>,
        wait_for_bootstrap_boundary: bool,
    ) -> Result<()> {
        let config = self.config.clone();
        let source_id = self.base.id.clone();
        let reporter = self.base.status_handle();
        let base = self.base.clone_shared();
        let replay_state = self.replay_state.clone();
        // `startup_tx` signals that `start()` may return. In the non-bootstrap path
        // it is sent once replication is connected (see `stream.run`). In the bootstrap
        // path it is sent as soon as the task is spawned, because the task must then
        // block on `wait_for_subscribers()` and the bootstrap boundary — work that
        // happens only after `start()` returns and a subscriber registers. It therefore
        // does NOT imply a live CDC connection in that path.
        let (startup_tx, startup_rx) = oneshot::channel::<std::result::Result<(), String>>();

        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let source_id_for_span = source_id.clone();
        let span = tracing::info_span!(
            "postgres_replication_task",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source",
            start_lsn = ?start_lsn
        );

        let task = tokio::spawn(
            async move {
                info!("Starting replication for source {source_id}");
                let mut startup_tx = Some(startup_tx);

                let effective_start_lsn = if wait_for_bootstrap_boundary {
                    // Release start() now: the task must wait for a subscriber and the
                    // bootstrap boundary, neither of which can happen until start() returns.
                    if let Some(tx) = startup_tx.take() {
                        let _ = tx.send(Ok(()));
                    }

                    // Wait for the first subscriber so we know whether an
                    // initial bootstrap was actually requested before choosing
                    // the CDC start position. A streaming-only subscription
                    // (enable_bootstrap=false) never publishes a boundary, so
                    // without this gate the task would wait forever.
                    base.wait_for_subscribers().await;

                    if base.take_pending_initial_bootstrap() {
                        info!(
                            "PostgreSQL source '{source_id}' waiting for bootstrap snapshot boundary"
                        );
                        match base.wait_for_bootstrap_boundary().await {
                            Some(boundary) => match connection::position_bytes_to_lsn(&boundary)
                                .context("invalid PostgreSQL bootstrap boundary position")
                            {
                                Ok(lsn) => {
                                    info!(
                                        "PostgreSQL source '{source_id}' starting CDC from bootstrap boundary LSN {lsn:x}"
                                    );
                                    Some(lsn)
                                }
                                Err(e) => {
                                    error!("Replication task failed for {source_id}: {e}");
                                    reporter
                                        .set_status(
                                            ComponentStatus::Error,
                                            Some(format!("Replication failed: {e}")),
                                        )
                                        .await;
                                    return;
                                }
                            },
                            None => {
                                info!(
                                    "PostgreSQL source '{source_id}' stopped before bootstrap boundary was published"
                                );
                                return;
                            }
                        }
                    } else {
                        info!(
                            "PostgreSQL source '{source_id}' starting CDC from current position (no initial bootstrap requested)"
                        );
                        start_lsn
                    }
                } else {
                    start_lsn
                };

                let mut stream = stream::ReplicationStream::new(
                    config,
                    source_id.clone(),
                    reporter.clone(),
                    base,
                    replay_state,
                    effective_start_lsn,
                );

                if let Err(e) = stream.run(startup_tx).await {
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

        match startup_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => {
                let _ = self.base.task_handle.write().await.take();
                Err(anyhow!(
                    "Failed to establish PostgreSQL replication: {message}"
                ))
            }
            Err(_) => {
                let _ = self.base.task_handle.write().await.take();
                Err(anyhow!(
                    "PostgreSQL replication task exited before confirming startup"
                ))
            }
        }
    }

    async fn abort_replication_task(&self) {
        if let Some(task) = self.base.task_handle.write().await.take() {
            task.abort();
            let _ = task.await;
        }
    }

    async fn pause_replication_for_restart(&self, start_lsn: u64) {
        info!(
            "Pausing PostgreSQL source '{}' before replay from requested LSN {:x}",
            self.base.id, start_lsn
        );

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some(format!(
                    "Rewinding PostgreSQL replication to LSN {start_lsn:x}"
                )),
            )
            .await;

        self.abort_replication_task().await;

        // Clear stale sequence→position mappings from the pre-replay stream.
        // Without this, compute_confirmed_source_position() could map a
        // position handle (seeded from the recovery checkpoint) to a WAL position
        // from the old stream, causing flush_lsn feedback to advance the
        // slot's restart_lsn past other queries' checkpoints.
        self.base.clear_sequence_position_map().await;

        // Set the flush fence to prevent send_feedback() from advancing
        // flush_lsn past start_lsn. This protects subscribers that haven't
        // connected yet — their checkpoint is at (or near) start_lsn, so the
        // slot must retain WAL from that point.
        self.replay_state.set_flush_fence(start_lsn);
    }

    async fn resume_replication_from(&self, start_lsn: u64) -> Result<()> {
        self.spawn_replication_task(Some(start_lsn), false).await?;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some(format!(
                    "PostgreSQL replication resumed from LSN {start_lsn:x}"
                )),
            )
            .await;

        Ok(())
    }

    async fn restart_replication_from(&self, start_lsn: u64) -> Result<()> {
        info!(
            "Restarting PostgreSQL source '{}' from requested LSN {:x}",
            self.base.id, start_lsn
        );

        self.pause_replication_for_restart(start_lsn).await;
        self.resume_replication_from(start_lsn).await
    }

    /// Query the replication slot's consistent_point to determine the earliest
    /// WAL position that can be replayed.
    async fn get_earliest_available_lsn(&self) -> Result<u64> {
        let mut conn = connection::ReplicationConnection::connect(
            &self.config.host,
            self.config.port,
            &self.config.database,
            &self.config.user,
            &self.config.password,
        )
        .await?;

        let _ = conn.identify_system().await?;
        let slot_info = conn
            .get_replication_slot_info(&self.config.slot_name)
            .await?;
        let _ = conn.close().await;

        // Use restart_lsn (the earliest WAL the slot retains) when available,
        // falling back to consistent_point for newly created slots.
        let lsn_str = slot_info
            .restart_lsn
            .as_deref()
            .unwrap_or(&slot_info.consistent_point);

        if lsn_str.is_empty() || lsn_str == "0/0" {
            Ok(0)
        } else {
            connection::parse_lsn(lsn_str)
        }
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

    fn describe_schema(&self) -> Option<SourceSchema> {
        self.cached_schema
            .read()
            .ok()
            .and_then(|schema| schema.clone())
            .or_else(|| {
                if self.config.tables.is_empty() {
                    None
                } else {
                    Some(SourceSchema {
                        nodes: self
                            .config
                            .tables
                            .iter()
                            .map(|table| NodeSchema::new(normalize_table_label(table)))
                            .collect(),
                        relations: Vec::new(),
                    })
                }
            })
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting, None).await;
        self.base.reset_bootstrap_boundary();
        info!("Starting PostgreSQL replication source: {}", self.base.id);

        match introspect_postgres_schema(&self.config).await {
            Ok(Some(schema)) => {
                if let Ok(mut cached) = self.cached_schema.write() {
                    *cached = Some(schema);
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!(
                    "Failed to introspect PostgreSQL schema for '{}': {e}",
                    self.base.id
                );
            }
        }

        let wait_for_bootstrap_boundary = self.base.has_bootstrap_provider();
        self.spawn_replication_task(None, wait_for_bootstrap_boundary)
            .await?;
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

        self.abort_replication_task().await;

        // Clear cached schema so a subsequent start() re-introspects
        if let Ok(mut cached) = self.cached_schema.write() {
            *cached = None;
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
        // Serialize subscribe calls that may restart the replication task to
        // prevent TOCTOU races between concurrent callers.
        let _guard = self.subscribe_lock.lock().await;

        let mut restart_from = None;
        let mut pause_before_subscribe = false;

        if let Some(ref resume_bytes) = settings.resume_from {
            let resume_lsn = connection::position_bytes_to_lsn(resume_bytes)?;

            let earliest_available = self.get_earliest_available_lsn().await?;
            if resume_lsn < earliest_available {
                return Err(SourceError::PositionUnavailable {
                    source_id: self.base.id.clone(),
                    requested: resume_bytes.clone(),
                    earliest_available: Some(connection::commit_position_bytes(
                        earliest_available,
                        0,
                    )),
                }
                .into());
            }

            let read_lsn = self.replay_state.current_read_lsn();
            let is_running = self.base.get_status().await == ComponentStatus::Running;

            if !is_running || read_lsn == 0 || resume_lsn < read_lsn {
                restart_from = Some(resume_lsn);
                pause_before_subscribe = is_running;
            }
        }

        if let Some(start_lsn) = restart_from.filter(|_| pause_before_subscribe) {
            // Quiesce the current replication task before attaching the resumed
            // receiver so it cannot observe newer live events ahead of replayed
            // older WAL entries.
            self.pause_replication_for_restart(start_lsn).await;
        }

        let response = match self
            .base
            .subscribe_with_bootstrap(&settings, "PostgreSQL")
            .await
        {
            Ok(response) => response,
            Err(err) => {
                if pause_before_subscribe {
                    self.base
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("Failed to register replay subscription: {err}")),
                        )
                        .await;
                }
                return Err(err);
            }
        };

        if let Some(start_lsn) = restart_from {
            if pause_before_subscribe {
                self.resume_replication_from(start_lsn).await?;
            } else {
                self.restart_replication_from(start_lsn).await?;
            }
        }

        Ok(response)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
        // Source positions are uniformly 16-byte big-endian
        // `[ commit_lsn (8) | offset (8) ]` (CDC events and the
        // `[snapshot_lsn | u64::MAX]` bootstrap boundary), so byte-lexicographic
        // comparison equals tuple ordering on (commit_lsn, offset).
        self.base
            .set_position_comparator(drasi_lib::sources::ByteLexPositionComparator)
            .await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.base.remove_position_handle(query_id).await;
    }

    async fn on_subscriptions_complete(&self) {
        // Release the flush fence so that send_feedback() can advance flush_lsn
        // based on the natural min-watermark of all registered position handles.
        self.replay_state.clear_flush_fence();
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for PostgreSQL source configuration.
///
/// Provides a fluent API for constructing PostgreSQL source configurations
/// with sensible defaults.
///
/// # Example
///
/// ```rust,no_run
/// use drasi_source_postgres::PostgresReplicationSource;
///
/// # fn main() -> anyhow::Result<()> {
/// let source = PostgresReplicationSource::builder("pg-source")
///     .with_host("db.example.com")
///     .with_database("production")
///     .with_user("replication_user")
///     .with_password("secret")
///     .with_tables(vec!["users".to_string(), "orders".to_string()])
///     .with_slot_name("my_slot")
///     .build()?;
/// # Ok(())
/// # }
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
            cached_schema: Arc::new(std::sync::RwLock::new(None)),
            replay_state: Arc::new(ReplayState::default()),
            subscribe_lock: Mutex::new(()),
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

        #[test]
        fn test_describe_schema_falls_back_to_configured_tables() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .with_tables(vec!["public.users".to_string(), "orders".to_string()])
                .build()
                .unwrap();

            let schema = source
                .describe_schema()
                .expect("configured postgres tables should produce fallback schema");

            assert_eq!(schema.nodes.len(), 2);
            assert!(schema.nodes.iter().any(|node| node.label == "users"));
            assert!(schema.nodes.iter().any(|node| node.label == "orders"));
        }

        #[test]
        fn test_postgres_type_to_property_type_integer() {
            assert_eq!(
                postgres_type_to_property_type("integer"),
                Some(PropertyType::Integer)
            );
            assert_eq!(
                postgres_type_to_property_type("bigint"),
                Some(PropertyType::Integer)
            );
            assert_eq!(
                postgres_type_to_property_type("smallint"),
                Some(PropertyType::Integer)
            );
        }

        #[test]
        fn test_postgres_type_to_property_type_float() {
            assert_eq!(
                postgres_type_to_property_type("double precision"),
                Some(PropertyType::Float)
            );
            assert_eq!(
                postgres_type_to_property_type("real"),
                Some(PropertyType::Float)
            );
            assert_eq!(
                postgres_type_to_property_type("numeric"),
                Some(PropertyType::Float)
            );
            assert_eq!(
                postgres_type_to_property_type("decimal"),
                Some(PropertyType::Float)
            );
        }

        #[test]
        fn test_postgres_type_to_property_type_boolean() {
            assert_eq!(
                postgres_type_to_property_type("boolean"),
                Some(PropertyType::Boolean)
            );
        }

        #[test]
        fn test_postgres_type_to_property_type_timestamp() {
            assert_eq!(
                postgres_type_to_property_type("timestamp with time zone"),
                Some(PropertyType::Timestamp)
            );
            assert_eq!(
                postgres_type_to_property_type("timestamp without time zone"),
                Some(PropertyType::Timestamp)
            );
            assert_eq!(
                postgres_type_to_property_type("date"),
                Some(PropertyType::Timestamp)
            );
        }

        #[test]
        fn test_postgres_type_to_property_type_json() {
            assert_eq!(
                postgres_type_to_property_type("json"),
                Some(PropertyType::Json)
            );
            assert_eq!(
                postgres_type_to_property_type("jsonb"),
                Some(PropertyType::Json)
            );
        }

        #[test]
        fn test_postgres_type_to_property_type_string() {
            assert_eq!(
                postgres_type_to_property_type("character varying"),
                Some(PropertyType::String)
            );
            assert_eq!(
                postgres_type_to_property_type("text"),
                Some(PropertyType::String)
            );
            assert_eq!(
                postgres_type_to_property_type("uuid"),
                Some(PropertyType::String)
            );
        }

        #[test]
        fn test_postgres_type_to_property_type_unknown_returns_none() {
            assert_eq!(postgres_type_to_property_type("point"), None);
            assert_eq!(postgres_type_to_property_type("polygon"), None);
            assert_eq!(postgres_type_to_property_type("cidr"), None);
        }
    }

    mod lifecycle {
        use super::*;

        /// A test secret resolver that returns a fixed value for any secret name.
        struct TestSecretResolver;

        #[async_trait::async_trait]
        impl drasi_plugin_sdk::resolver::ValueResolver for TestSecretResolver {
            async fn resolve_to_string(
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
            drasi_plugin_sdk::resolver::register_secret_resolver(std::sync::Arc::new(
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

        #[tokio::test]
        async fn test_pause_replication_for_restart_aborts_existing_task() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .build()
                .unwrap();

            source.base.set_status(ComponentStatus::Running, None).await;

            let task = tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            });
            *source.base.task_handle.write().await = Some(task);

            source.pause_replication_for_restart(42).await;

            assert!(source.base.task_handle.read().await.is_none());
            assert_eq!(source.status().await, ComponentStatus::Starting);
        }

        #[test]
        fn test_supports_replay_returns_true() {
            let source = PostgresSourceBuilder::new("test")
                .with_database("db")
                .with_user("user")
                .build()
                .unwrap();
            assert!(source.supports_replay());
        }
    }

    mod subscribe {
        use super::*;
        use drasi_lib::config::SourceSubscriptionSettings;
        use std::collections::HashSet;

        #[tokio::test]
        async fn test_malformed_resume_from_rejected() {
            let source = PostgresSourceBuilder::new("test-source")
                .with_database("testdb")
                .with_user("testuser")
                .build()
                .unwrap();

            // 8 bytes — a bare LSN is no longer a valid position (only the
            // 16-byte `[commit_lsn | offset]` encoding is accepted).
            let bad_position = bytes::Bytes::from(vec![0u8; 8]);
            let settings = SourceSubscriptionSettings {
                source_id: "test-source".to_string(),
                query_id: "q-bad-position".to_string(),
                enable_bootstrap: false,
                nodes: HashSet::new(),
                relations: HashSet::new(),
                resume_from: Some(bad_position),
                request_position_handle: false,
            };

            let result = source.subscribe(settings).await;
            assert!(result.is_err());
            let err_msg = format!("{}", result.err().unwrap());
            assert!(
                err_msg.contains("expected 16 bytes"),
                "Error should mention expected byte length, got: {err_msg}"
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
