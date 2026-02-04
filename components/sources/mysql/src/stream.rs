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

//! Binlog replication stream for MySQL sources.

use std::time::Duration;

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use mysql_cdc::binlog_client::BinlogClient;
use mysql_cdc::binlog_options::BinlogOptions;
use mysql_cdc::events::binlog_event::BinlogEvent;
use mysql_cdc::events::event_header::EventHeader;
use mysql_cdc::replica_options::ReplicaOptions;
use mysql_cdc::ssl_mode::SslMode as MySqlCdcSslMode;

use drasi_core::models::SourceChange;
use drasi_lib::channels::{
    ComponentEvent, ComponentStatus, ComponentType, SourceEvent, SourceEventWrapper,
};
use drasi_lib::sources::base::SourceBase;

use crate::config::{MySqlSourceConfig, SslMode, StartPosition};
use crate::decoder::MySqlDecoder;
use crate::types::{ReplicationState, TableMapping};

const STATE_KEY: &str = "replication_position";

pub struct ReplicationStream {
    config: MySqlSourceConfig,
    source_id: String,
    base: SourceBase,
    decoder: MySqlDecoder,
    table_mapping: TableMapping,
    pending_changes: Option<Vec<SourceChange>>,
    current_binlog_file: String,
    current_binlog_position: u32,
    current_gtid: Option<String>,
}

