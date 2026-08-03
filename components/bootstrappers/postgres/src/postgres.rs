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

//! PostgreSQL bootstrap provider for reading initial data from PostgreSQL databases

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_postgres_common::{
    oid_from_information_schema, parse_bytea_text, PostgresValue, BOOL, BYTEA, CHAR, DATE, FLOAT4,
    FLOAT8, INT2, INT4, INT8, JSON, JSONB, NAME, NUMERIC, TEXT, TIME, TIMESTAMP, TIMESTAMPTZ, UUID,
    VARCHAR,
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_postgres::{Client, NoTls, Row, Transaction};

use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::SourceChangeEvent;

pub use crate::config::{PostgresBootstrapConfig, SslMode, TableKeyConfig};

fn parse_lsn(lsn_str: &str) -> Result<u64> {
    let parts: Vec<&str> = lsn_str.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow!("Invalid LSN format: {lsn_str}"));
    }

    let high = u64::from_str_radix(parts[0], 16)
        .with_context(|| format!("Invalid high bits in LSN: {lsn_str}"))?;
    let low = u64::from_str_radix(parts[1], 16)
        .with_context(|| format!("Invalid low bits in LSN: {lsn_str}"))?;

    Ok((high << 32) | low)
}

/// Encodes the bootstrap snapshot boundary as a 16-byte source-position
/// `[ snapshot_lsn (8 BE) | u64::MAX (8 BE) ]`, matching the CDC source's
/// `[ commit_lsn | in-transaction offset ]` encoding.
///
/// Padding the offset with `u64::MAX` preserves the exact handover boundary:
/// a CDC transaction whose `commit_lsn == snapshot_lsn` (already contained in
/// the snapshot) stays suppressed, while every transaction committing after the
/// snapshot is delivered. See issue #599.
pub fn snapshot_position_bytes(snapshot_lsn: u64) -> Bytes {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&snapshot_lsn.to_be_bytes());
    buf.extend_from_slice(&u64::MAX.to_be_bytes());
    Bytes::from(buf)
}

/// Bootstrap provider for PostgreSQL sources
///
/// This provider takes its configuration directly at construction time,
/// following the instance-based plugin architecture.
pub struct PostgresBootstrapProvider {
    config: PostgresConfig,
}

impl PostgresBootstrapProvider {
    /// Create a new PostgreSQL bootstrap provider with the given configuration
    pub fn new(postgres_config: PostgresBootstrapConfig) -> Self {
        Self {
            config: PostgresConfig::from_bootstrap_config(postgres_config),
        }
    }

    /// Create a builder for PostgresBootstrapProvider
    pub fn builder() -> PostgresBootstrapProviderBuilder {
        PostgresBootstrapProviderBuilder::new()
    }
}

/// Builder for PostgresBootstrapProvider
///
/// # Example
///
/// ```no_run
/// use drasi_bootstrap_postgres::PostgresBootstrapProvider;
///
/// let provider = PostgresBootstrapProvider::builder()
///     .with_host("localhost")
///     .with_port(5432)
///     .with_database("mydb")
///     .with_user("postgres")
///     .with_password("secret")
///     .with_tables(vec!["users".to_string()])
///     .build();
/// ```
pub struct PostgresBootstrapProviderBuilder {
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
}

impl PostgresBootstrapProviderBuilder {
    /// Create a new builder with default values
    pub fn new() -> Self {
        Self {
            host: "localhost".to_string(), // DevSkim: ignore DS137138
            port: 5432,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            tables: Vec::new(),
            slot_name: "drasi_slot".to_string(),
            publication_name: "drasi_pub".to_string(),
            ssl_mode: SslMode::Disable,
            table_keys: Vec::new(),
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

    /// Set the username
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Set the password
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    /// Set the tables to bootstrap
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = tables;
        self
    }

    /// Add a table to bootstrap
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
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
    pub fn with_table_key(mut self, table: impl Into<String>, key_columns: Vec<String>) -> Self {
        self.table_keys.push(TableKeyConfig {
            table: table.into(),
            key_columns,
        });
        self
    }

    /// Build the PostgresBootstrapProvider
    pub fn build(self) -> PostgresBootstrapProvider {
        let config = PostgresBootstrapConfig {
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
        PostgresBootstrapProvider::new(config)
    }
}

impl Default for PostgresBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BootstrapProvider for PostgresBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        info!(
            "Starting PostgreSQL bootstrap for query '{}' with {} node labels and {} relation labels",
            request.query_id,
            request.node_labels.len(),
            request.relation_labels.len()
        );

        // Create bootstrap handler with pre-configured settings
        let mut handler =
            PostgresBootstrapHandler::new(self.config.clone(), context.source_id.clone());

        // Store query_id before moving request
        let query_id = request.query_id.clone();

        // Execute bootstrap
        let (count, source_position) = handler.execute(request, context, event_tx).await?;

        info!("Completed PostgreSQL bootstrap for query {query_id}: sent {count} records");

        Ok(BootstrapResult {
            event_count: count,
            source_position: Some(source_position),
        })
    }
}

