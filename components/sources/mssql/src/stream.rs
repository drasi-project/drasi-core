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

//! CDC polling stream implementation

use crate::config::{validate_sql_identifier, MsSqlSourceConfig, StartPosition};
use crate::connection::MsSqlConnection;
use crate::decoder::{cdc_columns, CdcOperation};
use crate::error::{ConnectionError, MsSqlError, MsSqlErrorKind};
use crate::keys::PrimaryKeyCache;
use crate::lsn::Lsn;
use crate::types::extract_properties_from_cdc_row;
use anyhow::{anyhow, Result};
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_core::position::SequencePosition;
use drasi_lib::channels::{ChangeDispatcher, SourceEvent, SourceEventWrapper};
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::sync::RwLock;
use tokio::time::sleep;

/// Reconnection configuration constants
const INITIAL_RECONNECT_DELAY_MS: u64 = 1000; // 1 second
const MAX_RECONNECT_DELAY_MS: u64 = 60000; // 60 seconds
const RECONNECT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Classify an error to determine how to handle it
fn classify_error(error: &anyhow::Error) -> MsSqlErrorKind {
    MsSqlError::classify(error).unwrap_or(MsSqlErrorKind::Other)
}

/// Construct a 20-byte CDC position from start_lsn and seqval.
///
/// The position is `start_lsn (10 bytes) || seqval (10 bytes)`, which is
/// lexicographically ordered (matching CDC's natural ordering) and uniquely
/// identifies a single CDC row.
pub fn cdc_position(start_lsn: &Lsn, seqval: &Lsn) -> SequencePosition {
    let mut bytes = [0u8; 20];
    bytes[..10].copy_from_slice(start_lsn.as_bytes());
    bytes[10..20].copy_from_slice(seqval.as_bytes());
    SequencePosition::from_bytes(&bytes)
}

/// Extract the start_lsn from a 20-byte CDC position.
///
/// Returns the first 10 bytes as an `Lsn`, suitable for seeking into the CDC
/// stream with `fn_cdc_get_all_changes_*`.
pub fn position_to_seek_lsn(pos: &SequencePosition) -> Result<Lsn> {
    let bytes = pos.as_bytes();
    if bytes.len() != 20 {
        return Err(anyhow!(
            "Expected 20-byte MSSQL CDC position, got {} bytes",
            bytes.len()
        ));
    }
    Lsn::from_bytes(&bytes[..10])
}

