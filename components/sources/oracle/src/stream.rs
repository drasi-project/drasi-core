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

//! Oracle LogMiner polling stream implementation.

use crate::{
    BootstrapSyncState, OracleConnection, OracleError, OracleErrorKind, OracleSourceConfig,
    PrimaryKeyCache, Scn, StartPosition,
};
use anyhow::{anyhow, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::channels::{ChangeDispatcher, SourceEvent, SourceEventWrapper};
use drasi_lib::profiling;
use drasi_lib::state_store::StateStoreProvider;
use drasi_oracle_common::{
    extract_row_properties, parse_sql_undo_insert, parse_sql_undo_update, split_table_name,
    sql_literal_to_element_value, LogMinerGuard,
};
use oracle::Connection;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, RwLock};

const CHECKPOINT_KEY: &str = "checkpoint.scn";
const INITIAL_RECONNECT_DELAY_MS: u64 = 1_000;
const MAX_RECONNECT_DELAY_MS: u64 = 60_000;
const RECONNECT_BACKOFF_MULTIPLIER: f64 = 2.0;

#[derive(Debug, Clone, PartialEq, Eq)]
enum OracleOperation {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone)]
struct OracleChangeEvent {
    commit_scn: Scn,
    scn: Scn,
    operation: OracleOperation,
    schema_name: String,
    table_name: String,
    sql_undo: Option<String>,
    row_id: Option<String>,
}

pub(crate) async fn run_logminer_stream(
    source_id: String,
    config: OracleSourceConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    bootstrap_sync: Arc<AsyncMutex<BootstrapSyncState>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut reconnect_delay = Duration::from_millis(INITIAL_RECONNECT_DELAY_MS);

    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        match run_stream_session(
            source_id.clone(),
            config.clone(),
            dispatchers.clone(),
            state_store.clone(),
            bootstrap_sync.clone(),
            shutdown_rx.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }

                match OracleError::classify(&error).unwrap_or(OracleErrorKind::Other) {
                    OracleErrorKind::Connection => {
                        log::error!(
                            "Oracle connection error for source '{source_id}': {error}. Reconnecting in {reconnect_delay:?}..."
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(reconnect_delay) => {}
                            _ = shutdown_rx.changed() => return Ok(()),
                        }
                        reconnect_delay = Duration::from_millis(
                            ((reconnect_delay.as_millis() as f64) * RECONNECT_BACKOFF_MULTIPLIER)
                                .min(MAX_RECONNECT_DELAY_MS as f64)
                                as u64,
                        );
                    }
                    OracleErrorKind::RecoverableScn | OracleErrorKind::Other => {
                        log::error!("Oracle stream error for source '{source_id}': {error}");
                        reconnect_delay = Duration::from_millis(INITIAL_RECONNECT_DELAY_MS);
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                            _ = shutdown_rx.changed() => return Ok(()),
                        }
                    }
                }
            }
        }
    }
}

async fn run_stream_session(
    source_id: String,
    config: OracleSourceConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    bootstrap_sync: Arc<AsyncMutex<BootstrapSyncState>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<SourceChange>>();
    let runtime = tokio::runtime::Handle::current();
    let source_id_for_worker = source_id.clone();

    let worker = tokio::task::spawn_blocking(move || {
        run_logminer_poll_loop(
            source_id_for_worker,
            config,
            state_store,
            bootstrap_sync,
            shutdown_rx,
            tx,
            runtime,
        )
    });

    while let Some(batch) = rx.recv().await {
        dispatch_batch(&source_id, &dispatchers, batch).await?;
    }

    worker
        .await
        .map_err(|error| anyhow!("Oracle worker task panicked: {error}"))?
}

async fn dispatch_batch(
    source_id: &str,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    batch: Vec<SourceChange>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let dispatchers = dispatchers.read().await;
    for dispatcher in dispatchers.iter() {
        for change in &batch {
            let mut profiling = profiling::ProfilingMetadata::new();
            profiling.source_send_ns = Some(profiling::timestamp_ns());

            let wrapper = SourceEventWrapper::with_profiling(
                source_id.to_string(),
                SourceEvent::Change(change.clone()),
                chrono::Utc::now(),
                profiling,
            );
            dispatcher.dispatch_change(Arc::new(wrapper)).await?;
        }
    }

    Ok(())
}

