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

//! MS SQL bootstrap provider implementation

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, SourceChangeEvent};
use drasi_source_mssql::{MsSqlConnection, MsSqlSourceConfig, PrimaryKeyCache};
use log::{debug, info, warn};
use ordered_float::OrderedFloat;
use std::sync::Arc;
use tiberius::Row;

/// Bootstrap provider for MS SQL Server
///
/// Reads initial table snapshots for bootstrapping continuous queries.
pub struct MsSqlBootstrapProvider {
    config: MsSqlSourceConfig,
    source_id: String,
}

impl MsSqlBootstrapProvider {
    /// Create a new MS SQL bootstrap provider
    ///
    /// # Arguments
    /// * `source_id` - Identifier for the source
    /// * `config` - MS SQL configuration
    pub fn new(source_id: impl Into<String>, config: MsSqlSourceConfig) -> Self {
        Self {
            config,
            source_id: source_id.into(),
        }
    }

    /// Create a builder for the bootstrap provider
    pub fn builder() -> MsSqlBootstrapProviderBuilder {
        MsSqlBootstrapProviderBuilder::new()
    }
}

#[async_trait]
impl BootstrapProvider for MsSqlBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: tokio::sync::mpsc::Sender<drasi_lib::channels::BootstrapEvent>,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Starting MS SQL bootstrap for query '{}' with {} node labels",
            request.query_id,
            request.node_labels.len()
        );

        // Create bootstrap handler
        let mut handler = MsSqlBootstrapHandler::new(self.config.clone(), self.source_id.clone());

        // Store query_id before moving request
        let query_id = request.query_id.clone();

        // Execute bootstrap
        let count = handler.execute(request, context, event_tx).await?;

        info!(
            "Completed MS SQL bootstrap for query {}: sent {} records",
            query_id, count
        );

        Ok(count)
    }
}

/// Builder for MS SQL bootstrap provider
pub struct MsSqlBootstrapProviderBuilder {
    config: MsSqlSourceConfig,
    source_id: String,
}

impl MsSqlBootstrapProviderBuilder {
    /// Create a new builder with defaults
    pub fn new() -> Self {
        Self {
            config: MsSqlSourceConfig::default(),
            source_id: "mssql-bootstrap".to_string(),
        }
    }

    /// Set the source ID
    pub fn with_source_id(mut self, id: impl Into<String>) -> Self {
        self.source_id = id.into();
        self
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

    /// Set the list of tables
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.config.tables = tables;
        self
    }

    /// Build the bootstrap provider
    pub fn build(self) -> Result<MsSqlBootstrapProvider> {
        if self.config.database.is_empty() {
            return Err(anyhow::anyhow!("Database name is required"));
        }
        if self.config.user.is_empty() {
            return Err(anyhow::anyhow!("Database user is required"));
        }

        Ok(MsSqlBootstrapProvider::new(self.source_id, self.config))
    }
}

impl Default for MsSqlBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Handles bootstrap operations for MS SQL source
struct MsSqlBootstrapHandler {
    config: MsSqlSourceConfig,
    source_id: String,
    pk_cache: PrimaryKeyCache,
}

impl MsSqlBootstrapHandler {
    fn new(config: MsSqlSourceConfig, source_id: String) -> Self {
        Self {
            config,
            source_id,
            pk_cache: PrimaryKeyCache::new(),
        }
    }

    /// Execute bootstrap for the given request
    async fn execute(
        &mut self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: tokio::sync::mpsc::Sender<BootstrapEvent>,
    ) -> Result<usize> {
        info!(
            "Bootstrap: Connecting to MS SQL at {}:{}",
            self.config.host, self.config.port
        );

        // Connect to MS SQL
        let mut connection = MsSqlConnection::connect(&self.config).await?;
        let client = connection.client_mut();

        // Discover primary keys for all configured tables
        info!("Discovering primary keys");
        self.pk_cache.discover_keys(client, &self.config).await?;

        // Map labels to tables (label -> table_name)
        let tables = self.map_labels_to_tables(&request)?;

        if tables.is_empty() {
            warn!("No tables to bootstrap");
            return Ok(0);
        }

        // Start bootstrap transaction with snapshot isolation
        info!("Starting bootstrap transaction with snapshot isolation");
        client
            .simple_query("SET TRANSACTION ISOLATION LEVEL SNAPSHOT")
            .await?
            .into_results()
            .await?;

        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;

        let mut total_count = 0;

        // Bootstrap each node label
        for (label, table_name) in &tables {
            info!(
                "Bootstrapping table '{table_name}' with label '{label}'"
            );

            let count = self
                .bootstrap_table(client, label, table_name, context, &event_tx)
                .await?;

            total_count += count;
            info!("Bootstrapped {count} records from table '{table_name}'");
        }

        // Commit transaction
        client
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;

        info!("Bootstrap transaction committed");

        Ok(total_count)
    }