impl ReplicationStream {
    pub fn new(config: MySqlSourceConfig, source_id: String, base: SourceBase) -> Self {
        let decoder = MySqlDecoder::new(source_id.clone(), &config.table_keys);
        Self {
            config,
            source_id,
            base,
            decoder,
            table_mapping: TableMapping::default(),
            pending_changes: None,
            current_binlog_file: String::new(),
            current_binlog_position: 0,
            current_gtid: None,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting MySQL replication for source {}", self.source_id);

        let state_store = self.base.state_store().await;
        let start_position = self.load_start_position(state_store.as_deref()).await?;

        let options = self.build_replica_options(start_position)?;

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1000);
        let source_id = self.source_id.clone();
        let status_tx = self.base.status_tx();
        let status = self.base.status.clone();

        tokio::task::spawn_blocking(move || {
            let mut client = BinlogClient::new(options);
            let events = match client.replicate() {
                Ok(events) => events,
                Err(e) => {
                    if let Ok(mut status) = status.try_write() {
                        *status = ComponentStatus::Error;
                    }
                    if let Ok(status_guard) = status_tx.try_read() {
                        if let Some(tx) = status_guard.as_ref() {
                            let _ = tx.try_send(ComponentEvent {
                                component_id: source_id.clone(),
                                component_type: ComponentType::Source,
                                status: ComponentStatus::Error,
                                timestamp: chrono::Utc::now(),
                                message: Some(format!("Replication failed: {e:?}")),
                            });
                        }
                    }
                    return;
                }
            };

            for result in events {
                match result {
                    Ok((header, event)) => {
                        client.commit(&header, &event);
                        if event_tx.blocking_send((header, event)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Error reading binlog event: {e:?}");
                        break;
                    }
                }
            }
        });

        while let Some((header, event)) = event_rx.recv().await {
            if let Err(e) = self.process_event(&header, &event).await {
                error!("Error processing binlog event: {e:?}");
            }

            if let Some(state_store) = state_store.as_deref() {
                if let Err(e) = self.persist_state(state_store, &header).await {
                    warn!("Failed to persist replication state: {e:?}");
                }
            }
        }

        Ok(())
    }

    async fn process_event(&mut self, header: &EventHeader, event: &BinlogEvent) -> Result<()> {
        match event {
            BinlogEvent::TableMapEvent(table_event) => {
                self.table_mapping.update(table_event.clone());
            }
            BinlogEvent::RotateEvent(rotate_event) => {
                self.current_binlog_file = rotate_event.binlog_filename.clone();
                self.current_binlog_position = rotate_event.binlog_position as u32;
            }
            BinlogEvent::MySqlGtidEvent(gtid_event) => {
                self.current_gtid = Some(gtid_event.gtid.to_string());
            }
            BinlogEvent::WriteRowsEvent(write_event) => {
                let table = self
                    .table_mapping
                    .get(write_event.table_id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(
                            "Missing TableMapEvent for table_id {}",
                            write_event.table_id
                        )
                    })?;
                if !self.should_process_table(&table) {
                    return Ok(());
                }
                self.ensure_transaction_buffer();
                for row in &write_event.rows {
                    let change = self.decoder.decode_insert(&table, row)?;
                    self.push_change(change).await?;
                }
            }
            BinlogEvent::UpdateRowsEvent(update_event) => {
                let table = self
                    .table_mapping
                    .get(update_event.table_id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(
                            "Missing TableMapEvent for table_id {}",
                            update_event.table_id
                        )
                    })?;
                if !self.should_process_table(&table) {
                    return Ok(());
                }
                self.ensure_transaction_buffer();
                for row in &update_event.rows {
                    let change = self.decoder.decode_update(&table, row)?;
                    self.push_change(change).await?;
                }
            }
            BinlogEvent::DeleteRowsEvent(delete_event) => {
                let table = self
                    .table_mapping
                    .get(delete_event.table_id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(
                            "Missing TableMapEvent for table_id {}",
                            delete_event.table_id
                        )
                    })?;
                if !self.should_process_table(&table) {
                    return Ok(());
                }
                self.ensure_transaction_buffer();
                for row in &delete_event.rows {
                    let change = self.decoder.decode_delete(&table, row)?;
                    self.push_change(change).await?;
                }
            }
            BinlogEvent::XidEvent(_) => {
                self.flush_transaction().await?;
            }
            BinlogEvent::QueryEvent(query_event) => {
                if query_event.sql_statement.eq_ignore_ascii_case("COMMIT")
                    || query_event.sql_statement.eq_ignore_ascii_case("ROLLBACK")
                {
                    self.flush_transaction().await?;
                }
            }
            _ => {
                debug!("Ignoring binlog event type: {:?}", event);
            }
        }

        debug!(
            "Processed event: type={} position={}",
            header.event_type, header.next_event_position
        );

        Ok(())
    }

    async fn push_change(&mut self, change: SourceChange) -> Result<()> {
        if let Some(buffer) = self.pending_changes.as_mut() {
            buffer.push(change);
            return Ok(());
        }

        let wrapper = SourceEventWrapper::new(
            self.source_id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );

        SourceBase::dispatch_from_task(self.base.dispatchers.clone(), wrapper, &self.source_id)
            .await
    }

    async fn flush_transaction(&mut self) -> Result<()> {
        if let Some(changes) = self.pending_changes.take() {
            for change in changes {
                let wrapper = SourceEventWrapper::new(
                    self.source_id.clone(),
                    SourceEvent::Change(change),
                    chrono::Utc::now(),
                );
                SourceBase::dispatch_from_task(
                    self.base.dispatchers.clone(),
                    wrapper,
                    &self.source_id,
                )
                .await?;
            }
        }
        Ok(())
    }

    fn ensure_transaction_buffer(&mut self) {
        if self.pending_changes.is_none() {
            self.pending_changes = Some(Vec::new());
        }
    }

    fn should_process_table(
        &self,
        table: &mysql_cdc::events::table_map_event::TableMapEvent,
    ) -> bool {
        if self.config.tables.is_empty() {
            return true;
        }
        let table_name = if table.database_name.is_empty() {
            table.table_name.clone()
        } else {
            format!("{}.{}", table.database_name, table.table_name)
        };
        self.config.tables.contains(&table_name) || self.config.tables.contains(&table.table_name)
    }

    async fn load_start_position(
        &self,
        state_store: Option<&dyn drasi_lib::state_store::StateStoreProvider>,
    ) -> Result<StartPosition> {
        if let Some(store) = state_store {
            if let Some(bytes) = store.get(&self.source_id, STATE_KEY).await? {
                if let Ok(state) = serde_json::from_slice::<ReplicationState>(&bytes) {
                    if let Some(gtid) = state.gtid_set {
                        return Ok(StartPosition::FromGtid(gtid));
                    }
                    if !state.binlog_file.is_empty() {
                        return Ok(StartPosition::FromPosition {
                            file: state.binlog_file,
                            position: state.binlog_position,
                        });
                    }
                }
            }
        }

        Ok(self.config.start_position.clone())
    }

    fn build_replica_options(&self, start_position: StartPosition) -> Result<ReplicaOptions> {
        let mut options = ReplicaOptions::default();
        options.hostname = self.config.host.clone();
        options.port = self.config.port;
        options.username = self.config.user.clone();
        options.password = self.config.password.clone();
        options.database = Some(self.config.database.clone());
        options.server_id = self.config.server_id;
        options.blocking = true;
        options.heartbeat_interval = Duration::from_secs(self.config.heartbeat_interval_seconds);

        options.ssl_mode = match self.config.ssl_mode {
            SslMode::Disabled => MySqlCdcSslMode::Disabled,
            SslMode::IfAvailable => MySqlCdcSslMode::IfAvailable,
            SslMode::Require => MySqlCdcSslMode::Require,
            SslMode::RequireVerifyCa => MySqlCdcSslMode::RequireVerifyCa,
            SslMode::RequireVerifyFull => MySqlCdcSslMode::RequireVerifyFull,
        };

        options.binlog = match start_position {
            StartPosition::FromStart => BinlogOptions::from_start(),
            StartPosition::FromEnd => BinlogOptions::from_end(),
            StartPosition::FromPosition { file, position } => {
                BinlogOptions::from_position(file, position)
            }
            StartPosition::FromGtid(gtid) => {
                let gtid_set = mysql_cdc::providers::mysql::gtid::gtid_set::GtidSet::parse(&gtid)
                    .map_err(|e| anyhow!("Invalid GTID set: {e:?}"))?;
                BinlogOptions::from_mysql_gtid(gtid_set)
            }
        };

        if options.ssl_mode != MySqlCdcSslMode::Disabled {
            return Err(anyhow!(
                "mysql_cdc does not support SSL at runtime; use ssl_mode=disabled"
            ));
        }

        Ok(options)
    }

    async fn persist_state(
        &self,
        store: &dyn drasi_lib::state_store::StateStoreProvider,
        header: &EventHeader,
    ) -> Result<()> {
        let state = ReplicationState {
            binlog_file: self.current_binlog_file.clone(),
            binlog_position: if self.current_binlog_position == 0 {
                header.next_event_position
            } else {
                self.current_binlog_position
            },
            gtid_set: self.current_gtid.clone(),
            last_processed_timestamp: header.timestamp as u64,
        };

        let bytes = serde_json::to_vec(&state)?;
        store.set(&self.source_id, STATE_KEY, bytes).await?;
        Ok(())
    }
}