fn run_logminer_poll_loop(
    source_id: String,
    config: OracleSourceConfig,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    bootstrap_sync: Arc<AsyncMutex<BootstrapSyncState>>,
    shutdown_rx: watch::Receiver<bool>,
    batch_tx: mpsc::UnboundedSender<Vec<SourceChange>>,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    let mut shutdown_rx = shutdown_rx;
    let connection = OracleConnection::connect(&config)?;
    connection.test_connection()?;
    let conn = connection.inner();

    check_archivelog_mode(conn)?;

    let mut pk_cache = PrimaryKeyCache::new();
    pk_cache.discover_keys(conn, &config)?;

    let mut current_checkpoint =
        if let Some(checkpoint) = load_checkpoint(&runtime, &source_id, &state_store)? {
            checkpoint
        } else {
            resolve_initial_scn(conn, config.start_position)?
        };

    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        {
            let mut bootstrap_state = bootstrap_sync.blocking_lock();
            if let Some(pending_bootstrap_scn) = bootstrap_state.pending_scn.take() {
                if pending_bootstrap_scn > current_checkpoint {
                    current_checkpoint = pending_bootstrap_scn;
                    save_checkpoint(&runtime, &source_id, &state_store, current_checkpoint)?;
                }
            }

            let current_scn = get_current_scn(conn)?;
            if current_scn <= current_checkpoint {
                std::thread::sleep(Duration::from_millis(config.poll_interval_ms));
                continue;
            }

            match poll_logminer(
                conn,
                current_checkpoint,
                current_scn,
                &config.tables,
                &config.user,
            ) {
                Ok((events, next_checkpoint)) => {
                    let changes = convert_events_to_changes(&source_id, conn, &pk_cache, &events)?;
                    if !changes.is_empty() {
                        batch_tx
                            .send(changes)
                            .map_err(|error| anyhow!("failed to send Oracle batch: {error}"))?;
                    }
                    if next_checkpoint != current_checkpoint {
                        save_checkpoint(&runtime, &source_id, &state_store, next_checkpoint)?;
                        current_checkpoint = next_checkpoint;
                    }
                }
                Err(error) => {
                    if OracleError::classify(&error) == Some(OracleErrorKind::RecoverableScn) {
                        clear_checkpoint(&runtime, &source_id, &state_store)?;
                        current_checkpoint = get_current_scn(conn)?;
                        continue;
                    }
                    return Err(error);
                }
            }
        }

        std::thread::sleep(Duration::from_millis(config.poll_interval_ms));
    }
}

fn convert_events_to_changes(
    source_id: &str,
    conn: &Connection,
    pk_cache: &PrimaryKeyCache,
    events: &[OracleChangeEvent],
) -> Result<Vec<SourceChange>> {
    let mut grouped_indices = HashMap::<String, Vec<usize>>::new();
    for (index, event) in events.iter().enumerate() {
        let key = event_group_key(event, index, pk_cache)?;
        grouped_indices.entry(key).or_default().push(index);
    }

    let mut materialized = BTreeMap::<usize, SourceChange>::new();
    for indices in grouped_indices.values() {
        materialize_group(
            indices,
            source_id,
            conn,
            pk_cache,
            events,
            &mut materialized,
        )?;
    }

    Ok(materialized.into_values().collect())
}

fn materialize_group(
    indices: &[usize],
    source_id: &str,
    conn: &Connection,
    pk_cache: &PrimaryKeyCache,
    events: &[OracleChangeEvent],
    materialized: &mut BTreeMap<usize, SourceChange>,
) -> Result<()> {
    if indices.is_empty() {
        return Ok(());
    }

    let latest_event = &events[*indices.last().expect("group indices must be non-empty")];
    let mut state_after = latest_row_state(conn, latest_event)?;

    for index in indices.iter().rev().copied() {
        let event = &events[index];
        let change = match event.operation {
            OracleOperation::Insert => {
                let Some(after_state) = state_after.clone() else {
                    log::warn!(
                        "Skipping Oracle INSERT for {}.{} at COMMIT_SCN {} because the committed row image could not be reconstructed",
                        event.schema_name,
                        event.table_name,
                        event.commit_scn
                    );
                    continue;
                };
                let change =
                    make_insert_or_update_change(source_id, pk_cache, event, after_state, true)?;
                state_after = None;
                change
            }
            OracleOperation::Update => {
                let Some(after_state) = state_after.clone() else {
                    log::warn!(
                        "Skipping Oracle UPDATE for {}.{} at COMMIT_SCN {} because the committed row image could not be reconstructed",
                        event.schema_name,
                        event.table_name,
                        event.commit_scn
                    );
                    continue;
                };
                let change = make_insert_or_update_change(
                    source_id,
                    pk_cache,
                    event,
                    after_state.clone(),
                    false,
                )?;
                state_after = Some(apply_sql_undo_update(&after_state, event)?);
                change
            }
            OracleOperation::Delete => {
                let before_state = delete_before_state(event)?;
                let change = make_delete_change(source_id, pk_cache, event, &before_state)?;
                state_after = Some(before_state);
                change
            }
        };

        materialized.insert(index, change);
    }

    Ok(())
}

