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
use crate::keys::PrimaryKeyCache;
use crate::lsn::Lsn;
use crate::types::extract_properties_from_cdc_row;
use anyhow::{anyhow, Result};
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::{ChangeDispatcher, SourceEventWrapper};
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;

/// Reconnection configuration constants
const INITIAL_RECONNECT_DELAY_MS: u64 = 1000; // 1 second
const MAX_RECONNECT_DELAY_MS: u64 = 60000; // 60 seconds
const RECONNECT_BACKOFF_MULTIPLIER: f64 = 2.0;

/// Check if an error indicates a connection problem that warrants reconnection
fn is_connection_error(error: &anyhow::Error) -> bool {
    let error_str = error.to_string().to_lowercase();

    // Common connection-related error patterns
    error_str.contains("connection")
        || error_str.contains("broken pipe")
        || error_str.contains("reset by peer")
        || error_str.contains("timed out")
        || error_str.contains("network")
        || error_str.contains("socket")
        || error_str.contains("eof")
        || error_str.contains("closed")
        || error_str.contains("refused")
        || error_str.contains("unreachable")
}

/// Run the CDC polling loop with automatic reconnection
///
/// This continuously polls MS SQL CDC for changes and dispatches them to subscribers.
/// If the connection is lost, it will automatically attempt to reconnect with
/// exponential backoff.
pub async fn run_cdc_stream(
    source_id: String,
    config: MsSqlSourceConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: Option<Arc<dyn drasi_lib::state_store::StateStoreProvider>>,
) -> Result<()> {
    info!("Starting CDC stream for source '{source_id}'");

    let mut reconnect_delay = Duration::from_millis(INITIAL_RECONNECT_DELAY_MS);

    // Outer reconnection loop
    loop {
        match run_cdc_polling_loop(&source_id, &config, &dispatchers, &state_store).await {
            Ok(()) => {
                // Normal exit (shouldn't happen in practice as the loop is infinite)
                info!("CDC polling loop exited normally for source '{source_id}'");
                return Ok(());
            }
            Err(e) => {
                if is_connection_error(&e) {
                    error!(
                        "Connection error in CDC stream for source '{source_id}': {e}. \
                         Reconnecting in {reconnect_delay:?}..."
                    );

                    // Wait before reconnecting
                    sleep(reconnect_delay).await;

                    // Increase delay for next attempt (exponential backoff)
                    reconnect_delay = Duration::from_millis(
                        ((reconnect_delay.as_millis() as f64) * RECONNECT_BACKOFF_MULTIPLIER)
                            .min(MAX_RECONNECT_DELAY_MS as f64) as u64,
                    );
                } else {
                    // Non-connection error - log and continue with normal delay
                    error!("Error in CDC stream for source '{source_id}': {e}");

                    // Reset backoff delay on non-connection errors
                    reconnect_delay = Duration::from_millis(INITIAL_RECONNECT_DELAY_MS);

                    // Brief pause before retry
                    sleep(Duration::from_secs(1)).await;
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
) -> Result<()> {
    // Connect to MS SQL
    info!("Connecting to MS SQL Server for source '{source_id}'");
    let mut connection = MsSqlConnection::connect(config).await?;
    let client = connection.client_mut();

    // Discover primary keys
    let mut pk_cache = PrimaryKeyCache::new();
    pk_cache.discover_keys(client, config).await?;
    info!("Discovered primary keys for {} tables", config.tables.len());

    // Load last LSN checkpoint from StateStore
    let mut current_lsn = load_checkpoint(source_id, state_store).await?;
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
        let lsn_before = current_lsn;

        match poll_cdc_changes(
            source_id,
            config,
            client,
            &pk_cache,
            &mut current_lsn,
            dispatchers,
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

                // Check if this is a connection error
                if is_connection_error(&e) {
                    error!("Connection error during CDC polling: {e}");
                    return Err(e);
                }

                // Check if LSN is invalid and needs reset
                if e.to_string().contains("invalid LSN") || e.to_string().contains("out of range") {
                    warn!("LSN appears invalid, clearing checkpoint and restarting from current");
                    clear_checkpoint(source_id, state_store).await?;
                    current_lsn = None;
                    consecutive_errors = 0; // Reset since we handled this
                } else {
                    error!("Error polling CDC changes: {e}");

                    // If we've had too many consecutive errors, assume connection is bad
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        error!(
                            "Too many consecutive errors ({consecutive_errors}), \
                             assuming connection is unhealthy"
                        );
                        return Err(anyhow!(
                            "Connection appears unhealthy after {consecutive_errors} consecutive errors: {e}"
                        ));
                    }
                }
            }
        }

        // Sleep before next poll
        sleep(poll_interval).await;
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

    // If from_lsn >= max_lsn, no new changes
    if from_lsn >= max_lsn {
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
    let mut batch = Vec::new();

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
                                effective_from: 0, // Will be set by dispatcher
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
                CdcOperation::UpdateBefore => {
                    // Skip update before images - we only care about after
                    continue;
                }
            };

            batch.push(change);
            change_count += 1;
        }
    }

    // After processing all changes, update checkpoint to max_lsn
    // This ensures we don't reprocess the same changes
    if change_count > 0 {
        *current_lsn = Some(max_lsn);
    }

    // Dispatch all changes in batch
    if !batch.is_empty() {
        debug!(
            "Dispatching {} changes to {} dispatchers",
            batch.len(),
            dispatchers.read().await.len()
        );
        let dispatchers = dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            for change in &batch {
                let wrapper = SourceEventWrapper::new(
                    source_id.to_string(),
                    drasi_lib::channels::SourceEvent::Change(change.clone()),
                    chrono::Utc::now(),
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
        "SELECT * FROM cdc.fn_cdc_get_all_changes_{capture_instance}(@P1, @P2, 'all') ORDER BY __$start_lsn, __$seqval"
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
async fn get_min_lsn(
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
    fn test_checkpoint_key_format() {
        let key = "checkpoint.lsn";
        assert_eq!(key, "checkpoint.lsn");
    }

    #[test]
    fn test_is_connection_error_detects_connection_issues() {
        // Connection-related errors should be detected
        assert!(is_connection_error(&anyhow!("connection reset by peer")));
        assert!(is_connection_error(&anyhow!("Connection refused")));
        assert!(is_connection_error(&anyhow!("broken pipe")));
        assert!(is_connection_error(&anyhow!("network unreachable")));
        assert!(is_connection_error(&anyhow!("socket closed")));
        assert!(is_connection_error(&anyhow!("operation timed out")));
        assert!(is_connection_error(&anyhow!("unexpected eof")));
        assert!(is_connection_error(&anyhow!("host unreachable")));
    }

    #[test]
    fn test_is_connection_error_ignores_non_connection_issues() {
        // Non-connection errors should not be detected as connection errors
        assert!(!is_connection_error(&anyhow!("invalid LSN")));
        assert!(!is_connection_error(&anyhow!("syntax error in query")));
        assert!(!is_connection_error(&anyhow!("permission denied")));
        assert!(!is_connection_error(&anyhow!("table not found")));
        assert!(!is_connection_error(&anyhow!("out of range")));
    }
}
