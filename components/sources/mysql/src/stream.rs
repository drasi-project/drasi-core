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

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc as StdArc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use log::{debug, error, info, warn};
use mysql_async::prelude::{Query, Queryable};
use mysql_async::{BinlogStream, BinlogStreamRequest, Conn, OptsBuilder, Row, SslOpts};
use mysql_common::binlog::events::{
    BinlogEventHeader, Event, EventData, RowsEventData, TableMapEvent,
};
use mysql_common::packets::Sid;
use mysql_common::uuid::Uuid;

use drasi_core::models::SourceChange;
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_lib::sources::base::SourceBase;

use crate::config::{MySqlSourceConfig, SslMode, StartPosition};
use crate::decoder::MySqlDecoder;
use crate::types::ReplicationState;

const STATE_KEY: &str = "replication_position";

pub struct ReplicationStream {
    config: MySqlSourceConfig,
    source_id: String,
    base: SourceBase,
    decoder: MySqlDecoder,
    pending_changes: Option<Vec<SourceChange>>,
    current_binlog_file: String,
    current_binlog_position: u32,
    current_gtid: Option<String>,
    shutdown: StdArc<AtomicBool>,
}

const MAX_RECONNECT_ATTEMPTS: u32 = 10;
const INITIAL_RECONNECT_DELAY_SECS: u64 = 5;
const MAX_RECONNECT_DELAY_SECS: u64 = 60;

struct ResolvedStartPosition {
    filename: String,
    request_position: u64,
    state_position: u32,
    use_gtid: bool,
    gtid_set: Vec<Sid<'static>>,
}