fn latest_row_state(
    conn: &Connection,
    event: &OracleChangeEvent,
) -> Result<Option<ElementPropertyMap>> {
    match event.operation {
        OracleOperation::Delete => Ok(None),
        OracleOperation::Insert | OracleOperation::Update => {
            let Some(row_id) = event.row_id.as_deref() else {
                return Ok(None);
            };

            let query = format!(
                "SELECT * FROM \"{}\".\"{}\" WHERE ROWID = :1",
                event.schema_name, event.table_name
            );
            match conn.query_row(&query, &[&row_id]) {
                Ok(row) => Ok(Some(extract_row_properties(&row)?)),
                Err(error) => {
                    log::warn!(
                        "Skipping Oracle {:?} for {}.{} because row fetch by ROWID {} failed: {}",
                        event.operation,
                        event.schema_name,
                        event.table_name,
                        row_id,
                        error
                    );
                    Ok(None)
                }
            }
        }
    }
}

fn make_insert_or_update_change(
    source_id: &str,
    pk_cache: &PrimaryKeyCache,
    event: &OracleChangeEvent,
    properties: ElementPropertyMap,
    is_insert: bool,
) -> Result<SourceChange> {
    let label = event.table_name.to_lowercase();
    let element_id =
        pk_cache.make_element_id(&event.schema_name, &event.table_name, &properties)?;
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: Arc::from([Arc::from(label.as_str())]),
            effective_from: 0,
        },
        properties,
    };

    Ok(if is_insert {
        SourceChange::Insert { element }
    } else {
        SourceChange::Update { element }
    })
}

fn make_delete_change(
    source_id: &str,
    pk_cache: &PrimaryKeyCache,
    event: &OracleChangeEvent,
    properties: &ElementPropertyMap,
) -> Result<SourceChange> {
    let label = event.table_name.to_lowercase();
    let element_id = pk_cache.make_element_id(&event.schema_name, &event.table_name, properties)?;
    Ok(SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: Arc::from([Arc::from(label.as_str())]),
            effective_from: 0,
        },
    })
}

fn delete_before_state(event: &OracleChangeEvent) -> Result<ElementPropertyMap> {
    let sql_undo = event.sql_undo.as_deref().ok_or_else(|| {
        anyhow!(
            "Oracle DELETE for {}.{} at COMMIT_SCN {} is missing SQL_UNDO",
            event.schema_name,
            event.table_name,
            event.commit_scn
        )
    })?;
    sql_undo_insert_to_properties(sql_undo)
}

fn apply_sql_undo_update(
    current_state: &ElementPropertyMap,
    event: &OracleChangeEvent,
) -> Result<ElementPropertyMap> {
    let sql_undo = event.sql_undo.as_deref().ok_or_else(|| {
        anyhow!(
            "Oracle UPDATE for {}.{} at COMMIT_SCN {} is missing SQL_UNDO",
            event.schema_name,
            event.table_name,
            event.commit_scn
        )
    })?;

    let assignments = parse_sql_undo_update(sql_undo)?;
    let mut previous_state = current_state.clone();
    for (column, value) in assignments {
        previous_state.insert(&column, sql_literal_to_element_value(&value));
    }
    Ok(previous_state)
}

fn sql_undo_insert_to_properties(sql_undo: &str) -> Result<ElementPropertyMap> {
    let values = parse_sql_undo_insert(sql_undo)?;
    let mut properties = ElementPropertyMap::new();
    for (column, value) in values {
        properties.insert(&column, sql_literal_to_element_value(&value));
    }
    Ok(properties)
}