    /// Map query labels to table names
    ///
    /// Uses configured tables and maps labels to table names with flexible matching.
    fn map_labels_to_tables(&self, request: &BootstrapRequest) -> Result<Vec<(String, String)>> {
        let mut tables = Vec::new();

        for label in &request.node_labels {
            // Try various matching strategies:
            // 1. Exact match
            // 2. Match with schema prefix (dbo.Label)
            // 3. Match ignoring schema (Label matches dbo.Label)
            // 4. Case-insensitive match

            let mut matched = false;

            // Try exact match first
            if self.config.tables.contains(label) {
                tables.push((label.clone(), label.clone()));
                matched = true;
            } else {
                // Try with dbo schema prefix
                let with_schema = format!("dbo.{label}");
                if self.config.tables.contains(&with_schema) {
                    tables.push((label.clone(), with_schema));
                    matched = true;
                } else {
                    // Try matching any configured table that ends with .{label}
                    for table in &self.config.tables {
                        if table == label || table.ends_with(&format!(".{label}")) {
                            tables.push((label.clone(), table.clone()));
                            matched = true;
                            break;
                        }
                    }
                }
            }

            if !matched {
                warn!(
                    "Table for label '{}' not found in configured tables {:?}, skipping",
                    label, self.config.tables
                );
            }
        }

        Ok(tables)
    }

    /// Bootstrap a single table
    async fn bootstrap_table(
        &self,
        client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
        label: &str,
        table_name: &str,
        context: &BootstrapContext,
        event_tx: &tokio::sync::mpsc::Sender<BootstrapEvent>,
    ) -> Result<usize> {
        debug!(
            "Starting bootstrap of table '{table_name}' with label '{label}'"
        );

        // Query all rows from the table
        let query = format!("SELECT * FROM {table_name}");
        let stream = client.query(&query, &[]).await?;
        let rows = stream.into_results().await?;

        let mut count = 0;
        let mut batch = Vec::new();
        let batch_size = 1000;

        for row_set in rows {
            for row in row_set {
                let source_change = self.row_to_source_change(&row, label, table_name).await?;

                batch.push(SourceChangeEvent {
                    source_id: self.source_id.clone(),
                    change: source_change,
                    timestamp: chrono::Utc::now(),
                });

                if batch.len() >= batch_size {
                    self.send_batch(&mut batch, context, event_tx).await?;
                    count += batch_size;
                }
            }
        }

        // Send remaining batch
        if !batch.is_empty() {
            count += batch.len();
            self.send_batch(&mut batch, context, event_tx).await?;
        }

        Ok(count)
    }

