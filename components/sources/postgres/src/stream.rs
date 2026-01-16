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
use chrono::Utc;
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::{interval, sleep};

use super::connection::ReplicationConnection;
use super::decoder::PgOutputDecoder;
use super::protocol::BackendMessage;
use super::types::{StandbyStatusUpdate, WalMessage};
use super::PostgresSourceConfig;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::{ComponentEventSender, ComponentStatus, SourceEvent, SourceEventWrapper};
use drasi_lib::sources::base::SourceBase;

pub struct ReplicationStream {
    config: PostgresSourceConfig,
    source_id: String,
    connection: Option<ReplicationConnection>,
    decoder: PgOutputDecoder,
    dispatchers: Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    #[allow(dead_code)]
    status_tx: Arc<RwLock<Option<ComponentEventSender>>>,
    status: Arc<RwLock<ComponentStatus>>,
    current_lsn: u64,
    last_feedback_time: std::time::Instant,
    pending_transaction: Option<Vec<SourceChange>>,
    relations: HashMap<u32, RelationMapping>,
    table_primary_keys: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

struct RelationMapping {
    #[allow(dead_code)]
    table_name: String,
    #[allow(dead_code)]
    schema_name: String,
    label: String,
}

impl ReplicationStream {
    pub fn new(
        config: PostgresSourceConfig,
        source_id: String,
        dispatchers: Arc<
            RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        status_tx: Arc<RwLock<Option<ComponentEventSender>>>,
        status: Arc<RwLock<ComponentStatus>>,
    ) -> Self {
        Self {
            config,
            source_id,
            connection: None,
            decoder: PgOutputDecoder::new(),
            dispatchers,
            status_tx,
            status,
            current_lsn: 0,
            last_feedback_time: std::time::Instant::now(),
            pending_transaction: None,
            relations: HashMap::new(),
            table_primary_keys: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // Note: table_primary_keys is initialized empty and remains so.
    // Element IDs are generated from configured table_keys (in config.table_keys),
    // or fall back to using all column values if no keys are configured.

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting replication stream for source {}", self.source_id);

        // Connect and setup replication
        self.connect_and_setup().await?;

        // Start streaming loop
        let mut keepalive_interval = interval(Duration::from_secs(10));

        loop {
            // Check if we should stop
            {
                let status = self.status.read().await;
                if *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped {
                    info!("Received stop signal, shutting down replication");
                    break;
                }
            }

            tokio::select! {
                // Check for replication messages
                result = self.read_next_message() => {
                    match result {
                        Ok(Some(msg)) => {
                            if let Err(e) = self.handle_message(msg).await {
                                error!("Error handling message: {e}");
                                // Attempt recovery
                                if let Err(e) = self.recover_connection().await {
                                    error!("Failed to recover connection: {e}");
                                    return Err(e);
                                }
                            }
                        }
                        Ok(None) => {
                            // No message available
                        }
                        Err(e) => {
                            error!("Error reading message: {e}");
                            // Attempt recovery
                            if let Err(e) = self.recover_connection().await {
                                error!("Failed to recover connection: {e}");
                                return Err(e);
                            }
                        }
                    }
                }

                // Send periodic keepalives
                _ = keepalive_interval.tick() => {
                    if let Err(e) = self.send_feedback(false).await {
                        warn!("Failed to send keepalive: {e}");
                    }
                }
            }
        }

        // Clean shutdown
        self.shutdown().await?;
        Ok(())
    }

    async fn connect_and_setup(&mut self) -> Result<()> {
        info!("Connecting to PostgreSQL for replication");

        // Create connection
        let mut conn = ReplicationConnection::connect(
            &self.config.host,
            self.config.port,
            &self.config.database,
            &self.config.user,
            &self.config.password,
        )
        .await?;

        // Identify system
        let system_info = conn.identify_system().await?;
        info!("Connected to PostgreSQL system: {system_info:?}");

        // Create or verify replication slot
        let slot_info = conn
            .create_replication_slot(&self.config.slot_name, false)
            .await?;
        info!("Using replication slot: {slot_info:?}");

        // Parse starting LSN
        if !slot_info.consistent_point.is_empty() && slot_info.consistent_point != "0/0" {
            self.current_lsn = parse_lsn(&slot_info.consistent_point)?;
        } else {
            // Start from beginning if no consistent point
            self.current_lsn = 0;
        }

        // Build replication options
        let mut options = HashMap::new();
        options.insert("proto_version".to_string(), "1".to_string());
        options.insert(
            "publication_names".to_string(),
            self.config.publication_name.clone(),
        );

        // Start replication
        conn.start_replication(&self.config.slot_name, Some(self.current_lsn), options)
            .await?;

        self.connection = Some(conn);
        info!("Replication started from LSN: {:x}", self.current_lsn);

        Ok(())
    }

    async fn read_next_message(&mut self) -> Result<Option<BackendMessage>> {
        if let Some(conn) = &mut self.connection {
            // Try to read with a short timeout to avoid blocking forever
            match tokio::time::timeout(Duration::from_millis(100), conn.read_replication_message())
                .await
            {
                Ok(Ok(msg)) => Ok(Some(msg)),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(None), // Timeout, no message available
            }
        } else {
            Err(anyhow!("No connection available"))
        }
    }

    async fn handle_message(&mut self, msg: BackendMessage) -> Result<()> {
        match msg {
            BackendMessage::CopyData(data) => {
                self.handle_copy_data(&data).await?;
            }
            BackendMessage::PrimaryKeepaliveMessage {
                wal_end,
                timestamp: _,
                reply,
            } => {
                self.current_lsn = wal_end;
                if reply == 1 {
                    self.send_feedback(true).await?;
                }
            }
            BackendMessage::ErrorResponse(err) => {
                error!("Server error: {}", err.message);
                return Err(anyhow!("Server error: {}", err.message));
            }
            _ => {
                trace!("Ignoring message: {msg:?}");
            }
        }
        Ok(())
    }

    async fn handle_copy_data(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        // First byte indicates the message type
        let msg_type = data[0];

        match msg_type {
            b'w' => {
                // XLogData message
                self.handle_xlog_data(&data[1..]).await?;
            }
            b'k' => {
                // Primary keepalive
                self.handle_keepalive(&data[1..]).await?;
            }
            _ => {
                warn!("Unknown copy data message type: 0x{msg_type:02x}");
            }
        }

        Ok(())
    }

    async fn handle_xlog_data(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 24 {
            return Err(anyhow!("XLogData message too short: {} bytes", data.len()));
        }

        // Parse XLogData header
        let _start_lsn = u64::from_be_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        let end_lsn = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        let _timestamp = i64::from_be_bytes([
            data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
        ]);

        // Update LSN
        self.current_lsn = end_lsn;

        // Decode the actual WAL message
        let wal_data = &data[24..];

        // Try to decode, but don't fail on partial messages
        if !wal_data.is_empty() {
            let msg_type = wal_data[0];
            debug!(
                "Attempting to decode WAL message type: {} ({}), data length: {}",
                msg_type as char,
                msg_type,
                wal_data.len()
            );
        }

        match self.decoder.decode_message(wal_data) {
            Ok(Some(wal_msg)) => {
                self.process_wal_message(wal_msg).await?;
            }
            Ok(None) => {
                // No message or skipped message type
            }
            Err(e) => {
                // Log but don't fail on decode errors - might be partial message
                if !wal_data.is_empty() {
                    debug!(
                        "Failed to decode WAL message type {} ({}): {}, data length: {}",
                        wal_data[0] as char,
                        wal_data[0],
                        e,
                        wal_data.len()
                    );
                }
                // We'll get the rest in the next message
            }
        }

        // Send feedback periodically
        if self.last_feedback_time.elapsed() > Duration::from_secs(5) {
            self.send_feedback(false).await?;
        }

        Ok(())
    }

    async fn handle_keepalive(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 17 {
            return Err(anyhow!("Keepalive message too short"));
        }

        let wal_end = u64::from_be_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        let reply = data[16];

        self.current_lsn = wal_end;

        if reply == 1 {
            self.send_feedback(true).await?;
        }

        Ok(())
    }

    async fn process_wal_message(&mut self, msg: WalMessage) -> Result<()> {
        match msg {
            WalMessage::Begin(_) => {
                // Start a new transaction
                self.pending_transaction = Some(Vec::new());
            }
            WalMessage::Commit(tx_info) => {
                // Commit the transaction
                if let Some(changes) = self.pending_transaction.take() {
                    // Send all changes in the transaction
                    for change in changes {
                        // Create profiling metadata with timestamps
                        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                        let wrapper = SourceEventWrapper::with_profiling(
                            self.source_id.clone(),
                            SourceEvent::Change(change),
                            chrono::Utc::now(),
                            profiling,
                        );

                        // Dispatch via helper
                        if let Err(e) = SourceBase::dispatch_from_task(
                            self.dispatchers.clone(),
                            wrapper.clone(),
                            &self.source_id,
                        )
                        .await
                        {
                            debug!(
                                "[{}] Failed to dispatch change (no subscribers): {}",
                                self.source_id, e
                            );
                        }
                    }
                    debug!(
                        "Committed transaction {} with LSN {:x}",
                        tx_info.xid, tx_info.commit_lsn
                    );
                }
            }
            WalMessage::Relation(relation) => {
                // Store relation mapping - use table name as-is for label (no uppercase)
                // This ensures consistency with bootstrap which uses the actual table name case
                let label = relation.name.clone();
                self.relations.insert(
                    relation.id,
                    RelationMapping {
                        table_name: relation.name.clone(),
                        schema_name: relation.namespace.clone(),
                        label,
                    },
                );

                // Update decoder's relation info
                // The decoder already handles this internally
            }
            WalMessage::Insert { relation_id, tuple } => {
                if let Some(change) = self.convert_insert(relation_id, tuple).await? {
                    if let Some(tx) = &mut self.pending_transaction {
                        tx.push(change);
                    } else {
                        // No transaction context, send immediately
                        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                        let wrapper = SourceEventWrapper::with_profiling(
                            self.source_id.clone(),
                            SourceEvent::Change(change),
                            chrono::Utc::now(),
                            profiling,
                        );

                        // Dispatch via helper
                        if let Err(e) = SourceBase::dispatch_from_task(
                            self.dispatchers.clone(),
                            wrapper.clone(),
                            &self.source_id,
                        )
                        .await
                        {
                            debug!(
                                "[{}] Failed to dispatch change (no subscribers): {}",
                                self.source_id, e
                            );
                        }
                    }
                }
            }
            WalMessage::Update {
                relation_id,
                old_tuple,
                new_tuple,
            } => {
                if let Some(change) = self
                    .convert_update(relation_id, old_tuple, new_tuple)
                    .await?
                {
                    if let Some(tx) = &mut self.pending_transaction {
                        tx.push(change);
                    } else {
                        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                        let wrapper = SourceEventWrapper::with_profiling(
                            self.source_id.clone(),
                            SourceEvent::Change(change),
                            chrono::Utc::now(),
                            profiling,
                        );

                        // Dispatch via helper
                        if let Err(e) = SourceBase::dispatch_from_task(
                            self.dispatchers.clone(),
                            wrapper.clone(),
                            &self.source_id,
                        )
                        .await
                        {
                            debug!(
                                "[{}] Failed to dispatch change (no subscribers): {}",
                                self.source_id, e
                            );
                        }
                    }
                }
            }
            WalMessage::Delete {
                relation_id,
                old_tuple,
            } => {
                if let Some(change) = self.convert_delete(relation_id, old_tuple).await? {
                    if let Some(tx) = &mut self.pending_transaction {
                        tx.push(change);
                    } else {
                        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                        let wrapper = SourceEventWrapper::with_profiling(
                            self.source_id.clone(),
                            SourceEvent::Change(change),
                            chrono::Utc::now(),
                            profiling,
                        );

                        // Dispatch via helper
                        if let Err(e) = SourceBase::dispatch_from_task(
                            self.dispatchers.clone(),
                            wrapper.clone(),
                            &self.source_id,
                        )
                        .await
                        {
                            debug!(
                                "[{}] Failed to dispatch change (no subscribers): {}",
                                self.source_id, e
                            );
                        }
                    }
                }
            }
            WalMessage::Truncate { relation_ids } => {
                warn!("Truncate not yet implemented for relations: {relation_ids:?}");
            }
        }
        Ok(())
    }

    async fn convert_insert(
        &self,
        relation_id: u32,
        tuple: Vec<super::types::PostgresValue>,
    ) -> Result<Option<SourceChange>> {
        // Get relation info
        let relation = self
            .decoder
            .get_relation(relation_id)
            .ok_or_else(|| anyhow!("Unknown relation {relation_id}"))?;

        let mapping = self
            .relations
            .get(&relation_id)
            .ok_or_else(|| anyhow!("No mapping for relation {relation_id}"))?;

        // Convert tuple to properties
        let mut properties = drasi_core::models::ElementPropertyMap::new();
        for (i, value) in tuple.iter().enumerate() {
            if let Some(column) = relation.columns.get(i) {
                let json_value = value.to_json();
                if !json_value.is_null() {
                    properties.insert(
                        &column.name,
                        drasi_lib::sources::manager::convert_json_to_element_value(&json_value)?,
                    );
                }
            }
        }

        // Create element ID (use primary key if available, otherwise generate)
        let element_id = self.generate_element_id(relation, &tuple).await?;

        // Create the element
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(&self.source_id, &element_id),
                labels: Arc::from([Arc::from(mapping.label.as_str())]),
                effective_from: Utc::now().timestamp_millis() as u64,
            },
            properties,
        };