fn event_group_key(
    event: &OracleChangeEvent,
    index: usize,
    pk_cache: &PrimaryKeyCache,
) -> Result<String> {
    if let Some(row_id) = event.row_id.as_deref() {
        return Ok(format!(
            "{}.{}:{row_id}",
            event.schema_name.to_uppercase(),
            event.table_name.to_uppercase()
        ));
    }

    if let Some(sql_undo) = event.sql_undo.as_deref() {
        let values = parse_sql_undo_insert(sql_undo)?;
        let element_id =
            pk_cache.make_element_id_from_values(&event.schema_name, &event.table_name, &values)?;
        return Ok(element_id);
    }

    Ok(format!(
        "{}.{}:{}:{}:{index}",
        event.schema_name.to_uppercase(),
        event.table_name.to_uppercase(),
        event.commit_scn.0,
        event.scn.0
    ))
}

fn resolve_initial_scn(conn: &Connection, start_position: StartPosition) -> Result<Scn> {
    match start_position {
        StartPosition::Beginning => get_min_available_scn(conn),
        StartPosition::Current => Ok(get_current_scn(conn)?),
    }
}

fn get_current_scn(conn: &Connection) -> Result<Scn> {
    let row = conn.query_row("SELECT CURRENT_SCN FROM V$DATABASE", &[])?;
    let scn: i64 = row.get(0)?;
    Ok(Scn(scn as u64))
}

fn get_min_available_scn(conn: &Connection) -> Result<Scn> {
    let row = conn.query_row(
        "SELECT COALESCE(MIN(FIRST_CHANGE#), 0) FROM V$ARCHIVED_LOG WHERE FIRST_CHANGE# IS NOT NULL",
        &[],
    )?;
    let scn: i64 = row.get(0)?;
    if scn <= 0 {
        return Err(OracleError::RecoverableScn(
            "Unable to determine the earliest available Oracle archived log SCN".to_string(),
        )
        .into());
    }
    Ok(Scn(scn as u64))
}

fn check_archivelog_mode(conn: &Connection) -> Result<()> {
    let row = conn.query_row("SELECT LOG_MODE FROM V$DATABASE", &[])?;
    let mode: String = row.get(0)?;
    if mode.trim() != "ARCHIVELOG" {
        return Err(OracleError::Config(format!(
            "Oracle database is in {} mode. LogMiner requires ARCHIVELOG mode.",
            mode.trim()
        ))
        .into());
    }
    Ok(())
}

fn poll_logminer(
    conn: &Connection,
    start_scn: Scn,
    end_scn: Scn,
    tables: &[String],
    default_owner: &str,
) -> Result<(Vec<OracleChangeEvent>, Scn)> {
    if end_scn <= start_scn {
        return Ok((Vec::new(), start_scn));
    }

    let predicates = table_predicates(tables, default_owner)?;
    let mut guard = LogMinerGuard::new(conn, start_scn, end_scn)?;

    let query = format!(
        "SELECT SCN, COMMIT_SCN, OPERATION_CODE, SEG_OWNER, SEG_NAME, SQL_UNDO, ROW_ID
         FROM V$LOGMNR_CONTENTS
         WHERE OPERATION_CODE IN (1, 2, 3)
           AND COMMIT_SCN > {}
           AND COMMIT_SCN <= {}
           AND ({})
         ORDER BY COMMIT_SCN, SCN",
        start_scn.0, end_scn.0, predicates
    );

    let rows = conn.query(&query, &[])?;
    let mut events = Vec::new();
    let mut max_commit_scn = start_scn;

    for row in rows {
        let row = row?;
        let scn: i64 = row.get(0)?;
        let commit_scn: i64 = row.get(1)?;
        let operation_code: i64 = row.get(2)?;
        let schema_name: String = row.get(3)?;
        let table_name: String = row.get(4)?;
        let sql_undo: Option<String> = row.get(5).ok();
        let row_id: Option<String> = row.get(6).ok();

        let commit_scn = Scn(commit_scn as u64);
        if commit_scn > max_commit_scn {
            max_commit_scn = commit_scn;
        }

        events.push(OracleChangeEvent {
            commit_scn,
            scn: Scn(scn as u64),
            operation: match operation_code {
                1 => OracleOperation::Insert,
                2 => OracleOperation::Delete,
                3 => OracleOperation::Update,
                _ => continue,
            },
            schema_name,
            table_name,
            sql_undo,
            row_id,
        });
    }

    guard.stop()?;
    Ok((events, max_commit_scn))
}

fn table_predicates(tables: &[String], default_owner: &str) -> Result<String> {
    let mut predicates = Vec::with_capacity(tables.len());
    for table in tables {
        let (schema, table_name) = split_table_name(table, default_owner)?;
        predicates.push(format!(
            "(SEG_OWNER = '{schema}' AND SEG_NAME = '{table_name}')"
        ));
    }

    Ok(predicates.join(" OR "))
}

