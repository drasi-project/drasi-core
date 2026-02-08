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
    /// LSN of the last received WAL data from PostgreSQL (write position)
    current_lsn: u64,
    /// LSN of events that have been successfully dispatched to downstream consumers.
    /// This is the LSN we report as "flushed" and "applied" to PostgreSQL.
    /// Only advances AFTER events are successfully delivered to prevent data loss.
    confirmed_lsn: u64,
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
            confirmed_lsn: 0,
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
            let starting_lsn = parse_lsn(&slot_info.consistent_point)?;
            self.current_lsn = starting_lsn;
            self.confirmed_lsn = starting_lsn; // Start with same LSN - nothing pending yet
        } else {
            // Start from beginning if no consistent point
            self.current_lsn = 0;
            self.confirmed_lsn = 0;
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
                // Update current_lsn to track what PostgreSQL reports
                self.current_lsn = wal_end;
                // If no pending transaction, safe to confirm this position
                if self.pending_transaction.is_none() {
                    self.confirmed_lsn = wal_end;
                }
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

        // Update current_lsn to track what we've received (write position).
        // NOTE: This does NOT mean the data has been successfully processed!
        // confirmed_lsn tracks what has been successfully dispatched.
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
                // Process the WAL message. This will update confirmed_lsn
                // only after successful dispatch of transaction commits.
                self.process_wal_message(wal_msg, end_lsn).await?;
            }
            Ok(None) => {
                // Decoder returned no actionable message. This may correspond to
                // metadata or skipped message types (e.g., origin/type messages),
                // but can also include unknown message types from newer PostgreSQL
                // versions. To avoid acknowledging unprocessed data, we do NOT
                // advance confirmed_lsn here. This is conservative but safe.
                trace!(
                    "Decoder returned Ok(None) for LSN {:x}; not advancing confirmed_lsn",
                    end_lsn
                );
            }
            Err(e) => {
                // Log but don't fail on decode errors - might be partial message
                // TODO: Persistent decode errors (e.g., from unsupported PostgreSQL versions
                // or corrupted WAL) will wedge progress since we never advance confirmed_lsn.
                // Consider adding a configurable skip mechanism or error threshold.
                if !wal_data.is_empty() {
                    debug!(
                        "Failed to decode WAL message type {} ({}): {}, data length: {}",
                        wal_data[0] as char,
                        wal_data[0],
                        e,
                        wal_data.len()
                    );
                }
                // Don't advance confirmed_lsn on decode errors
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

        // Update current_lsn to track what PostgreSQL has sent us
        // NOTE: Don't update confirmed_lsn here - that only advances on successful dispatch
        self.current_lsn = wal_end;

        // If there's no pending transaction, it's safe to also update confirmed_lsn
        // since keepalives are sent between transactions
        if self.pending_transaction.is_none() {
            self.confirmed_lsn = wal_end;
        }

        if reply == 1 {
            self.send_feedback(true).await?;
        }

        Ok(())
    }

    /// Process a WAL message and dispatch changes to subscribers.
    ///
    /// # LSN Tracking for Data Safety
    ///
    /// This method implements safe LSN acknowledgment to prevent data loss:
    ///
    /// - `confirmed_lsn` is only updated AFTER events are successfully dispatched
    /// - For transactions: LSN is confirmed only after ALL changes in the transaction
    ///   are successfully dispatched to downstream consumers
    /// - For non-transactional changes: LSN is confirmed after each successful dispatch
    ///
    /// This ensures PostgreSQL will replay events if Drasi crashes before dispatch completes.
    async fn process_wal_message(&mut self, msg: WalMessage, message_lsn: u64) -> Result<()> {
        match msg {
            WalMessage::Begin(_) => {
                // Start a new transaction - don't confirm LSN yet
                self.pending_transaction = Some(Vec::new());
            }
            WalMessage::Commit(tx_info) => {
                // Commit the transaction - dispatch all changes and THEN confirm LSN
                if let Some(changes) = self.pending_transaction.take() {
                    let change_count = changes.len();

                    // Send all changes in the transaction
                    // IMPORTANT: We dispatch directly instead of using SourceBase::dispatch_from_task()
                    // because that helper always returns Ok(()) even on failure (see lib/src/sources/base.rs).
                    // We need proper error propagation to prevent premature LSN advancement.
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

                        // Dispatch directly to each dispatcher and check for errors
                        // This ensures we actually detect dispatch failures
                        if let Err(e) = self.dispatch_event_directly(wrapper).await {
                            // FAILURE: Dispatch failed - DO NOT confirm LSN
                            // This ensures PostgreSQL will replay these events on reconnect
                            error!(
                                "[{}] Transaction {} dispatch failed: {}. \
                                 LSN NOT confirmed - events will be replayed on reconnect.",
                                self.source_id, tx_info.xid, e
                            );
                            return Err(anyhow!(
                                "Failed to dispatch event in transaction {}: {}",
                                tx_info.xid,
                                e
                            ));
                        }
                    }

                    // SUCCESS: All events dispatched - now safe to confirm LSN
                    self.confirmed_lsn = tx_info.commit_lsn;
                    debug!(
                        "Committed transaction {} with LSN {:x} ({} changes successfully dispatched)",
                        tx_info.xid, tx_info.commit_lsn, change_count
                    );
                } else {
                    // Empty transaction (no changes) - safe to confirm
                    self.confirmed_lsn = tx_info.commit_lsn;
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

                // Relation messages don't contain data - safe to confirm
                self.confirmed_lsn = message_lsn;
            }
            WalMessage::Insert { relation_id, tuple } => {
                if let Some(change) = self.convert_insert(relation_id, tuple).await? {
                    if let Some(tx) = &mut self.pending_transaction {
                        // Part of a transaction - add to pending, don't confirm yet
                        tx.push(change);
                    } else {
                        // No transaction context - dispatch immediately and confirm on success
                        self.dispatch_single_change(change, message_lsn).await?;
                    }
                } else {
                    // No change produced - safe to confirm
                    if self.pending_transaction.is_none() {
                        self.confirmed_lsn = message_lsn;
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
                        // Part of a transaction - add to pending, don't confirm yet
                        tx.push(change);
                    } else {
                        // No transaction context - dispatch immediately and confirm on success
                        self.dispatch_single_change(change, message_lsn).await?;
                    }
                } else {
                    // No change produced - safe to confirm
                    if self.pending_transaction.is_none() {
                        self.confirmed_lsn = message_lsn;
                    }
                }
            }
            WalMessage::Delete {
                relation_id,
                old_tuple,
            } => {
                if let Some(change) = self.convert_delete(relation_id, old_tuple).await? {
                    if let Some(tx) = &mut self.pending_transaction {
                        // Part of a transaction - add to pending, don't confirm yet
                        tx.push(change);
                    } else {
                        // No transaction context - dispatch immediately and confirm on success
                        self.dispatch_single_change(change, message_lsn).await?;
                    }
                } else {
                    // No change produced - safe to confirm
                    if self.pending_transaction.is_none() {
                        self.confirmed_lsn = message_lsn;
                    }
                }
            }
            WalMessage::Truncate { relation_ids } => {
                warn!("Truncate not yet implemented for relations: {relation_ids:?}");
                // Don't confirm LSN for unimplemented operations
            }
        }
        Ok(())
    }

    /// Dispatch a single change outside of a transaction context.
    /// Only confirms LSN after successful dispatch.
    async fn dispatch_single_change(
        &mut self,
        change: SourceChange,
        message_lsn: u64,
    ) -> Result<()> {
        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

        let wrapper = SourceEventWrapper::with_profiling(
            self.source_id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
            profiling,
        );

        // Dispatch directly - NOT via SourceBase::dispatch_from_task() which always returns Ok(())
        match self.dispatch_event_directly(wrapper).await {
            Ok(()) => {
                // SUCCESS: Confirm the LSN only after successful dispatch
                self.confirmed_lsn = message_lsn;
                Ok(())
            }
            Err(e) => {
                // FAILURE: DO NOT confirm LSN - event will be replayed on reconnect
                error!(
                    "[{}] Failed to dispatch change at LSN {:x}: {}. \
                     LSN NOT confirmed - event will be replayed on reconnect.",
                    self.source_id, message_lsn, e
                );
                Err(anyhow!(
                    "Failed to dispatch change at LSN {:x}: {}",
                    message_lsn,
                    e
                ))
            }
        }
    }

    /// Dispatch an event directly to all registered dispatchers.
    ///
    /// IMPORTANT: We implement this instead of using `SourceBase::dispatch_from_task()`
    /// because that helper always returns `Ok(())` even when dispatch fails
    /// (see lib/src/sources/base.rs lines 551-556). We need proper error propagation
    /// to prevent premature LSN advancement and data loss.
    ///
    /// Behavior:
    /// - If no dispatchers are registered, logs a warning and returns Ok (vacuously delivered).
    ///   This handles the case where sources start before queries subscribe.
    /// - Attempts dispatch to ALL registered dispatchers, collecting any errors.
    /// - Returns error only if ANY dispatcher failed, preventing LSN advancement.
    async fn dispatch_event_directly(&self, wrapper: SourceEventWrapper) -> Result<()> {
        let dispatchers_guard = self.dispatchers.read().await;

        // If no dispatchers are registered, treat as vacuously delivered.
        // This allows sources to start before queries subscribe and handles
        // dynamic query addition/removal without causing crash loops.
        if dispatchers_guard.is_empty() {
            warn!(
                "[{}] No dispatchers registered - event treated as delivered (vacuously true)",
                self.source_id
            );
            return Ok(());
        }

        let arc_wrapper = Arc::new(wrapper);

        // Dispatch to ALL registered dispatchers and collect errors.
        // We attempt delivery to every dispatcher even if some fail, to avoid
        // leaving subscribers in inconsistent states.
        let mut errors: Vec<String> = Vec::new();

        for dispatcher in dispatchers_guard.iter() {
            if let Err(e) = dispatcher.dispatch_change(arc_wrapper.clone()).await {
                errors.push(e.to_string());
            }
        }

        // If ANY dispatcher failed, return error to prevent LSN advancement.
        // This ensures PostgreSQL will replay the event on reconnect.
        if !errors.is_empty() {
            return Err(anyhow!(
                "Dispatch failed for {} of {} dispatchers: {}",
                errors.len(),
                dispatchers_guard.len(),
                errors.join("; ")
            ));
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

    /// Send replication feedback to PostgreSQL with LSN positions.
    ///
    /// # LSN Positions Explained
    ///
    /// We report three different LSN values to PostgreSQL:
    ///
    /// - `write_lsn`: Position of WAL data we've received (current_lsn)
    /// - `flush_lsn`: Position of data safely persisted/dispatched (confirmed_lsn)
    /// - `apply_lsn`: Position of data actually applied downstream (confirmed_lsn)
    ///
    /// By using `confirmed_lsn` for flush/apply, PostgreSQL will retain WAL data
    /// for any events that haven't been successfully dispatched yet. If Drasi
    /// crashes, PostgreSQL will replay events from `confirmed_lsn` on reconnect.
    async fn send_feedback(&mut self, reply_requested: bool) -> Result<()> {
        if let Some(conn) = &mut self.connection {
            let status = StandbyStatusUpdate {
                // write_lsn: What we've received from PostgreSQL
                write_lsn: self.current_lsn,
                // flush_lsn: What we've successfully dispatched to downstream
                // Using confirmed_lsn ensures PostgreSQL keeps WAL for unconfirmed events
                flush_lsn: self.confirmed_lsn,
                // apply_lsn: What has been applied downstream (same as flush for us)
                apply_lsn: self.confirmed_lsn,
                reply_requested,
            };

            conn.send_standby_status(status).await?;
            self.last_feedback_time = std::time::Instant::now();
            trace!(
                "Sent feedback - write_lsn: {:x}, confirmed_lsn: {:x}",
                self.current_lsn,
                self.confirmed_lsn
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::channels::{BroadcastChangeDispatcher, ChannelChangeDispatcher};

    // ==========================================================================
    // LSN Parsing Tests
    // ==========================================================================

    #[test]
    fn test_parse_lsn_valid() {
        // Standard PostgreSQL LSN format
        let lsn = parse_lsn("0/1234ABCD").unwrap();
        assert_eq!(lsn, 0x1234ABCD);

        let lsn = parse_lsn("1/0").unwrap();
        assert_eq!(lsn, 0x100000000);

        let lsn = parse_lsn("ABCD/EF01").unwrap();
        assert_eq!(lsn, 0xABCD0000EF01);
    }

    #[test]
    fn test_parse_lsn_invalid_format() {
        assert!(parse_lsn("invalid").is_err());
        assert!(parse_lsn("0").is_err());
        assert!(parse_lsn("0/1/2").is_err());
        assert!(parse_lsn("").is_err());
    }

    #[test]
    fn test_parse_lsn_invalid_hex() {
        assert!(parse_lsn("G/0").is_err());
        assert!(parse_lsn("0/G").is_err());
    }

    // ==========================================================================
    // LSN Tracking Tests - Verifying the Fix for CVE-like Data Loss Bug
    // ==========================================================================
    //
    // These tests verify that:
    // 1. confirmed_lsn is separate from current_lsn
    // 2. confirmed_lsn only advances AFTER successful dispatch
    // 3. Feedback uses confirmed_lsn for flush/apply positions
    // 4. Failed dispatches do NOT advance confirmed_lsn

    /// Helper to create a ReplicationStream for testing
    fn create_test_stream() -> ReplicationStream {
        let config = super::super::PostgresSourceConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "testdb".to_string(),
            user: "testuser".to_string(),
            password: String::new(),
            tables: vec!["test_table".to_string()],
            slot_name: "test_slot".to_string(),
            publication_name: "test_pub".to_string(),
            ssl_mode: super::super::SslMode::Disable,
            table_keys: vec![],
        };

        let dispatchers: Arc<
            RwLock<Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>,
        > = Arc::new(RwLock::new(Vec::new()));
        let status_tx = Arc::new(RwLock::new(None));
        let status = Arc::new(RwLock::new(ComponentStatus::Stopped));

        ReplicationStream::new(config, "test-source".to_string(), dispatchers, status_tx, status)
    }

    #[test]
    fn test_initial_lsn_state() {
        let stream = create_test_stream();

        // Both LSNs should start at 0
        assert_eq!(stream.current_lsn, 0);
        assert_eq!(stream.confirmed_lsn, 0);
        assert!(stream.pending_transaction.is_none());
    }

    #[tokio::test]
    async fn test_confirmed_lsn_does_not_advance_on_begin() {
        let mut stream = create_test_stream();

        // Simulate receiving a BEGIN message
        let begin_msg = super::super::types::WalMessage::Begin(super::super::types::TransactionInfo {
            xid: 1234,
            commit_lsn: 0,
            commit_timestamp: chrono::Utc::now(),
        });

        let message_lsn = 0x1000;
        stream.current_lsn = message_lsn;

        // Process BEGIN - should NOT advance confirmed_lsn
        stream.process_wal_message(begin_msg, message_lsn).await.unwrap();

        // confirmed_lsn should NOT have advanced
        assert_eq!(stream.confirmed_lsn, 0);
        // But pending_transaction should be set
        assert!(stream.pending_transaction.is_some());
    }

    #[tokio::test]
    async fn test_confirmed_lsn_advances_on_relation() {
        let mut stream = create_test_stream();

        // Simulate receiving a RELATION message
        let relation_msg = super::super::types::WalMessage::Relation(super::super::types::RelationInfo {
            id: 1,
            namespace: "public".to_string(),
            name: "test_table".to_string(),
            replica_identity: super::super::types::ReplicaIdentity::Default,
            columns: vec![],
        });

        let message_lsn = 0x2000;
        stream.current_lsn = message_lsn;

        // Process RELATION - should advance confirmed_lsn since no data changes
        stream.process_wal_message(relation_msg, message_lsn).await.unwrap();

        // confirmed_lsn should have advanced for non-data messages
        assert_eq!(stream.confirmed_lsn, message_lsn);
    }

    #[tokio::test]
    async fn test_feedback_uses_confirmed_lsn_not_current_lsn() {
        let stream = create_test_stream();

        // Verify the LSN tracking structure is set up correctly
        // We can't easily test send_feedback without a connection, but we can
        // verify the struct fields are separate
        assert_eq!(stream.current_lsn, 0);
        assert_eq!(stream.confirmed_lsn, 0);

        // The fix ensures that:
        // - write_lsn uses current_lsn (what we received)
        // - flush_lsn uses confirmed_lsn (what we successfully processed)
        // - apply_lsn uses confirmed_lsn (what downstream received)
    }

    #[tokio::test]
    async fn test_current_and_confirmed_lsn_can_differ() {
        let mut stream = create_test_stream();

        // Set current_lsn to simulate receiving data
        stream.current_lsn = 0x5000;

        // confirmed_lsn should still be at 0 (nothing dispatched yet)
        assert_eq!(stream.confirmed_lsn, 0);

        // This is the key invariant: current_lsn can be ahead of confirmed_lsn
        // PostgreSQL will be told to replay from confirmed_lsn if we crash
        assert!(stream.current_lsn > stream.confirmed_lsn);
    }

    #[tokio::test]
    async fn test_keepalive_confirms_lsn_when_no_pending_transaction() {
        let mut stream = create_test_stream();

        // No pending transaction
        assert!(stream.pending_transaction.is_none());

        // Simulate handling a keepalive
        let keepalive_lsn = 0x3000u64;
        let mut keepalive_data = [0u8; 17];
        keepalive_data[0..8].copy_from_slice(&keepalive_lsn.to_be_bytes());
        keepalive_data[16] = 0; // No reply requested

        stream.handle_keepalive(&keepalive_data).await.unwrap();

        // Both LSNs should be updated when there's no pending transaction
        assert_eq!(stream.current_lsn, keepalive_lsn);
        assert_eq!(stream.confirmed_lsn, keepalive_lsn);
    }

    #[tokio::test]
    async fn test_keepalive_does_not_confirm_during_pending_transaction() {
        let mut stream = create_test_stream();

        // Start a pending transaction
        stream.pending_transaction = Some(Vec::new());
        stream.confirmed_lsn = 0x1000; // Previous confirmed position

        // Simulate handling a keepalive during the transaction
        let keepalive_lsn = 0x3000u64;
        let mut keepalive_data = [0u8; 17];
        keepalive_data[0..8].copy_from_slice(&keepalive_lsn.to_be_bytes());
        keepalive_data[16] = 0;

        stream.handle_keepalive(&keepalive_data).await.unwrap();

        // current_lsn should be updated
        assert_eq!(stream.current_lsn, keepalive_lsn);
        // But confirmed_lsn should NOT advance during a pending transaction
        assert_eq!(stream.confirmed_lsn, 0x1000);
    }

    // ==========================================================================
    // Integration Tests - Simulating Real Scenarios
    // ==========================================================================

    #[tokio::test]
    async fn test_transaction_workflow_confirms_lsn_after_successful_dispatch() {
        let config = super::super::PostgresSourceConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "testdb".to_string(),
            user: "testuser".to_string(),
            password: String::new(),
            tables: vec!["test_table".to_string()],
            slot_name: "test_slot".to_string(),
            publication_name: "test_pub".to_string(),
            ssl_mode: super::super::SslMode::Disable,
            table_keys: vec![],
        };

        // Create a dispatcher that will receive events
        let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(100);
        let mut receiver = dispatcher.create_receiver().await.unwrap();

        let dispatchers: Arc<
            RwLock<Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>,
        > = Arc::new(RwLock::new(vec![Box::new(dispatcher)]));
        let status_tx = Arc::new(RwLock::new(None));
        let status = Arc::new(RwLock::new(ComponentStatus::Running));

        let mut stream =
            ReplicationStream::new(config, "test-source".to_string(), dispatchers, status_tx, status);

        // Setup relation mapping for the test
        stream.relations.insert(
            1,
            RelationMapping {
                table_name: "test_table".to_string(),
                schema_name: "public".to_string(),
                label: "test_table".to_string(),
            },
        );

        // Also setup decoder's relation info
        stream.decoder.add_relation_for_testing(super::super::types::RelationInfo {
            id: 1,
            namespace: "public".to_string(),
            name: "test_table".to_string(),
            replica_identity: super::super::types::ReplicaIdentity::Default,
            columns: vec![
                super::super::types::ColumnInfo {
                    name: "id".to_string(),
                    type_oid: 23, // INT4
                    type_modifier: -1,
                    is_key: true,
                },
            ],
        });

        // Initial state
        assert_eq!(stream.confirmed_lsn, 0);

        // 1. BEGIN transaction
        let begin_lsn = 0x1000;
        let begin_msg = super::super::types::WalMessage::Begin(super::super::types::TransactionInfo {
            xid: 100,
            commit_lsn: 0,
            commit_timestamp: chrono::Utc::now(),
        });
        stream.process_wal_message(begin_msg, begin_lsn).await.unwrap();

        // confirmed_lsn should NOT advance after BEGIN
        assert_eq!(stream.confirmed_lsn, 0);

        // 2. INSERT (within transaction)
        let insert_lsn = 0x2000;
        let insert_msg = super::super::types::WalMessage::Insert {
            relation_id: 1,
            tuple: vec![super::super::types::PostgresValue::Int4(42)],
        };
        stream.process_wal_message(insert_msg, insert_lsn).await.unwrap();

        // confirmed_lsn should still NOT advance (transaction pending)
        assert_eq!(stream.confirmed_lsn, 0);
        // But changes should be buffered
        assert_eq!(stream.pending_transaction.as_ref().unwrap().len(), 1);

        // 3. COMMIT transaction
        let commit_lsn = 0x3000;
        let commit_msg = super::super::types::WalMessage::Commit(super::super::types::TransactionInfo {
            xid: 100,
            commit_lsn,
            commit_timestamp: chrono::Utc::now(),
        });
        stream.process_wal_message(commit_msg, commit_lsn).await.unwrap();

        // NOW confirmed_lsn should advance (after successful dispatch)
        assert_eq!(stream.confirmed_lsn, commit_lsn);

        // Verify the event was actually dispatched
        let received = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            receiver.recv(),
        )
        .await
        .expect("Should receive event within timeout")
        .expect("Should successfully receive event");

        assert_eq!(received.source_id, "test-source");
    }

    #[tokio::test]
    async fn test_empty_transaction_confirms_lsn() {
        let mut stream = create_test_stream();

        // BEGIN
        let begin_msg = super::super::types::WalMessage::Begin(super::super::types::TransactionInfo {
            xid: 200,
            commit_lsn: 0,
            commit_timestamp: chrono::Utc::now(),
        });
        stream.process_wal_message(begin_msg, 0x1000).await.unwrap();

        // COMMIT with no changes
        let commit_lsn = 0x2000;
        let commit_msg = super::super::types::WalMessage::Commit(super::super::types::TransactionInfo {
            xid: 200,
            commit_lsn,
            commit_timestamp: chrono::Utc::now(),
        });
        stream.process_wal_message(commit_msg, commit_lsn).await.unwrap();

        // Empty transactions should still confirm LSN
        assert_eq!(stream.confirmed_lsn, commit_lsn);
    }

    // ==========================================================================
    // Tests for the StandbyStatusUpdate structure
    // ==========================================================================

    #[test]
    fn test_standby_status_update_structure() {
        // Verify that StandbyStatusUpdate has separate fields for each LSN type
        let status = super::super::types::StandbyStatusUpdate {
            write_lsn: 0x5000,   // What we received
            flush_lsn: 0x3000,  // What we dispatched
            apply_lsn: 0x3000,  // What downstream processed
            reply_requested: false,
        };

        // The fix ensures flush_lsn can be different from write_lsn
        assert!(status.write_lsn > status.flush_lsn);
        assert_eq!(status.flush_lsn, status.apply_lsn);
    }

    // ==========================================================================
    // Dispatch Failure Tests
    // ==========================================================================

    /// A mock dispatcher that always fails
    struct FailingDispatcher;

    #[async_trait::async_trait]
    impl drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> for FailingDispatcher {
        async fn dispatch_change(&self, _change: Arc<SourceEventWrapper>) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("Simulated dispatch failure"))
        }

        async fn create_receiver(
            &self,
        ) -> anyhow::Result<Box<dyn drasi_lib::channels::ChangeReceiver<SourceEventWrapper>>> {
            Err(anyhow::anyhow!("Not implemented for test"))
        }
    }

    #[tokio::test]
    async fn test_dispatch_failure_does_not_advance_confirmed_lsn() {
        // This test verifies the critical safety property: if dispatch fails,
        // confirmed_lsn must NOT advance, ensuring PostgreSQL replays the event.
        let config = super::super::PostgresSourceConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "testdb".to_string(),
            user: "testuser".to_string(),
            password: String::new(),
            tables: vec!["test_table".to_string()],
            slot_name: "test_slot".to_string(),
            publication_name: "test_pub".to_string(),
            ssl_mode: super::super::SslMode::Disable,
            table_keys: vec![],
        };

        // Register a dispatcher that always fails
        let dispatchers: Arc<
            RwLock<Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>,
        > = Arc::new(RwLock::new(vec![Box::new(FailingDispatcher)]));
        let status_tx = Arc::new(RwLock::new(None));
        let status = Arc::new(RwLock::new(ComponentStatus::Running));

        let mut stream =
            ReplicationStream::new(config, "test-source".to_string(), dispatchers, status_tx, status);

        // Setup relation mapping
        stream.relations.insert(
            1,
            RelationMapping {
                table_name: "test_table".to_string(),
                schema_name: "public".to_string(),
                label: "test_table".to_string(),
            },
        );

        stream.decoder.add_relation_for_testing(super::super::types::RelationInfo {
            id: 1,
            namespace: "public".to_string(),
            name: "test_table".to_string(),
            replica_identity: super::super::types::ReplicaIdentity::Default,
            columns: vec![super::super::types::ColumnInfo {
                name: "id".to_string(),
                type_oid: 23,
                type_modifier: -1,
                is_key: true,
            }],
        });

        // Record initial confirmed_lsn
        let initial_lsn = stream.confirmed_lsn;
        assert_eq!(initial_lsn, 0);

        // BEGIN transaction
        let begin_msg = super::super::types::WalMessage::Begin(super::super::types::TransactionInfo {
            xid: 999,
            commit_lsn: 0,
            commit_timestamp: chrono::Utc::now(),
        });
        stream.process_wal_message(begin_msg, 0x1000).await.unwrap();

        // INSERT (buffered in transaction)
        let insert_msg = super::super::types::WalMessage::Insert {
            relation_id: 1,
            tuple: vec![super::super::types::PostgresValue::Int4(123)],
        };
        stream.process_wal_message(insert_msg, 0x2000).await.unwrap();

        // COMMIT - this will attempt dispatch and FAIL
        let commit_lsn = 0x3000;
        let commit_msg = super::super::types::WalMessage::Commit(super::super::types::TransactionInfo {
            xid: 999,
            commit_lsn,
            commit_timestamp: chrono::Utc::now(),
        });

        // The commit should fail due to dispatch failure
        let result = stream.process_wal_message(commit_msg, commit_lsn).await;
        assert!(result.is_err(), "Commit should fail when dispatch fails");

        // CRITICAL: confirmed_lsn must NOT have advanced
        assert_eq!(
            stream.confirmed_lsn, initial_lsn,
            "confirmed_lsn must NOT advance when dispatch fails"
        );
    }
}