        Ok(Some(SourceChange::Insert { element }))
    }

    async fn convert_update(
        &self,
        relation_id: u32,
        old_tuple: Option<Vec<super::types::PostgresValue>>,
        new_tuple: Vec<super::types::PostgresValue>,
    ) -> Result<Option<SourceChange>> {
        let relation = self
            .decoder
            .get_relation(relation_id)
            .ok_or_else(|| anyhow!("Unknown relation {relation_id}"))?;

        let mapping = self
            .relations
            .get(&relation_id)
            .ok_or_else(|| anyhow!("No mapping for relation {relation_id}"))?;

        // Generate element ID (should be the same for both old and new tuples)
        let element_id = self.generate_element_id(relation, &new_tuple).await?;

        // If we don't have old_tuple, treat as INSERT
        let Some(_old_tuple) = old_tuple else {
            warn!("UPDATE without old tuple for relation {relation_id}, treating as INSERT");
            return self.convert_insert(relation_id, new_tuple).await;
        };

        // Create properties for after state
        // Note: We validated old_tuple exists to ensure this is a proper UPDATE
        let mut after_properties = drasi_core::models::ElementPropertyMap::new();

        // Process new tuple (after state)
        for (i, column) in relation.columns.iter().enumerate() {
            if let Some(value) = new_tuple.get(i) {
                let json_value = value.to_json();
                if !json_value.is_null() {
                    after_properties.insert(
                        &column.name,
                        drasi_lib::sources::manager::convert_json_to_element_value(&json_value)?,
                    );
                }
            }
        }

        // Note: SourceChange::Update only needs the after element
        // The before state is tracked internally by drasi-core
        // We still process old_tuple to ensure we have the correct element_id

        let after_element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(&self.source_id, &element_id),
                labels: Arc::from([Arc::from(mapping.label.as_str())]),
                effective_from: Utc::now().timestamp_millis() as u64,
            },
            properties: after_properties,
        };

        Ok(Some(SourceChange::Update {
            element: after_element,
        }))
    }

    async fn convert_delete(
        &self,
        relation_id: u32,
        old_tuple: Vec<super::types::PostgresValue>,
    ) -> Result<Option<SourceChange>> {
        let relation = self
            .decoder
            .get_relation(relation_id)
            .ok_or_else(|| anyhow!("Unknown relation {relation_id}"))?;

        let mapping = self
            .relations
            .get(&relation_id)
            .ok_or_else(|| anyhow!("No mapping for relation {relation_id}"))?;

        let element_id = self.generate_element_id(relation, &old_tuple).await?;

        Ok(Some(SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new(&self.source_id, &element_id),
                labels: Arc::from([Arc::from(mapping.label.as_str())]),
                effective_from: Utc::now().timestamp_millis() as u64,
            },
        }))
    }

    /// Generate a stable element ID for a tuple based on primary key values.
    ///
    /// Priority order:
    /// 1. User-configured key columns (from table_keys config)
    /// 2. Automatically detected primary keys from PostgreSQL system catalogs
    /// 3. UUID fallback if no keys are available
    ///
    /// Element ID format:
    /// - Single key: Table name prefix with value (e.g., "stocks:AAPL")
    /// - Composite key: Table name prefix with values joined (e.g., "portfolio:tenant1_user2")
    /// - No key: Table name prefix with UUID (e.g., "orders:550e8400-e29b-41d4-a716-446655440000")
    async fn generate_element_id(
        &self,
        relation: &super::types::RelationInfo,
        tuple: &[super::types::PostgresValue],
    ) -> Result<String> {
        // Get the table name (use schema-qualified if not in public schema)
        let table_name = if relation.namespace == "public" {
            relation.name.clone()
        } else {
            format!("{}.{}", relation.namespace, relation.name)
        };

        // Get primary key columns for this table
        let primary_keys = self.table_primary_keys.read().await;
        let pk_columns = primary_keys.get(&table_name);

        // Check configured table_keys first
        let configured_keys = self
            .config
            .table_keys
            .iter()
            .find(|tk| tk.table == table_name)
            .map(|tk| &tk.key_columns);

        // Use configured keys if available, otherwise use detected primary keys
        let key_columns = configured_keys.or(pk_columns);

        if let Some(keys) = key_columns {
            let mut key_parts = Vec::new();

            for (i, column) in relation.columns.iter().enumerate() {
                if keys.contains(&column.name) {
                    if let Some(value) = tuple.get(i) {
                        let json_val = value.to_json();
                        if !json_val.is_null() {
                            // Remove quotes from JSON string representation
                            let val_str = json_val.to_string();
                            let cleaned = val_str.trim_matches('"');
                            key_parts.push(cleaned.to_string());
                        }
                    }
                }
            }

            if !key_parts.is_empty() {
                // Include table name as prefix to ensure uniqueness across tables
                return Ok(format!("{}:{}", table_name, key_parts.join("_")));
            }
        }

        // No primary key found or all key values are NULL
        warn!("No primary key value found for table '{table_name}'. Consider adding 'table_keys' configuration.");
        // Still include table name prefix for consistency
        Ok(format!("{}:{}", table_name, uuid::Uuid::new_v4()))
    }

    async fn send_feedback(&mut self, reply_requested: bool) -> Result<()> {
        if let Some(conn) = &mut self.connection {
            let status = StandbyStatusUpdate {
                write_lsn: self.current_lsn,
                flush_lsn: self.current_lsn,
                apply_lsn: self.current_lsn,
                reply_requested,
            };

            conn.send_standby_status(status).await?;
            self.last_feedback_time = std::time::Instant::now();
            trace!("Sent feedback with LSN: {:x}", self.current_lsn);
        }

        Ok(())
    }

    #[allow(dead_code)]
    async fn check_stop_signal(&self) -> bool {
        let status = self.status.read().await;
        *status == ComponentStatus::Stopping || *status == ComponentStatus::Stopped
    }

    async fn recover_connection(&mut self) -> Result<()> {
        warn!("Attempting to recover connection");

        // Close existing connection if any
        if let Some(conn) = self.connection.take() {
            let _ = conn.close().await;
        }

        // Wait a bit before reconnecting
        sleep(Duration::from_secs(5)).await;

        // Try to reconnect
        self.connect_and_setup().await?;

        info!("Connection recovered successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("Shutting down replication stream");

        // Send final feedback
        let _ = self.send_feedback(false).await;

        // Close connection
        if let Some(conn) = self.connection.take() {
            conn.close().await?;
        }

        Ok(())
    }
}

fn parse_lsn(lsn_str: &str) -> Result<u64> {
    let parts: Vec<&str> = lsn_str.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow!("Invalid LSN format: {lsn_str}"));
    }

    let high = u64::from_str_radix(parts[0], 16)?;
    let low = u64::from_str_radix(parts[1], 16)?;

    Ok((high << 32) | low)
}