fn load_checkpoint(
    runtime: &tokio::runtime::Handle,
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<Option<Scn>> {
    if let Some(store) = state_store {
        if let Some(bytes) = runtime.block_on(store.get(source_id, CHECKPOINT_KEY))? {
            match Scn::from_bytes(&bytes) {
                Ok(scn) => return Ok(Some(scn)),
                Err(error) => {
                    log::warn!("Failed to parse stored Oracle SCN checkpoint: {error}");
                }
            }
        }
    }
    Ok(None)
}

fn save_checkpoint(
    runtime: &tokio::runtime::Handle,
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    scn: Scn,
) -> Result<()> {
    if let Some(store) = state_store {
        runtime.block_on(store.set(source_id, CHECKPOINT_KEY, scn.to_bytes()))?;
    }
    Ok(())
}

fn clear_checkpoint(
    runtime: &tokio::runtime::Handle,
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<()> {
    if let Some(store) = state_store {
        runtime.block_on(store.delete(source_id, CHECKPOINT_KEY))?;
    }
    Ok(())
}

pub fn validate_connection(config: &OracleSourceConfig) -> Result<()> {
    let connection = OracleConnection::connect(config)?;
    connection.test_connection()?;
    let conn = connection.inner();
    check_archivelog_mode(conn)?;

    let mut pk_cache = PrimaryKeyCache::new();
    pk_cache.discover_keys(conn, config)?;

    Ok(())
}

pub fn fetch_bootstrap_scn(config: &OracleSourceConfig) -> Result<Scn> {
    let connection = OracleConnection::connect(config)?;
    connection.test_connection()?;
    get_current_scn(connection.inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::ElementValue;

    #[test]
    fn test_table_predicates() {
        let predicates = table_predicates(
            &["HR.EMPLOYEES".to_string(), "HR.DEPARTMENTS".to_string()],
            "system",
        )
        .unwrap();
        assert!(predicates.contains("SEG_OWNER = 'HR'"));
        assert!(predicates.contains("SEG_NAME = 'EMPLOYEES'"));
        assert!(predicates.contains("SEG_NAME = 'DEPARTMENTS'"));
    }

    #[test]
    fn test_checkpoint_key() {
        assert_eq!(CHECKPOINT_KEY, "checkpoint.scn");
    }

    #[test]
    fn test_table_predicates_use_default_owner() {
        let predicates = table_predicates(&["employees".to_string()], "hr").unwrap();
        assert!(predicates.contains("SEG_OWNER = 'HR'"));
        assert!(predicates.contains("SEG_NAME = 'EMPLOYEES'"));
    }

    #[test]
    fn test_sql_undo_insert_to_properties() {
        let properties = sql_undo_insert_to_properties(
            r#"INSERT INTO "HR"."EMPLOYEES"("EMPLOYEE_ID","NAME") VALUES (42,'Bob')"#,
        )
        .unwrap();
        assert_eq!(properties["employee_id"], ElementValue::Integer(42));
        assert_eq!(properties["name"], ElementValue::String(Arc::from("Bob")));
    }

    #[test]
    fn test_apply_sql_undo_update_restores_previous_state() {
        let event = OracleChangeEvent {
            commit_scn: Scn(2),
            scn: Scn(2),
            operation: OracleOperation::Update,
            schema_name: "HR".to_string(),
            table_name: "EMPLOYEES".to_string(),
            sql_undo: Some(
                r#"update "HR"."EMPLOYEES" set "NAME" = 'Bob', "MANAGER_ID" = NULL where "EMPLOYEE_ID" = 42"#.to_string(),
            ),
            row_id: Some("AAABBB".to_string()),
        };
        let mut current_state = ElementPropertyMap::new();
        current_state.insert("employee_id", ElementValue::Integer(42));
        current_state.insert("name", ElementValue::String(Arc::from("Alice")));
        current_state.insert("manager_id", ElementValue::Integer(7));

        let previous_state = apply_sql_undo_update(&current_state, &event).unwrap();

        assert_eq!(previous_state["employee_id"], ElementValue::Integer(42));
        assert_eq!(
            previous_state["name"],
            ElementValue::String(Arc::from("Bob"))
        );
        assert_eq!(previous_state["manager_id"], ElementValue::Null);
    }
}
