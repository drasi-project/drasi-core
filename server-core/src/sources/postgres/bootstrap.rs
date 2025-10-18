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

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_postgres::{Client, NoTls, Row, Transaction};

use crate::channels::{
    BootstrapRequest, SourceChangeEvent, SourceEvent, SourceEventSender, SourceEventWrapper,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

use super::PostgresReplicationConfig;

/// Handles bootstrap operations for PostgreSQL source
pub struct BootstrapHandler {
    config: PostgresReplicationConfig,
    source_id: String,
    source_event_tx: SourceEventSender,
    /// Stores the LSN at bootstrap time for replication coordination
    pub snapshot_lsn: Arc<RwLock<Option<String>>>,
    /// Stores primary key information for each table
    pub table_primary_keys: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

impl BootstrapHandler {
    pub fn new(
        config: PostgresReplicationConfig,
        source_id: String,
        source_event_tx: SourceEventSender,
    ) -> Self {
        Self {
            config,
            source_id,
            source_event_tx,
            snapshot_lsn: Arc::new(RwLock::new(None)),
            table_primary_keys: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Execute bootstrap for the given request
    pub async fn execute(&self, request: BootstrapRequest) -> Result<usize> {
        info!(
            "Starting bootstrap for query '{}' with {} node labels and {} relation labels",
            request.query_id,
            request.node_labels.len(),
            request.relation_labels.len()
        );

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
        let (transaction, lsn) = self.create_snapshot(&mut client).await?;

        // Store LSN for replication coordination
        *self.snapshot_lsn.write().await = Some(lsn.clone());
        info!("Bootstrap snapshot created at LSN: {}", lsn);

        // Map labels to tables
        let tables = self.map_labels_to_tables(&request, &transaction).await?;
        info!(
            "Mapped {} labels to {} tables",
            request.node_labels.len() + request.relation_labels.len(),
            tables.len()
        );

        // Fetch and stream data from each table
        let mut total_count = 0;
        for (label, table_name) in tables {
            let count = self
                .bootstrap_table(&transaction, &label, &table_name)
                .await?;
            info!(
                "Bootstrapped {} rows from table '{}' with label '{}'",
                count, table_name, label
            );
            total_count += count;
        }

        // Commit transaction to release snapshot
        transaction.commit().await?;

        info!("Bootstrap completed: {} total elements sent", total_count);
        Ok(total_count)
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
                error!("PostgreSQL connection error: {}", e);
            }
        });

        Ok(client)
    }

    /// Create a consistent snapshot and capture current LSN
    async fn create_snapshot<'a>(
        &self,
        client: &'a mut Client,
    ) -> Result<(Transaction<'a>, String)> {
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

        // Export snapshot for consistency (optional, mainly for documentation)
        let _snapshot_row = transaction
            .query_one("SELECT pg_export_snapshot()", &[])
            .await?;

        Ok((transaction, lsn))
    }

    /// Map requested labels to actual table names
    async fn map_labels_to_tables(
        &self,
        request: &BootstrapRequest,
        transaction: &Transaction<'_>,
    ) -> Result<Vec<(String, String)>> {
        let mut tables = Vec::new();

        // Combine all labels (treating nodes and relations the same)
        let all_labels: Vec<String> = request
            .node_labels
            .iter()
            .chain(request.relation_labels.iter())
            .cloned()
            .collect();

        for label in all_labels {
            // Default mapping: uppercase label to lowercase table name
            let table_name = label.to_lowercase();

            // Check if table exists
            let exists = self.table_exists(transaction, &table_name).await?;

            if exists {
                tables.push((label, table_name));
            } else {
                warn!(
                    "Table '{}' for label '{}' does not exist, skipping",
                    table_name, label
                );
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
        label: &str,
        table_name: &str,
    ) -> Result<usize> {
        debug!(
            "Starting bootstrap of table '{}' with label '{}'",
            table_name, label
        );

        // Get table columns for proper type handling
        let columns = self.get_table_columns(transaction, table_name).await?;

        // Use cursor for memory efficiency
        let query = format!("SELECT * FROM {}", table_name);
        let rows = transaction.query(&query, &[]).await?;

        let mut count = 0;
        let mut batch = Vec::new();
        let batch_size = 1000;

        for row in rows {
            let source_change = self
                .row_to_source_change(&row, label, table_name, &columns)
                .await?;

            batch.push(SourceChangeEvent {
                source_id: self.source_id.clone(),
                change: source_change,
                timestamp: chrono::Utc::now(),
            });

            if batch.len() >= batch_size {
                self.send_batch(&mut batch).await?;
                count += batch_size;
            }
        }

        // Send remaining batch
        if !batch.is_empty() {
            count += batch.len();
            self.send_batch(&mut batch).await?;
        }

        Ok(count)
    }

    /// Get column information for a table
    async fn get_table_columns(
        &self,
        transaction: &Transaction<'_>,
        table_name: &str,
    ) -> Result<Vec<ColumnInfo>> {
        let rows = transaction
            .query(
                "SELECT column_name, data_type, 
                        CASE 
                            WHEN data_type = 'character varying' THEN 1043
                            WHEN data_type = 'integer' THEN 23
                            WHEN data_type = 'bigint' THEN 20
                            WHEN data_type = 'smallint' THEN 21
                            WHEN data_type = 'text' THEN 25
                            WHEN data_type = 'boolean' THEN 16
                            WHEN data_type = 'numeric' THEN 1700
                            WHEN data_type = 'real' THEN 700
                            WHEN data_type = 'double precision' THEN 701
                            WHEN data_type = 'timestamp without time zone' THEN 1114
                            WHEN data_type = 'timestamp with time zone' THEN 1184
                            WHEN data_type = 'date' THEN 1082
                            WHEN data_type = 'uuid' THEN 2950
                            WHEN data_type = 'json' THEN 114
                            WHEN data_type = 'jsonb' THEN 3802
                            ELSE 25  -- Default to text
                        END as type_oid
                 FROM information_schema.columns
                 WHERE table_schema = 'public' AND table_name = $1
                 ORDER BY ordinal_position",
                &[&table_name],
            )
            .await?;

        let mut columns = Vec::new();
        for row in rows {
            columns.push(ColumnInfo {
                name: row.get(0),
                data_type: row.get(1),
                type_oid: row.get::<_, i32>(2),
            });
        }

        Ok(columns)
    }

    /// Query primary key information for all tables in the database.
    ///
    /// This method:
    /// 1. Queries PostgreSQL system catalogs (pg_constraint, pg_class, pg_attribute)
    /// 2. Identifies primary key columns for each table
    /// 3. Stores them in table_primary_keys HashMap for use during element ID generation
    /// 4. Overlays user-configured key columns from table_keys config
    ///
    /// The primary key information is critical for generating stable element IDs
    /// that remain consistent across INSERT, UPDATE, and DELETE operations.
    async fn query_primary_keys(&self, client: &Client) -> Result<()> {
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
                format!("{}.{}", schema, table)
            };

            primary_keys
                .entry(table_key.clone())
                .or_default()
                .push(column.to_string());

            debug!(
                "Found primary key column '{}' for table '{}'",
                column, table_key
            );
        }

        // Add user-configured key columns (these override detected ones)
        for table_key_config in &self.config.table_keys {
            let table_name = &table_key_config.table;
            let key_columns = &table_key_config.key_columns;

            if !key_columns.is_empty() {
                info!(
                    "Using user-configured key columns for table '{}': {:?}",
                    table_name, key_columns
                );
                primary_keys.insert(table_name.clone(), key_columns.clone());
            }
        }

        // Store the primary keys
        *self.table_primary_keys.write().await = primary_keys.clone();

        info!("Found primary keys for {} tables", primary_keys.len());
        for (table, keys) in &primary_keys {
            info!("Table '{}' primary key columns: {:?}", table, keys);
        }

        Ok(())
    }

    /// Convert a PostgreSQL row to a SourceChange
    async fn row_to_source_change(
        &self,
        row: &Row,
        label: &str,
        table_name: &str,
        columns: &[ColumnInfo],
    ) -> Result<SourceChange> {
        let mut properties = ElementPropertyMap::new();

        // Get primary key columns for this table
        let primary_keys = self.table_primary_keys.read().await;
        let pk_columns = primary_keys.get(table_name);

        // Collect values for element ID generation
        let mut pk_values = Vec::new();

        for (idx, column) in columns.iter().enumerate() {
            // Check if this column is a primary key
            let is_pk = pk_columns
                .map(|pks| pks.contains(&column.name))
                .unwrap_or(false);

            // Get the value for this column
            let element_value = match column.type_oid {
                16 => {
                    // boolean
                    if let Ok(Some(val)) = row.try_get::<_, Option<bool>>(idx) {
                        drasi_core::models::ElementValue::Bool(val)
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
                21 | 23 | 20 => {
                    // int2, int4, int8
                    if let Ok(Some(val)) = row.try_get::<_, Option<i64>>(idx) {
                        drasi_core::models::ElementValue::Integer(val)
                    } else if let Ok(Some(val)) = row.try_get::<_, Option<i32>>(idx) {
                        drasi_core::models::ElementValue::Integer(val as i64)
                    } else if let Ok(Some(val)) = row.try_get::<_, Option<i16>>(idx) {
                        drasi_core::models::ElementValue::Integer(val as i64)
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
                700 | 701 => {
                    // float4, float8
                    if let Ok(Some(val)) = row.try_get::<_, Option<f64>>(idx) {
                        drasi_core::models::ElementValue::Float(ordered_float::OrderedFloat(val))
                    } else if let Ok(Some(val)) = row.try_get::<_, Option<f32>>(idx) {
                        drasi_core::models::ElementValue::Float(ordered_float::OrderedFloat(
                            val as f64,
                        ))
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
                1700 => {
                    // numeric/decimal
                    if let Ok(Some(val)) = row.try_get::<_, Option<rust_decimal::Decimal>>(idx) {
                        drasi_core::models::ElementValue::Float(ordered_float::OrderedFloat(
                            val.to_string().parse::<f64>().unwrap_or(0.0),
                        ))
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
                25 | 1043 | 19 => {
                    // text, varchar, name
                    if let Ok(Some(val)) = row.try_get::<_, Option<String>>(idx) {
                        drasi_core::models::ElementValue::String(std::sync::Arc::from(val))
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
                1114 | 1184 => {
                    // timestamp, timestamptz
                    if let Ok(Some(val)) = row.try_get::<_, Option<chrono::NaiveDateTime>>(idx) {
                        drasi_core::models::ElementValue::String(std::sync::Arc::from(
                            val.to_string(),
                        ))
                    } else if let Ok(Some(val)) =
                        row.try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(idx)
                    {
                        drasi_core::models::ElementValue::String(std::sync::Arc::from(
                            val.to_string(),
                        ))
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
                _ => {
                    // Default: try to get as string
                    if let Ok(Some(val)) = row.try_get::<_, Option<String>>(idx) {
                        drasi_core::models::ElementValue::String(std::sync::Arc::from(val))
                    } else {
                        drasi_core::models::ElementValue::Null
                    }
                }
            };

            // If this is a primary key column, collect its value for the element ID
            if is_pk && !matches!(element_value, drasi_core::models::ElementValue::Null) {
                let value_str = match &element_value {
                    drasi_core::models::ElementValue::Integer(i) => i.to_string(),
                    drasi_core::models::ElementValue::Float(f) => f.to_string(),
                    drasi_core::models::ElementValue::String(s) => s.to_string(),
                    drasi_core::models::ElementValue::Bool(b) => b.to_string(),
                    _ => format!("{:?}", element_value),
                };
                pk_values.push(value_str);
            }

            properties.insert(&column.name, element_value);
        }

        // Generate element ID based on primary key values
        // Always include table name as prefix to ensure uniqueness across tables
        let elem_id = if !pk_values.is_empty() {
            // Use table name prefix with primary key values
            format!("{}:{}", table_name, pk_values.join("_"))
        } else if pk_columns.is_none() || pk_columns.map(|pks| pks.is_empty()).unwrap_or(true) {
            // No primary key defined and none configured - require user configuration
            warn!(
                "No primary key found for table '{}'. Consider adding 'table_keys' configuration.",
                table_name
            );
            // Generate a UUID as fallback with table prefix
            format!("{}:{}", table_name, uuid::Uuid::new_v4())
        } else {
            // Primary key columns defined but all values are NULL - use UUID with table prefix
            format!("{}:{}", table_name, uuid::Uuid::new_v4())
        };

        let metadata = ElementMetadata {
            reference: ElementReference::new(&self.source_id, &elem_id),
            labels: Arc::from(vec![Arc::from(label)]),
            effective_from: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64,
        };

        let element = Element::Node {
            metadata,
            properties,
        };

        Ok(SourceChange::Insert { element })
    }

    /// Send a batch of changes through the channel
    async fn send_batch(&self, batch: &mut Vec<SourceChangeEvent>) -> Result<()> {
        for event in batch.drain(..) {
            let wrapper = SourceEventWrapper {
                source_id: event.source_id,
                event: SourceEvent::Change(event.change),
                timestamp: event.timestamp,
                profiling: None,
            };
            self.source_event_tx
                .send(wrapper)
                .await
                .map_err(|e| anyhow!("Failed to send bootstrap event: {}", e))?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ColumnInfo {
    name: String,
    #[allow(dead_code)]
    data_type: String,
    type_oid: i32,
}