impl ReplicationStream {
    pub fn new(
        config: MySqlSourceConfig,
        source_id: String,
        base: SourceBase,
        shutdown: StdArc<AtomicBool>,
    ) -> Self {
        let decoder = MySqlDecoder::new(source_id.clone(), &config.table_keys);
        Self {
            config,
            source_id,
            base,
            decoder,
            pending_changes: None,
            current_binlog_file: String::new(),
            current_binlog_position: 0,
            current_gtid: None,
            shutdown,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting MySQL replication for source {}", self.source_id);

        if self.config.tables.is_empty() {
            warn!(
                "Source '{}': no tables configured — ALL tables in the database will be replicated. \
                 Configure the 'tables' field to restrict which tables are monitored.",
                self.source_id
            );
        }

        let state_store = self.base.state_store().await;
        let mut attempts = 0u32;

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested for source {}", self.source_id);
                return Ok(());
            }

            let start_position = self.load_start_position(state_store.as_deref()).await?;

            match self
                .run_replication_loop(start_position, state_store.as_deref())
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if self.shutdown.load(Ordering::Relaxed) {
                        info!("Shutdown during replication for source {}", self.source_id);
                        return Ok(());
                    }

                    attempts += 1;
                    if attempts > MAX_RECONNECT_ATTEMPTS {
                        error!(
                            "Replication failed after {MAX_RECONNECT_ATTEMPTS} reconnect attempts \
                             for source {}: {e}",
                            self.source_id
                        );
                        return Err(anyhow!(
                            "Replication failed after {MAX_RECONNECT_ATTEMPTS} attempts: {e}"
                        ));
                    }

                    let delay = std::cmp::min(
                        INITIAL_RECONNECT_DELAY_SECS * 2u64.saturating_pow(attempts - 1),
                        MAX_RECONNECT_DELAY_SECS,
                    );
                    warn!(
                        "Replication connection lost for source {} (attempt {attempts}/{MAX_RECONNECT_ATTEMPTS}), \
                         reconnecting in {delay}s: {e}",
                        self.source_id
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }

    async fn run_replication_loop(
        &mut self,
        start_position: StartPosition,
        state_store: Option<&dyn drasi_lib::state_store::StateStoreProvider>,
    ) -> Result<()> {
        self.pending_changes = None;
        self.current_gtid = match &start_position {
            StartPosition::FromGtid(gtid) => Some(gtid.clone()),
            _ => None,
        };

        let mut stream = self.connect_binlog_stream(&start_position).await?;

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested for source {}", self.source_id);
                Self::close_stream(stream).await;
                return Ok(());
            }

            match stream.next().await {
                Some(Ok(event)) => {
                    self.process_event(&stream, &event, state_store).await?;
                }
                Some(Err(err)) => {
                    Self::close_stream(stream).await;
                    return Err(anyhow!("Error reading binlog event: {err}"));
                }
                None => {
                    Self::close_stream(stream).await;
                    if self.shutdown.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    return Err(anyhow!("Binlog replication stream ended unexpectedly"));
                }
            }
        }
    }

    async fn connect_binlog_stream(
        &mut self,
        start_position: &StartPosition,
    ) -> Result<BinlogStream> {
        match self.config.ssl_mode {
            SslMode::IfAvailable => match self
                .connect_binlog_stream_with_ssl(start_position, Some(self.relaxed_ssl_opts()))
                .await
            {
                Ok(stream) => Ok(stream),
                Err(ssl_error) => {
                    warn!(
                        "SSL connection attempt failed for source {}, retrying without SSL: {ssl_error}",
                        self.source_id
                    );
                    self.connect_binlog_stream_with_ssl(start_position, None)
                        .await
                        .context("Failed to connect without SSL after SSL fallback")
                }
            },
            SslMode::Disabled => {
                self.connect_binlog_stream_with_ssl(start_position, None)
                    .await
            }
            SslMode::Require => {
                self.connect_binlog_stream_with_ssl(start_position, Some(self.relaxed_ssl_opts()))
                    .await
            }
            SslMode::RequireVerifyCa => {
                self.connect_binlog_stream_with_ssl(start_position, Some(self.verify_ca_ssl_opts()))
                    .await
            }
            SslMode::RequireVerifyFull => {
                self.connect_binlog_stream_with_ssl(start_position, Some(SslOpts::default()))
                    .await
            }
        }
    }

    async fn connect_binlog_stream_with_ssl(
        &mut self,
        start_position: &StartPosition,
        ssl_opts: Option<SslOpts>,
    ) -> Result<BinlogStream> {
        let opts = OptsBuilder::default()
            .ip_or_hostname(&self.config.host)
            .tcp_port(self.config.port)
            .user(Some(&self.config.user))
            .pass(Some(&self.config.password))
            .db_name(Some(&self.config.database))
            .prefer_socket(Some(false))
            .ssl_opts(ssl_opts);

        let mut conn = Conn::new(opts).await?;
        self.configure_heartbeat(&mut conn).await?;

        let resolved = self
            .resolve_start_position(&mut conn, start_position)
            .await?;
        self.current_binlog_file = resolved.filename.clone();
        self.current_binlog_position = resolved.state_position;

        let mut request = BinlogStreamRequest::new(self.config.server_id)
            .with_hostname(self.config.host.as_bytes())
            .with_user(self.config.user.as_bytes())
            .with_password(self.config.password.as_bytes())
            .with_port(self.config.port)
            .with_pos(resolved.request_position);

        if !resolved.filename.is_empty() {
            request = request.with_filename(resolved.filename.as_bytes());
        }

        if resolved.use_gtid {
            request = request.with_gtid().with_gtid_set(resolved.gtid_set);
        }

        conn.get_binlog_stream(request).await.map_err(Into::into)
    }

    async fn configure_heartbeat(&self, conn: &mut Conn) -> Result<()> {
        let nanoseconds = u128::from(self.config.heartbeat_interval_seconds) * 1_000_000_000;
        conn.query_drop(format!("SET @master_heartbeat_period={nanoseconds}"))
            .await?;
        Ok(())
    }

    async fn resolve_start_position(
        &self,
        conn: &mut Conn,
        start_position: &StartPosition,
    ) -> Result<ResolvedStartPosition> {
        match start_position {
            StartPosition::FromStart => {
                let row: Row = "SHOW BINARY LOGS"
                    .first(conn)
                    .await?
                    .ok_or_else(|| anyhow!("SHOW BINARY LOGS returned no rows"))?;
                let filename = row
                    .get::<String, _>(0)
                    .context("SHOW BINARY LOGS did not return a filename")?;
                Ok(ResolvedStartPosition {
                    filename,
                    request_position: 4,
                    state_position: 4,
                    use_gtid: false,
                    gtid_set: Vec::new(),
                })
            }
            StartPosition::FromEnd => {
                let row: Row = "SHOW MASTER STATUS"
                    .first(conn)
                    .await?
                    .ok_or_else(|| anyhow!("SHOW MASTER STATUS returned no rows"))?;
                let filename = row
                    .get::<String, _>(0)
                    .context("SHOW MASTER STATUS did not return a filename")?;
                let position = row
                    .get::<u64, _>(1)
                    .context("SHOW MASTER STATUS did not return a binlog position")?;
                Ok(ResolvedStartPosition {
                    filename,
                    request_position: position,
                    state_position: u32::try_from(position)
                        .context("Binlog position exceeds supported range")?,
                    use_gtid: false,
                    gtid_set: Vec::new(),
                })
            }
            StartPosition::FromPosition { file, position } => Ok(ResolvedStartPosition {
                filename: file.clone(),
                request_position: u64::from(*position),
                state_position: *position,
                use_gtid: false,
                gtid_set: Vec::new(),
            }),
            StartPosition::FromGtid(gtid) => Ok(ResolvedStartPosition {
                filename: String::new(),
                request_position: 4,
                state_position: 4,
                use_gtid: true,
                gtid_set: parse_gtid_set(gtid)?,
            }),
        }
    }

    async fn process_event(
        &mut self,
        stream: &BinlogStream,
        event: &Event,
        state_store: Option<&dyn drasi_lib::state_store::StateStoreProvider>,
    ) -> Result<()> {
        let header = event.header();

        match event.read_data()? {
            Some(EventData::TableMapEvent(_)) => {}
            Some(EventData::RotateEvent(rotate_event)) => {
                self.current_binlog_file = rotate_event.name().into_owned();
                self.current_binlog_position = rotate_event.position() as u32;
            }
            Some(EventData::GtidEvent(gtid_event)) => {
                let sid = Uuid::from_bytes(gtid_event.sid());
                self.current_gtid = Some(format!("{sid}:{}", gtid_event.gno()));
            }
            Some(EventData::RowsEvent(rows_event)) => {
                self.process_rows_event(stream, rows_event).await?;
            }
            Some(EventData::XidEvent(_)) => {
                self.flush_transaction(state_store, &header).await?;
            }
            Some(EventData::QueryEvent(query_event)) => {
                let query = query_event.query();
                if query.eq_ignore_ascii_case("COMMIT") || query.eq_ignore_ascii_case("ROLLBACK") {
                    self.flush_transaction(state_store, &header).await?;
                }
            }
            Some(other) => {
                debug!("Ignoring binlog event: {other:?}");
            }
            None => {
                debug!(
                    "Ignoring unknown binlog event type {}",
                    header.event_type_raw()
                );
            }
        }

        debug!(
            "Processed event: type={} position={}",
            header.event_type_raw(),
            header.log_pos()
        );

        Ok(())
    }

    async fn process_rows_event(
        &mut self,
        stream: &BinlogStream,
        rows_event: RowsEventData<'_>,
    ) -> Result<()> {
        let table = stream.get_tme(rows_event.table_id()).ok_or_else(|| {
            anyhow!(
                "Missing TableMapEvent for table_id {}",
                rows_event.table_id()
            )
        })?;

        if !self.should_process_table(table) {
            return Ok(());
        }

        self.ensure_transaction_buffer();

        match &rows_event {
            RowsEventData::WriteRowsEvent(_) | RowsEventData::WriteRowsEventV1(_) => {
                for row in rows_event.rows(table) {
                    let (_, after) = row.context("Failed to decode write rows event")?;
                    let after =
                        after.ok_or_else(|| anyhow!("Write rows event missing after image"))?;
                    let change = self.decoder.decode_insert(table, &after)?;
                    self.push_change(change).await?;
                }
            }
            RowsEventData::UpdateRowsEvent(_)
            | RowsEventData::UpdateRowsEventV1(_)
            | RowsEventData::PartialUpdateRowsEvent(_) => {
                for row in rows_event.rows(table) {
                    let (before, after) = row.context("Failed to decode update rows event")?;
                    let before =
                        before.ok_or_else(|| anyhow!("Update rows event missing before image"))?;
                    let after =
                        after.ok_or_else(|| anyhow!("Update rows event missing after image"))?;
                    let change = self.decoder.decode_update(table, &before, &after)?;
                    self.push_change(change).await?;
                }
            }
            RowsEventData::DeleteRowsEvent(_) | RowsEventData::DeleteRowsEventV1(_) => {
                for row in rows_event.rows(table) {
                    let (before, _) = row.context("Failed to decode delete rows event")?;
                    let before =
                        before.ok_or_else(|| anyhow!("Delete rows event missing before image"))?;
                    let change = self.decoder.decode_delete(table, &before)?;
                    self.push_change(change).await?;
                }
            }
        }

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

    async fn flush_transaction(
        &mut self,
        state_store: Option<&dyn drasi_lib::state_store::StateStoreProvider>,
        header: &BinlogEventHeader,
    ) -> Result<()> {
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

        if let Some(store) = state_store {
            if let Err(e) = self.persist_state(store, header).await {
                warn!("Failed to persist replication state: {e:?}");
            }
        }

        Ok(())
    }

    fn ensure_transaction_buffer(&mut self) {
        if self.pending_changes.is_none() {
            self.pending_changes = Some(Vec::new());
        }
    }

    fn should_process_table(&self, table: &TableMapEvent<'_>) -> bool {
        if self.config.tables.is_empty() {
            return true;
        }

        let database_name = table.database_name().into_owned();
        let table_name = table.table_name().into_owned();
        let qualified_name = if database_name.is_empty() {
            table_name.clone()
        } else {
            format!("{database_name}.{table_name}")
        };

        self.config.tables.contains(&qualified_name) || self.config.tables.contains(&table_name)
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

    async fn persist_state(
        &self,
        store: &dyn drasi_lib::state_store::StateStoreProvider,
        header: &BinlogEventHeader,
    ) -> Result<()> {
        let state = ReplicationState {
            binlog_file: self.current_binlog_file.clone(),
            binlog_position: match header.log_pos() {
                0 => self.current_binlog_position,
                position => position,
            },
            gtid_set: self.current_gtid.clone(),
            last_processed_timestamp: header.timestamp() as u64,
        };

        let bytes = serde_json::to_vec(&state)?;
        store.set(&self.source_id, STATE_KEY, bytes).await?;
        Ok(())
    }

    fn relaxed_ssl_opts(&self) -> SslOpts {
        SslOpts::default()
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true)
    }

    fn verify_ca_ssl_opts(&self) -> SslOpts {
        SslOpts::default().with_danger_skip_domain_validation(true)
    }

    async fn close_stream(stream: BinlogStream) {
        if let Err(err) = stream.close().await {
            debug!("Failed to close MySQL binlog stream cleanly: {err}");
        }
    }
}

fn parse_gtid_set(gtid: &str) -> Result<Vec<Sid<'static>>> {
    let mut sids = Vec::new();
    for part in gtid
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let sid: Sid<'static> =
            Sid::from_str(part).map_err(|e| anyhow!("Invalid GTID set entry '{part}': {e}"))?;
        sids.push(sid);
    }

    if sids.is_empty() {
        anyhow::bail!("Invalid GTID set: no GTID intervals were provided");
    }

    Ok(sids)
}