/// Run the CDC polling loop with automatic reconnection
///
/// This continuously polls MS SQL CDC for changes and dispatches them to subscribers.
/// If the connection is lost, it will automatically attempt to reconnect with
/// exponential backoff. The loop can be gracefully stopped using the shutdown receiver.
pub async fn run_cdc_stream(
    source_id: String,
    config: MsSqlSourceConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: Option<Arc<dyn drasi_lib::state_store::StateStoreProvider>>,
    mut shutdown_rx: watch::Receiver<bool>,
    resume_from: Option<SequencePosition>,
) -> Result<()> {
    info!("Starting CDC stream for source '{source_id}'");

    let mut reconnect_delay = Duration::from_millis(INITIAL_RECONNECT_DELAY_MS);

    // If resume_from is provided, derive the starting LSN from the position.
    // This overrides the checkpoint and start_position config.
    let resume_lsn = match resume_from {
        Some(pos) => {
            let lsn = position_to_seek_lsn(&pos)?;
            info!("Resuming from position, seek LSN: {}", lsn.to_hex());
            Some((lsn, pos))
        }
        None => None,
    };

    // Outer reconnection loop
    loop {
        // Check for shutdown before attempting connection
        if *shutdown_rx.borrow() {
            info!("CDC stream for source '{source_id}' shutdown before connection");
            return Ok(());
        }

        match run_cdc_polling_loop(
            &source_id,
            &config,
            &dispatchers,
            &state_store,
            &mut shutdown_rx,
            &resume_lsn,
        )
        .await
        {
            Ok(()) => {
                // Normal exit (loop was shutdown or exited gracefully)
                info!("CDC polling loop exited normally for source '{source_id}'");
                return Ok(());
            }
            Err(e) => {
                // Check if we were shutdown
                if *shutdown_rx.borrow() {
                    info!("CDC stream for source '{source_id}' shutdown");
                    return Ok(());
                }

                match classify_error(&e) {
                    MsSqlErrorKind::Connection => {
                        error!(
                            "Connection error in CDC stream for source '{source_id}': {e}. \
                             Reconnecting in {reconnect_delay:?}..."
                        );

                        // Wait before reconnecting, but check for shutdown
                        tokio::select! {
                            _ = sleep(reconnect_delay) => {}
                            _ = shutdown_rx.changed() => {
                                info!("CDC stream for source '{source_id}' shutdown during reconnect wait");
                                return Ok(());
                            }
                        }

                        // Increase delay for next attempt (exponential backoff)
                        reconnect_delay = Duration::from_millis(
                            ((reconnect_delay.as_millis() as f64) * RECONNECT_BACKOFF_MULTIPLIER)
                                .min(MAX_RECONNECT_DELAY_MS as f64)
                                as u64,
                        );
                    }
                    MsSqlErrorKind::RecoverableLsn | MsSqlErrorKind::Other => {
                        // Non-connection error - log and continue with normal delay
                        error!("Error in CDC stream for source '{source_id}': {e}");

                        // Reset backoff delay on non-connection errors
                        reconnect_delay = Duration::from_millis(INITIAL_RECONNECT_DELAY_MS);

                        // Brief pause before retry, but check for shutdown
                        tokio::select! {
                            _ = sleep(Duration::from_secs(1)) => {}
                            _ = shutdown_rx.changed() => {
                                info!("CDC stream for source '{source_id}' shutdown during error recovery");
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Internal CDC polling loop that handles a single connection session
async fn run_cdc_polling_loop(
    source_id: &str,
    config: &MsSqlSourceConfig,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: &Option<Arc<dyn drasi_lib::state_store::StateStoreProvider>>,
    shutdown_rx: &mut watch::Receiver<bool>,
    resume_lsn: &Option<(Lsn, SequencePosition)>,
) -> Result<()> {
    // Connect to MS SQL
    info!("Connecting to MS SQL Server for source '{source_id}'");
    let mut connection = MsSqlConnection::connect(config).await?;
    let client = connection.client_mut();

    // Discover primary keys
    let mut pk_cache = PrimaryKeyCache::new();
    pk_cache.discover_keys(client, config).await?;
    info!("Discovered primary keys for {} tables", config.tables.len());

    // Determine starting LSN.
    // Priority: resume_from > checkpoint > start_position config
    let mut current_lsn = if let Some((lsn, _pos)) = resume_lsn {
        info!("Using resume LSN: {}", lsn.to_hex());
        Some(*lsn)
    } else {
        load_checkpoint(source_id, state_store).await?
    };

    // Track the resume position for filtering during the catch-up batch.
    // Events with position ≤ resume_from are skipped to avoid reprocessing.
    let mut skip_threshold: Option<SequencePosition> = resume_lsn.as_ref().map(|(_lsn, pos)| *pos);

    info!(
        "Starting CDC from LSN: {}",
        current_lsn
            .as_ref()
            .map(|l| l.to_hex())
            .unwrap_or_else(|| "NONE (will use current)".to_string())
    );

    // Track consecutive errors for connection health monitoring
    let mut consecutive_errors = 0u32;
    const MAX_CONSECUTIVE_ERRORS: u32 = 5;

    // Main polling loop
    let poll_interval = Duration::from_millis(config.poll_interval_ms);

    loop {
        // Check for shutdown at the start of each iteration
        if *shutdown_rx.borrow() {
            info!("CDC polling loop for source '{source_id}' received shutdown signal");
            return Ok(());
        }

        let lsn_before = current_lsn;

        match poll_cdc_changes(
            source_id,
            config,
            client,
            &pk_cache,
            &mut current_lsn,
            dispatchers,
            &mut skip_threshold,
        )
        .await
        {
            Ok(change_count) => {
                // Reset error counter on success
                consecutive_errors = 0;

                // Save checkpoint if LSN was initialized or if we processed changes
                let lsn_changed = current_lsn != lsn_before;
                if change_count > 0 || lsn_changed {
                    if change_count > 0 {
                        debug!("Processed {change_count} CDC changes");
                    }
                    if lsn_changed && change_count == 0 {
                        debug!("Initialized LSN checkpoint");
                    }

                    if let Some(ref lsn) = current_lsn {
                        save_checkpoint(source_id, lsn, state_store).await?;
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;

                // Classify the error using typed error handling
                match classify_error(&e) {
                    MsSqlErrorKind::Connection => {
                        error!("Connection error during CDC polling: {e}");
                        return Err(e);
                    }
                    MsSqlErrorKind::RecoverableLsn => {
                        warn!("LSN error detected, clearing checkpoint and restarting from current position");
                        clear_checkpoint(source_id, state_store).await?;
                        current_lsn = None;
                        consecutive_errors = 0; // Reset since we handled this
                    }
                    MsSqlErrorKind::Other => {
                        error!("Error polling CDC changes: {e}");

                        // If we've had too many consecutive errors, assume connection is bad
                        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                            error!(
                                "Too many consecutive errors ({consecutive_errors}), \
                                 assuming connection is unhealthy"
                            );
                            return Err(MsSqlError::Connection(ConnectionError::Unhealthy {
                                consecutive_errors,
                                last_error: e.to_string(),
                            })
                            .into());
                        }
                    }
                }
            }
        }

        // Sleep before next poll, but check for shutdown
        tokio::select! {
            _ = sleep(poll_interval) => {}
            _ = shutdown_rx.changed() => {
                info!("CDC polling loop for source '{source_id}' shutdown during poll interval");
                return Ok(());
            }
        }
    }
}

/// Poll CDC for changes since last LSN
async fn poll_cdc_changes(
    source_id: &str,
    config: &MsSqlSourceConfig,
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    pk_cache: &PrimaryKeyCache,
    current_lsn: &mut Option<Lsn>,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    skip_threshold: &mut Option<SequencePosition>,
) -> Result<usize> {
    // Get current max LSN from CDC
    let max_lsn = get_max_lsn(client).await?;
    debug!("Max LSN from CDC: {}", max_lsn.to_hex());

    // If no current LSN, use configured start position
    let from_lsn = match current_lsn {
        Some(lsn) => {
            // Validate LSN is still valid
            if !is_valid_lsn(client, lsn, &config.tables[0]).await? {
                warn!(
                    "Stored LSN {} is no longer valid, falling back to start_position",
                    lsn.to_hex()
                );
                match config.start_position {
                    StartPosition::Beginning => {
                        let min_lsn = get_min_lsn(client, &config.tables[0]).await?;
                        info!("Using minimum available LSN: {}", min_lsn.to_hex());
                        min_lsn
                    }
                    StartPosition::Current => {
                        info!("Using current LSN (no historical changes)");
                        max_lsn
                    }
                }
            } else {
                debug!("Using stored LSN: {}", lsn.to_hex());
                *lsn
            }
        }
        None => match config.start_position {
            StartPosition::Beginning => {
                let min_lsn = get_min_lsn(client, &config.tables[0]).await?;
                info!(
                    "No checkpoint LSN, starting from beginning (minimum LSN: {})",
                    min_lsn.to_hex()
                );
                min_lsn
            }
            StartPosition::Current => {
                info!(
                    "No checkpoint LSN, starting from current (LSN: {})",
                    max_lsn.to_hex()
                );
                max_lsn
            }
        },
    };

    // Update current_lsn if it was newly initialized
    if current_lsn.is_none() {
        *current_lsn = Some(from_lsn);
    }

    // If from_lsn > max_lsn, no new changes.
    // Note: we use strict > (not >=) because the CDC range [from_lsn, max_lsn]
    // is inclusive on both ends. When from_lsn == max_lsn (e.g. during resume),
    // there may still be rows at that LSN to process; skip_threshold handles
    // filtering already-seen rows.
    if from_lsn > max_lsn {
        debug!(
            "No new changes: from_lsn {} >= max_lsn {}",
            from_lsn.to_hex(),
            max_lsn.to_hex()
        );
        return Ok(0);
    }

    debug!(
        "Querying changes from LSN {} to {}",
        from_lsn.to_hex(),
        max_lsn.to_hex()
    );

    let mut change_count = 0;
    let mut batch: Vec<(SourceChange, SequencePosition)> = Vec::new();

    // Query each configured table's CDC changes
    for table in &config.tables {
        debug!("Querying table '{table}' for changes");
        let changes = match query_table_changes(client, table, &from_lsn, &max_lsn).await {
            Ok(changes) => changes,
            Err(e) => {
                error!("Failed to query CDC changes for table '{table}': {e}");
                return Err(e);
            }
        };

        debug!("Found {} changes for table '{}'", changes.len(), table);

        for row in changes {
            // Extract CDC metadata
            let operation = extract_operation(&row)?;

            // Skip update before images - we only care about after
            if operation == CdcOperation::UpdateBefore {
                continue;
            }

            // Extract position components for sequence stamping
            let start_lsn = extract_lsn(&row)?;
            let seqval = extract_seqval(&row)?;
            let position = cdc_position(&start_lsn, &seqval);

            // During resume, skip events that were already processed
            if let Some(threshold) = skip_threshold {
                if position <= *threshold {
                    continue;
                }
            }

            // Generate element ID from primary key
            let element_id = pk_cache.generate_element_id(table, &row)?;

            // Extract label from table name (remove schema prefix if present)
            let label = table.split('.').next_back().unwrap_or(table);

            // Convert to SourceChange
            let change = match operation {
                CdcOperation::Insert => {
                    let properties = extract_properties_from_cdc_row(&row)?;
                    SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new(source_id, &element_id),
                                labels: Arc::from([Arc::from(label)]),
                                effective_from: 0,
                            },
                            properties,
                        },
                    }
                }
                CdcOperation::UpdateAfter => {
                    let properties = extract_properties_from_cdc_row(&row)?;
                    SourceChange::Update {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new(source_id, &element_id),
                                labels: Arc::from([Arc::from(label)]),
                                effective_from: 0,
                            },
                            properties,
                        },
                    }
                }
                CdcOperation::Delete => SourceChange::Delete {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source_id, &element_id),
                        labels: Arc::from([Arc::from(label)]),
                        effective_from: 0,
                    },
                },
                CdcOperation::UpdateBefore => unreachable!("filtered above"),
            };

            batch.push((change, position));
            change_count += 1;
        }
    }

    // Clear skip_threshold after the first poll that actually queried rows —
    // even if every row was filtered, the threshold has served its purpose.
    // Also advance current_lsn past max_lsn so we don't re-query the same
    // inclusive range when all rows were filtered by skip_threshold.
    if skip_threshold.is_some() {
        let next_lsn = increment_lsn(client, &max_lsn).await?;
        *current_lsn = Some(next_lsn);
        *skip_threshold = None;
    } else if change_count > 0 {
        // Normal (non-resume) path: advance past max_lsn after processing.
        let next_lsn = increment_lsn(client, &max_lsn).await?;
        *current_lsn = Some(next_lsn);
    }

    // Sort batch by position to ensure strict ordering across tables
    batch.sort_by_key(|(_, pos)| *pos);

    // Dispatch all changes in batch with sequence positions
    if !batch.is_empty() {
        debug!(
            "Dispatching {} changes to {} dispatchers",
            batch.len(),
            dispatchers.read().await.len()
        );
        let dispatchers = dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            for (change, position) in &batch {
                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_sequence(
                    source_id.to_string(),
                    SourceEvent::Change(change.clone()),
                    chrono::Utc::now(),
                    *position,
                    Some(profiling),
                );
                dispatcher.dispatch_change(Arc::new(wrapper)).await?;
            }
        }
        debug!("Dispatched all {} changes successfully", batch.len());
    } else {
        debug!("No changes to dispatch");
    }

    Ok(change_count)
}

/// Query CDC changes for a specific table
async fn query_table_changes(
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    table: &str,
    from_lsn: &Lsn,
    to_lsn: &Lsn,
) -> Result<Vec<tiberius::Row>> {
    // Validate table name to prevent SQL injection
    validate_sql_identifier(table)?;

    // MS SQL CDC function name: cdc.fn_cdc_get_all_changes_{capture_instance}
    // Capture instance is usually: {schema}_{table}
    // If table already contains schema (e.g., "dbo.Orders"), replace dot with underscore
    let capture_instance = table.replace('.', "_");

    // Tiberius uses @P1, @P2, etc. for positional parameters
    let query = format!(
        "SELECT * FROM cdc.fn_cdc_get_all_changes_{capture_instance}(@P1, @P2, 'all') ORDER BY __$start_lsn, __$seqval, __$operation"
    );

    debug!("CDC query: {query}");
    debug!(
        "From LSN: {}, To LSN: {}",
        from_lsn.to_hex(),
        to_lsn.to_hex()
    );

    let from_bytes = from_lsn.to_bytes();
    let to_bytes = to_lsn.to_bytes();

    let stream = client
        .query(&query, &[&from_bytes.as_slice(), &to_bytes.as_slice()])
        .await?;

    let rows = stream.into_first_result().await?;
    debug!(
        "Retrieved {} rows from CDC function for table '{}'",
        rows.len(),
        table
    );
    Ok(rows)
}

/// Get current maximum LSN
async fn get_max_lsn(
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
) -> Result<Lsn> {
    let stream = client
        .query("SELECT sys.fn_cdc_get_max_lsn() AS max_lsn", &[])
        .await?;

    let rows = stream.into_first_result().await?;

    if rows.is_empty() {
        return Err(anyhow!("No max LSN returned from CDC"));
    }

    let row = &rows[0];
    let lsn_bytes: &[u8] = row.try_get(0)?.ok_or_else(|| anyhow!("max_lsn is NULL"))?;

    Lsn::from_bytes(lsn_bytes)
}

/// Get minimum LSN for a table's CDC capture instance
pub async fn get_min_lsn(
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    table: &str,
) -> Result<Lsn> {
    // Validate table name to prevent SQL injection
    validate_sql_identifier(table)?;

    let capture_instance = table.replace('.', "_");

    // Use string formatting instead of parameters since sys.fn_cdc_get_min_lsn expects a string literal
    let query = format!("SELECT sys.fn_cdc_get_min_lsn('{capture_instance}') AS min_lsn");

    let stream = client.query(&query, &[]).await?;

    let rows = stream.into_first_result().await?;

    if rows.is_empty() {
        return Err(anyhow!("No min LSN returned from CDC"));
    }

    let row = &rows[0];
    let lsn_bytes: &[u8] = row.try_get(0)?.ok_or_else(|| anyhow!("min_lsn is NULL"))?;

    Lsn::from_bytes(lsn_bytes)
}

/// Check if an LSN is still valid (within CDC retention)
async fn is_valid_lsn(
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    lsn: &Lsn,
    table: &str,
) -> Result<bool> {
    // Validate table name to prevent SQL injection
    validate_sql_identifier(table)?;

    let capture_instance = table.replace('.', "_");
    let query = format!("SELECT sys.fn_cdc_get_min_lsn('{capture_instance}') AS min_lsn");

    let stream = client.query(&query, &[]).await?;

    let rows = stream.into_first_result().await?;

    if rows.is_empty() {
        return Ok(false);
    }

    let row = &rows[0];
    if let Ok(Some(min_lsn_bytes)) = row.try_get::<&[u8], _>(0) {
        let min_lsn = Lsn::from_bytes(min_lsn_bytes)?;
        Ok(lsn >= &min_lsn)
    } else {
        Ok(false)
    }
}

/// Extract CDC operation from row
fn extract_operation(row: &tiberius::Row) -> Result<CdcOperation> {
    let col_idx = row
        .columns()
        .iter()
        .position(|c| c.name() == cdc_columns::OPERATION)
        .ok_or_else(|| anyhow!("CDC row missing __$operation column"))?;

    let op_value: i32 = row
        .try_get(col_idx)?
        .ok_or_else(|| anyhow!("__$operation is NULL"))?;

    CdcOperation::from_i32(op_value)
}

/// Extract LSN from CDC row
fn extract_lsn(row: &tiberius::Row) -> Result<Lsn> {
    let col_idx = row
        .columns()
        .iter()
        .position(|c| c.name() == cdc_columns::START_LSN)
        .ok_or_else(|| anyhow!("CDC row missing __$start_lsn column"))?;

    let lsn_bytes: &[u8] = row
        .try_get(col_idx)?
        .ok_or_else(|| anyhow!("__$start_lsn is NULL"))?;

    Lsn::from_bytes(lsn_bytes)
}

/// Extract seqval (sequence value within transaction) from CDC row
fn extract_seqval(row: &tiberius::Row) -> Result<Lsn> {
    let col_idx = row
        .columns()
        .iter()
        .position(|c| c.name() == cdc_columns::SEQVAL)
        .ok_or_else(|| anyhow!("CDC row missing __$seqval column"))?;

    let seqval_bytes: &[u8] = row
        .try_get(col_idx)?
        .ok_or_else(|| anyhow!("__$seqval is NULL"))?;

    Lsn::from_bytes(seqval_bytes)
}

/// Increment an LSN using `sys.fn_cdc_increment_lsn`.
///
/// This is used to advance past a boundary so the next poll's inclusive range
/// does not reprocess the last row from the previous batch.
async fn increment_lsn(
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    lsn: &Lsn,
) -> Result<Lsn> {
    let lsn_bytes = lsn.to_bytes();
    let stream = client
        .query(
            "SELECT sys.fn_cdc_increment_lsn(@P1) AS next_lsn",
            &[&lsn_bytes.as_slice()],
        )
        .await?;

    let rows = stream.into_first_result().await?;
    if rows.is_empty() {
        return Err(anyhow!("No result from fn_cdc_increment_lsn"));
    }

    let row = &rows[0];
    let next_bytes: &[u8] = row
        .try_get(0)?
        .ok_or_else(|| anyhow!("fn_cdc_increment_lsn returned NULL"))?;

    Lsn::from_bytes(next_bytes)
}

/// Load LSN checkpoint from StateStore
async fn load_checkpoint(
    source_id: &str,
    state_store: &Option<Arc<dyn drasi_lib::state_store::StateStoreProvider>>,
) -> Result<Option<Lsn>> {
    if let Some(store) = state_store {
        let key = "checkpoint.lsn";
        if let Some(bytes) = store.get(source_id, key).await? {
            match Lsn::from_bytes(&bytes) {
                Ok(lsn) => {
                    info!("Loaded checkpoint LSN: {}", lsn.to_hex());
                    return Ok(Some(lsn));
                }
                Err(e) => {
                    warn!("Failed to parse stored LSN: {e}, starting fresh");
                }
            }
        }
    }
    Ok(None)
}

/// Save LSN checkpoint to StateStore
async fn save_checkpoint(
    source_id: &str,
    lsn: &Lsn,
    state_store: &Option<Arc<dyn drasi_lib::state_store::StateStoreProvider>>,
) -> Result<()> {
    if let Some(store) = state_store {
        let key = "checkpoint.lsn";
        let bytes = lsn.to_bytes();
        store.set(source_id, key, bytes).await?;
        debug!("Saved checkpoint LSN: {}", lsn.to_hex());
    }
    Ok(())
}

/// Clear LSN checkpoint from StateStore
async fn clear_checkpoint(
    source_id: &str,
    state_store: &Option<Arc<dyn drasi_lib::state_store::StateStoreProvider>>,
) -> Result<()> {
    if let Some(store) = state_store {
        let key = "checkpoint.lsn";
        store.delete(source_id, key).await?;
        info!("Cleared checkpoint LSN");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_instance_format() {
        let table = "orders";
        let capture_instance = format!("dbo_{table}");
        assert_eq!(capture_instance, "dbo_orders");
    }

    #[test]
    fn test_cdc_position_roundtrip() {
        let lsn = Lsn::from_hex("0000002700000019c002").unwrap();
        let seqval = Lsn::from_hex("0000002700000019c003").unwrap();

        let pos = cdc_position(&lsn, &seqval);
        assert_eq!(pos.as_bytes().len(), 20);

        let extracted = position_to_seek_lsn(&pos).unwrap();
        assert_eq!(extracted, lsn);
    }

    #[test]
    fn test_cdc_position_ordering() {
        // Same LSN, different seqval → position ordering follows seqval
        let lsn = Lsn::from_hex("0000002700000019c002").unwrap();
        let seq_a = Lsn::from_hex("0000002700000019c001").unwrap();
        let seq_b = Lsn::from_hex("0000002700000019c002").unwrap();

        let pos_a = cdc_position(&lsn, &seq_a);
        let pos_b = cdc_position(&lsn, &seq_b);
        assert!(pos_a < pos_b);
    }

    #[test]
    fn test_cdc_position_ordering_different_lsn() {
        // Different LSNs → ordered by LSN regardless of seqval
        let lsn_a = Lsn::from_hex("0000002700000019c001").unwrap();
        let lsn_b = Lsn::from_hex("0000002700000019c002").unwrap();
        let seqval = Lsn::zero();

        let pos_a = cdc_position(&lsn_a, &seqval);
        let pos_b = cdc_position(&lsn_b, &seqval);
        assert!(pos_a < pos_b);
    }

    #[test]
    fn test_position_to_seek_lsn_rejects_wrong_size() {
        let bad_pos = SequencePosition::from_u64(42);
        let result = position_to_seek_lsn(&bad_pos);
        assert!(result.is_err());
    }

    #[test]
    fn test_checkpoint_key_format() {
        let key = "checkpoint.lsn";
        assert_eq!(key, "checkpoint.lsn");
    }

    #[test]
    fn test_classify_error_detects_connection_issues() {
        use anyhow::anyhow;

        // Connection-related errors should be detected
        assert_eq!(
            classify_error(&anyhow!("connection reset by peer")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("Connection refused")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("broken pipe")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("network unreachable")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("socket closed")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("operation timed out")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("unexpected eof")),
            MsSqlErrorKind::Connection
        );
        assert_eq!(
            classify_error(&anyhow!("host unreachable")),
            MsSqlErrorKind::Connection
        );
    }

    #[test]
    fn test_classify_error_detects_lsn_issues() {
        use anyhow::anyhow;

        // LSN-related errors should be detected as recoverable
        assert_eq!(
            classify_error(&anyhow!("The specified LSN is invalid")),
            MsSqlErrorKind::RecoverableLsn
        );
        assert_eq!(
            classify_error(&anyhow!("LSN out of range")),
            MsSqlErrorKind::RecoverableLsn
        );
    }

    #[test]
    fn test_classify_error_other_errors() {
        use anyhow::anyhow;

        // Non-connection, non-LSN errors should be classified as Other
        assert_eq!(
            classify_error(&anyhow!("syntax error in query")),
            MsSqlErrorKind::Other
        );
        assert_eq!(
            classify_error(&anyhow!("permission denied")),
            MsSqlErrorKind::Other
        );
        assert_eq!(
            classify_error(&anyhow!("table not found")),
            MsSqlErrorKind::Other
        );
    }
}