/// PostgreSQL configuration extracted from source properties
#[derive(Debug, Clone)]
struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    #[allow(dead_code)]
    pub tables: Vec<String>,
    #[allow(dead_code)]
    pub slot_name: String,
    #[allow(dead_code)]
    pub publication_name: String,
    #[allow(dead_code)]
    pub ssl_mode: SslMode,
    pub table_keys: Vec<TableKeyConfig>,
}

impl PostgresConfig {
    fn from_bootstrap_config(postgres_config: PostgresBootstrapConfig) -> Self {
        PostgresConfig {
            host: postgres_config.host.clone(),
            port: postgres_config.port,
            database: postgres_config.database.clone(),
            user: postgres_config.user.clone(),
            password: postgres_config.password.clone(),
            tables: postgres_config.tables.clone(),
            slot_name: postgres_config.slot_name.clone(),
            publication_name: postgres_config.publication_name.clone(),
            ssl_mode: postgres_config.ssl_mode,
            table_keys: postgres_config.table_keys.clone(),
        }
    }
}

/// Handles bootstrap operations for PostgreSQL source
struct PostgresBootstrapHandler {
    config: PostgresConfig,
    source_id: String,
    /// Stores primary key information for each table
    table_primary_keys: HashMap<String, Vec<String>>,
}

impl PostgresBootstrapHandler {
    fn new(config: PostgresConfig, source_id: String) -> Self {
        Self {
            config,
            source_id,
            table_primary_keys: HashMap::new(),
        }
    }

    /// Execute bootstrap for the given request
    async fn execute(
        &mut self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
    ) -> Result<(usize, Bytes)> {
        info!(
            "Bootstrap: Connecting to PostgreSQL at {}:{}",
            self.config.host, self.config.port
        );

        // Connect to PostgreSQL
        let mut client = self.connect().await?;

        // Query and cache primary key information
        self.query_primary_keys(&client).await?;

        info!("Bootstrap: Connected, creating snapshot transaction...");
        // Start snapshot transaction and capture LSN
        let (transaction, snapshot_lsn) = self.create_snapshot(&mut client).await?;
        let source_position = snapshot_position_bytes(snapshot_lsn);

        info!("Bootstrap snapshot created at LSN: {snapshot_lsn:x}");

        // Resolve labels to verified table names
        let tables = self.resolve_tables(&request, &transaction).await?;
        info!(
            "Resolved {} labels to {} tables",
            request.node_labels.len() + request.relation_labels.len(),
            tables.len()
        );

        // Fetch and stream data from each table
        let mut total_count = 0;
        for table in &tables {
            let count = self
                .bootstrap_table(&transaction, table, context, &event_tx)
                .await?;
            info!("Bootstrapped {count} rows from table '{table}'");
            total_count += count;
        }

        // Commit transaction to release snapshot
        transaction.commit().await?;

        info!("Bootstrap completed: {total_count} total elements sent");
        Ok((total_count, source_position))
    }