    /// Convert a row to a SourceChange
    async fn row_to_source_change(
        &self,
        row: &Row,
        label: &str,
        table_name: &str,
    ) -> Result<SourceChange> {
        let mut properties = ElementPropertyMap::new();

        // Process each column
        for (idx, column) in row.columns().iter().enumerate() {
            let column_name = column.name();

            // Convert the value
            let element_value = self.convert_column_value(row, idx, column)?;

            // Add to properties
            properties.insert(column_name, element_value);
        }

        // Generate element ID from primary key values
        let element_id = self.pk_cache.generate_element_id(table_name, row)?;

        // Create the element
        Ok(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(&self.source_id, &element_id),
                    labels: Arc::from([Arc::from(label)]),
                    effective_from: 0,
                },
                properties,
            },
        })
    }

    /// Convert a column value to ElementValue
    fn convert_column_value(
        &self,
        row: &Row,
        idx: usize,
        column: &tiberius::Column,
    ) -> Result<ElementValue> {
        use tiberius::ColumnType;

        match column.column_type() {
            ColumnType::Bit | ColumnType::Bitn => {
                if let Ok(Some(val)) = row.try_get::<bool, _>(idx) {
                    Ok(ElementValue::Bool(val))
                } else {
                    Ok(ElementValue::Null)
                }
            }
            ColumnType::Int1
            | ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Intn => {
                // Try different integer sizes - order matters for SQL Server types
                // INT (Int4) -> i32, BIGINT (Int8) -> i64, SMALLINT (Int2) -> i16, TINYINT (Int1) -> u8
                if let Ok(Some(val)) = row.try_get::<i32, _>(idx) {
                    Ok(ElementValue::Integer(val as i64))
                } else if let Ok(Some(val)) = row.try_get::<i64, _>(idx) {
                    Ok(ElementValue::Integer(val))
                } else if let Ok(Some(val)) = row.try_get::<i16, _>(idx) {
                    Ok(ElementValue::Integer(val as i64))
                } else if let Ok(Some(val)) = row.try_get::<u8, _>(idx) {
                    Ok(ElementValue::Integer(val as i64))
                } else {
                    Ok(ElementValue::Null)
                }
            }
            ColumnType::Float4
            | ColumnType::Float8
            | ColumnType::Floatn
            | ColumnType::Numericn
            | ColumnType::Decimaln => {
                // Try f32 first (Float4), then f64 (Float8)
                // For Decimal/Numeric, tiberius can return them as f64
                if let Ok(Some(val)) = row.try_get::<f32, _>(idx) {
                    Ok(ElementValue::Float(OrderedFloat(val as f64)))
                } else if let Ok(Some(val)) = row.try_get::<f64, _>(idx) {
                    Ok(ElementValue::Float(OrderedFloat(val)))
                } else {
                    Ok(ElementValue::Null)
                }
            }
            ColumnType::BigVarChar
            | ColumnType::BigChar
            | ColumnType::NVarchar
            | ColumnType::NChar
            | ColumnType::BigVarBin
            | ColumnType::BigBinary
            | ColumnType::Text
            | ColumnType::NText => {
                if let Ok(Some(val)) = row.try_get::<&str, _>(idx) {
                    Ok(ElementValue::String(Arc::from(val)))
                } else {
                    Ok(ElementValue::Null)
                }
            }
            ColumnType::Datetime
            | ColumnType::Datetime2
            | ColumnType::Datetime4
            | ColumnType::Datetimen => {
                if let Ok(Some(val)) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                    Ok(ElementValue::String(Arc::from(val.to_string().as_str())))
                } else if let Ok(Some(val)) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                    Ok(ElementValue::String(Arc::from(val.to_rfc3339().as_str())))
                } else {
                    Ok(ElementValue::Null)
                }
            }
            _ => {
                // For unsupported types, try to convert to string
                if let Ok(Some(val)) = row.try_get::<&str, _>(idx) {
                    Ok(ElementValue::String(Arc::from(val)))
                } else {
                    warn!(
                        "Unsupported column type {:?} for column {}, treating as NULL",
                        column.column_type(),
                        column.name()
                    );
                    Ok(ElementValue::Null)
                }
            }
        }
    }

    /// Send a batch of changes
    async fn send_batch(
        &self,
        batch: &mut Vec<SourceChangeEvent>,
        context: &BootstrapContext,
        event_tx: &tokio::sync::mpsc::Sender<BootstrapEvent>,
    ) -> Result<()> {
        for event in batch.drain(..) {
            // Get next sequence number for this bootstrap event
            let sequence = context.next_sequence();

            let bootstrap_event = BootstrapEvent {
                source_id: event.source_id,
                change: event.change,
                timestamp: event.timestamp,
                sequence,
            };

            event_tx
                .send(bootstrap_event)
                .await
                .map_err(|e| anyhow!("Failed to send bootstrap event: {}", e))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() {
        let provider = MsSqlBootstrapProvider::builder()
            .with_host("localhost")
            .with_database("testdb")
            .with_user("testuser")
            .with_password("testpass")
            .build()
            .unwrap();

        assert_eq!(provider.source_id, "mssql-bootstrap");
        assert_eq!(provider.config.host, "localhost");
    }

    #[test]
    fn test_builder_missing_required() {
        let result = MsSqlBootstrapProvider::builder()
            .with_host("localhost")
            .build();

        assert!(result.is_err());
    }
}