    /// Create a regular PostgreSQL connection
    async fn connect(&self) -> Result<Client> {
        let connection_string = format!(
            "host={} port={} user={} password={} dbname={}",
            self.config.host,
            self.config.port,
            self.config.user,
            self.config.password,
            self.config.database
        );

        let (client, connection) = tokio_postgres::connect(&connection_string, NoTls).await?;

        // Spawn connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!("PostgreSQL connection error: {e}");
            }
        });

        Ok(client)
    }

    /// Create a consistent snapshot and capture current LSN
    async fn create_snapshot<'a>(&self, client: &'a mut Client) -> Result<(Transaction<'a>, u64)> {
        // Start transaction with repeatable read isolation
        let transaction = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .start()
            .await?;

        // Capture current LSN for replication coordination
        let row = transaction
            .query_one("SELECT pg_current_wal_lsn()::text", &[])
            .await?;
        let lsn: String = row.get(0);
        let lsn = parse_lsn(&lsn).context("Failed to parse PostgreSQL snapshot LSN")?;

        Ok((transaction, lsn))
    }

    /// Resolve requested labels to verified table names.
    /// Labels are used as-is (case-sensitive) to match PostgreSQL table names,
    /// ensuring consistency with the CDC stream which also uses the actual table name.
    async fn resolve_tables(
        &self,
        request: &BootstrapRequest,
        transaction: &Transaction<'_>,
    ) -> Result<Vec<String>> {
        let mut tables = Vec::new();

        // Combine all labels (treating nodes and relations the same)
        let all_labels: Vec<String> = request
            .node_labels
            .iter()
            .chain(request.relation_labels.iter())
            .cloned()
            .collect();

        for label in all_labels {
            if self.table_exists(transaction, &label).await? {
                tables.push(label);
            } else {
                warn!("Table '{label}' does not exist, skipping");
            }
        }

        Ok(tables)
    }

    /// Check if a table exists in the database
    async fn table_exists(&self, transaction: &Transaction<'_>, table_name: &str) -> Result<bool> {
        let row = transaction
            .query_one(
                "SELECT EXISTS (
                    SELECT 1 FROM information_schema.tables
                    WHERE table_schema = 'public'
                    AND table_name = $1
                )",
                &[&table_name],
            )
            .await?;

        Ok(row.get(0))
    }

    /// Bootstrap all data from a single table
    async fn bootstrap_table(
        &self,
        transaction: &Transaction<'_>,
        table: &str,
        context: &BootstrapContext,
        event_tx: &drasi_lib::channels::BootstrapEventSender,
    ) -> Result<usize> {
        debug!("Starting bootstrap of table '{table}'");

        // Get table columns for proper type handling
        let columns = self.get_table_columns(transaction, table).await?;

        // Quote table name to preserve case
        let query = format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""));
        let rows = transaction.query(&query, &[]).await?;

        let mut count = 0;
        let mut batch = Vec::new();
        let batch_size = 1000;

        for row in rows {
            let source_change = self.row_to_source_change(&row, table, &columns).await?;

            batch.push(SourceChangeEvent {
                source_id: self.source_id.clone(),
                change: source_change,
                timestamp: chrono::Utc::now(),
                sequence: None,
            });

            if batch.len() >= batch_size {
                self.send_batch(&mut batch, context, event_tx).await?;
                count += batch_size;
            }
        }

        // Send remaining batch
        if !batch.is_empty() {
            count += batch.len();
            self.send_batch(&mut batch, context, event_tx).await?;
        }

        Ok(count)
    }

    /// Get column information for a table
    async fn get_table_columns(
        &self,
        transaction: &Transaction<'_>,
        table_name: &str,
    ) -> Result<Vec<ColumnInfo>> {
        // Prefer pg_attribute.atttypid for accurate OIDs (arrays, domains, etc.).
        // Fall back to information_schema mapping if the catalog query fails.
        let catalog_result = transaction
            .query(
                "SELECT a.attname AS column_name, a.atttypid::int4 AS type_oid
                 FROM pg_attribute a
                 JOIN pg_class c ON a.attrelid = c.oid
                 JOIN pg_namespace n ON c.relnamespace = n.oid
                 WHERE n.nspname = 'public'
                   AND c.relname = $1
                   AND a.attnum > 0
                   AND NOT a.attisdropped
                 ORDER BY a.attnum",
                &[&table_name],
            )
            .await;

        if let Ok(rows) = catalog_result {
            if !rows.is_empty() {
                let mut columns = Vec::new();
                for row in rows {
                    columns.push(ColumnInfo {
                        name: row.get(0),
                        type_oid: row.get::<_, i32>(1),
                    });
                }
                return Ok(columns);
            }
        }

        let rows = transaction
            .query(
                "SELECT column_name, data_type, udt_name
                 FROM information_schema.columns
                 WHERE table_schema = 'public' AND table_name = $1
                 ORDER BY ordinal_position",
                &[&table_name],
            )
            .await?;

        let mut columns = Vec::new();
        for row in rows {
            let name: String = row.get(0);
            let data_type: String = row.get(1);
            let udt_name: String = row.get(2);
            let type_oid = oid_from_information_schema(&data_type, Some(&udt_name)) as i32;
            columns.push(ColumnInfo { name, type_oid });
        }

        Ok(columns)
    }

    /// Query primary key information for all tables in the database.
    async fn query_primary_keys(&mut self, client: &Client) -> Result<()> {
        info!("Querying primary key information from PostgreSQL system catalogs");

        let query = r#"
            SELECT
                n.nspname as schema_name,
                c.relname as table_name,
                a.attname as column_name
            FROM pg_constraint con
            JOIN pg_class c ON con.conrelid = c.oid
            JOIN pg_namespace n ON c.relnamespace = n.oid
            JOIN pg_attribute a ON a.attrelid = c.oid
            WHERE con.contype = 'p'  -- Primary key constraint
                AND a.attnum = ANY(con.conkey)
                AND n.nspname NOT IN ('pg_catalog', 'information_schema')
            ORDER BY n.nspname, c.relname, array_position(con.conkey, a.attnum)
        "#;

        let rows = client.query(query, &[]).await?;

        let mut primary_keys: HashMap<String, Vec<String>> = HashMap::new();

        for row in rows {
            let schema: &str = row.get(0);
            let table: &str = row.get(1);
            let column: &str = row.get(2);

            // Use fully qualified table name if not in public schema
            let table_key = if schema == "public" {
                table.to_string()
            } else {
                format!("{schema}.{table}")
            };

            primary_keys
                .entry(table_key.clone())
                .or_default()
                .push(column.to_string());

            debug!("Found primary key column '{column}' for table '{table_key}'");
        }

        // Add user-configured key columns (these override detected ones)
        for table_key_config in &self.config.table_keys {
            let table_name = &table_key_config.table;
            let key_columns = &table_key_config.key_columns;

            if !key_columns.is_empty() {
                info!(
                    "Using user-configured key columns for table '{table_name}': {key_columns:?}"
                );
                primary_keys.insert(table_name.clone(), key_columns.clone());
            }
        }

        // Store the primary keys
        self.table_primary_keys = primary_keys.clone();

        info!("Found primary keys for {} tables", primary_keys.len());
        for (table, keys) in &primary_keys {
            info!("Table '{table}' primary key columns: {keys:?}");
        }

        Ok(())
    }

    /// Convert a PostgreSQL row to a SourceChange
    async fn row_to_source_change(
        &self,
        row: &Row,
        table: &str,
        columns: &[ColumnInfo],
    ) -> Result<SourceChange> {
        let mut properties = ElementPropertyMap::new();

        // Get primary key columns for this table
        let pk_columns = self.table_primary_keys.get(table);

        // Collect values for element ID generation
        let mut pk_values = Vec::new();

        for (idx, column) in columns.iter().enumerate() {
            // Check if this column is a primary key
            let is_pk = pk_columns
                .map(|pks| pks.contains(&column.name))
                .unwrap_or(false);

            // Convert via shared PostgresValue so bootstrap ≡ CDC (#670/#672).
            let pg_value = row_column_to_postgres_value(row, idx, column.type_oid as u32);
            let element_value = pg_value.to_element_value();

            // If this is a primary key column, collect its value for the element ID
            if is_pk {
                if let Some(value_str) = pg_value.to_key_string() {
                    pk_values.push(value_str);
                }
            }

            properties.insert(&column.name, element_value);
        }

        // Generate element ID based on primary key values
        // Always include table name as prefix to ensure uniqueness across tables
        let elem_id = if !pk_values.is_empty() {
            // Use table name prefix with primary key values
            format!("{}:{}", table, pk_values.join("_"))
        } else if pk_columns.is_none() || pk_columns.map(|pks| pks.is_empty()).unwrap_or(true) {
            // No primary key defined and none configured - require user configuration
            warn!(
                "No primary key found for table '{table}'. Consider adding 'table_keys' configuration."
            );
            // Generate a UUID as fallback with table prefix
            format!("{}:{}", table, uuid::Uuid::new_v4())
        } else {
            // Primary key columns defined but all values are NULL - use UUID with table prefix
            format!("{}:{}", table, uuid::Uuid::new_v4())
        };

        let metadata = ElementMetadata {
            reference: ElementReference::new(&self.source_id, &elem_id),
            labels: Arc::from(vec![Arc::from(table)]),
            effective_from: chrono::Utc::now().timestamp_millis() as u64,
        };

        let element = Element::Node {
            metadata,
            properties,
        };

        Ok(SourceChange::Insert { element })
    }

    /// Send a batch of changes through the channel
    async fn send_batch(
        &self,
        batch: &mut Vec<SourceChangeEvent>,
        context: &BootstrapContext,
        event_tx: &drasi_lib::channels::BootstrapEventSender,
    ) -> Result<()> {
        for event in batch.drain(..) {
            // Get next sequence number for this bootstrap event
            let sequence = context.next_sequence();

            let bootstrap_event = drasi_lib::channels::BootstrapEvent {
                source_id: event.source_id,
                change: event.change,
                timestamp: event.timestamp,
                sequence,
            };
            event_tx.send(bootstrap_event).await.map_err(|e| {
                anyhow!("Failed to send bootstrap event to channel (channel may be closed): {e}")
            })?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ColumnInfo {
    name: String,
    type_oid: i32,
}

/// Read a single column from a tokio-postgres `Row` into the shared `PostgresValue`.
fn row_column_to_postgres_value(row: &Row, idx: usize, type_oid: u32) -> PostgresValue {
    // Helper: Option try_get that maps None → Null
    macro_rules! opt {
        ($t:ty, $map:expr) => {
            match row.try_get::<_, Option<$t>>(idx) {
                Ok(Some(v)) => $map(v),
                Ok(None) => PostgresValue::Null,
                Err(e) => {
                    warn!(
                        "Failed to read column idx={idx} oid={type_oid} as {}: {e}",
                        stringify!($t)
                    );
                    // Last resort: string
                    match row.try_get::<_, Option<String>>(idx) {
                        Ok(Some(s)) => PostgresValue::Text(s),
                        Ok(None) => PostgresValue::Null,
                        Err(_) => PostgresValue::Null,
                    }
                }
            }
        };
    }

    match type_oid {
        BOOL => opt!(bool, PostgresValue::Bool),
        INT2 => opt!(i16, PostgresValue::Int2),
        INT4 => opt!(i32, PostgresValue::Int4),
        INT8 => opt!(i64, PostgresValue::Int8),
        FLOAT4 => opt!(f32, PostgresValue::Float4),
        FLOAT8 => opt!(f64, PostgresValue::Float8),
        NUMERIC => opt!(rust_decimal::Decimal, PostgresValue::Numeric),
        TEXT | NAME => opt!(String, PostgresValue::Text),
        VARCHAR => opt!(String, PostgresValue::Varchar),
        CHAR => match row.try_get::<_, Option<String>>(idx) {
            Ok(Some(s)) => PostgresValue::Char(s.trim_end().to_string()),
            Ok(None) => PostgresValue::Null,
            Err(e) => {
                warn!("Failed to read char column idx={idx}: {e}");
                PostgresValue::Null
            }
        },
        UUID => opt!(uuid::Uuid, PostgresValue::Uuid),
        TIMESTAMP => opt!(chrono::NaiveDateTime, PostgresValue::Timestamp),
        TIMESTAMPTZ => opt!(chrono::DateTime<chrono::Utc>, PostgresValue::TimestampTz),
        DATE => opt!(chrono::NaiveDate, PostgresValue::Date),
        TIME => opt!(chrono::NaiveTime, PostgresValue::Time),
        JSON => opt!(serde_json::Value, PostgresValue::Json),
        JSONB => opt!(serde_json::Value, PostgresValue::Jsonb),
        BYTEA => match row.try_get::<_, Option<Vec<u8>>>(idx) {
            Ok(Some(bytes)) => PostgresValue::Bytea(bytes),
            Ok(None) => PostgresValue::Null,
            Err(e) => {
                // Sometimes delivered as text \x...
                warn!("bytea try_get failed idx={idx}: {e}; trying text");
                match row.try_get::<_, Option<String>>(idx) {
                    Ok(Some(s)) => match parse_bytea_text(&s) {
                        Ok(b) => PostgresValue::Bytea(b),
                        Err(_) => PostgresValue::Text(s),
                    },
                    Ok(None) => PostgresValue::Null,
                    Err(_) => PostgresValue::Null,
                }
            }
        },
        // Common 1-D arrays
        1007 /* int4[] */ => match row.try_get::<_, Option<Vec<i32>>>(idx) {
            Ok(Some(v)) => PostgresValue::Array(v.into_iter().map(PostgresValue::Int4).collect()),
            Ok(None) => PostgresValue::Null,
            Err(_) => try_array_as_text(row, idx),
        },
        1009 /* text[] */ => match row.try_get::<_, Option<Vec<String>>>(idx) {
            Ok(Some(v)) => PostgresValue::Array(v.into_iter().map(PostgresValue::Text).collect()),
            Ok(None) => PostgresValue::Null,
            Err(_) => try_array_as_text(row, idx),
        },
        1016 /* int8[] */ => match row.try_get::<_, Option<Vec<i64>>>(idx) {
            Ok(Some(v)) => PostgresValue::Array(v.into_iter().map(PostgresValue::Int8).collect()),
            Ok(None) => PostgresValue::Null,
            Err(_) => try_array_as_text(row, idx),
        },
        1005 /* int2[] */ => match row.try_get::<_, Option<Vec<i16>>>(idx) {
            Ok(Some(v)) => PostgresValue::Array(v.into_iter().map(PostgresValue::Int2).collect()),
            Ok(None) => PostgresValue::Null,
            Err(_) => try_array_as_text(row, idx),
        },
        1000 /* bool[] */ => match row.try_get::<_, Option<Vec<bool>>>(idx) {
            Ok(Some(v)) => PostgresValue::Array(v.into_iter().map(PostgresValue::Bool).collect()),
            Ok(None) => PostgresValue::Null,
            Err(_) => try_array_as_text(row, idx),
        },
        _ => {
            // Unknown / other arrays: try string, else Null
            match row.try_get::<_, Option<String>>(idx) {
                Ok(Some(s)) => {
                    // If it looks like an array literal, try shared text decode
                    if s.starts_with('{') {
                        match drasi_postgres_common::decode_text_to_postgres_value(&s, type_oid) {
                            Ok(v) => v,
                            Err(_) => PostgresValue::Text(s),
                        }
                    } else {
                        PostgresValue::Text(s)
                    }
                }
                Ok(None) => PostgresValue::Null,
                Err(e) => {
                    warn!(
                        "Unsupported PG type oid={type_oid} at idx={idx}: {e}; emitting Null"
                    );
                    PostgresValue::Null
                }
            }
        }
    }
}

fn try_array_as_text(row: &Row, idx: usize) -> PostgresValue {
    match row.try_get::<_, Option<String>>(idx) {
        Ok(Some(s)) => match drasi_postgres_common::decode_text_to_postgres_value(&s, 1009) {
            Ok(v) => v,
            Err(_) => PostgresValue::Text(s),
        },
        Ok(None) => PostgresValue::Null,
        Err(_) => PostgresValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use drasi_core::models::validate_effective_from;

    /// Validates that the timestamp pattern used in convert_row_to_source_change
    /// produces a value in the millisecond range, not nanoseconds.
    ///
    /// This test would have caught the original bug where timestamp_nanos_opt()
    /// was used instead of timestamp_millis().
    #[test]
    fn effective_from_uses_milliseconds() {
        let effective_from = chrono::Utc::now().timestamp_millis() as u64;
        assert!(
            validate_effective_from(effective_from).is_ok(),
            "Postgres bootstrapper effective_from ({effective_from}) should be in millisecond range"
        );
    }

    /// Verifies that using nanoseconds would be caught by the validator.
    #[test]
    fn effective_from_rejects_nanoseconds_pattern() {
        // This is the OLD buggy pattern — should fail validation
        let bad_effective_from = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64;
        assert!(
            validate_effective_from(bad_effective_from).is_err(),
            "Nanosecond timestamp ({bad_effective_from}) should be rejected"
        );
    }

    /// The bootstrap snapshot boundary must be the 16-byte
    /// `[ snapshot_lsn (8 BE) | u64::MAX (8 BE) ]` encoding so it lines up with
    /// the CDC source's `[ commit_lsn | offset ]` positions and suppresses every
    /// change of a transaction whose commit_lsn equals the snapshot LSN.
    #[test]
    fn snapshot_position_bytes_is_16_byte_max_padded() {
        let lsn = 0x0000_0000_0152_00b0u64;
        let pos = super::snapshot_position_bytes(lsn);
        assert_eq!(pos.len(), 16, "snapshot boundary must be 16 bytes");

        let mut expected = Vec::with_capacity(16);
        expected.extend_from_slice(&lsn.to_be_bytes());
        expected.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(pos.as_ref(), expected.as_slice());
    }
}
