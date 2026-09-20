// Copyright 2026 The Drasi Authors.
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

#![cfg(feature = "computation")]

use anyhow::{Context, Result};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    config::QueryConfig,
    queries::output_state::SnapshotStream,
    queries::{FetchError, SnapshotResponse},
    CapacityPolicy, DrasiLib, DurabilityConfig, ExecutionMode, Query, RecoveryPolicy,
    StorageBackendRef,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_wal_redb::RedbWalProvider;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, timeout, Instant};

const SOURCE: &str = "people";
const QUERY: &str = "names";
const TEXT: &str = "MATCH (p:Person) RETURN p.name AS name";
const UPDATED_TEXT: &str = "MATCH (p:Person) RETURN p.name AS name, p.active AS active";

fn config(text: &str, capacity: usize) -> QueryConfig {
    Query::cypher(QUERY)
        .query(text)
        .from_source(SOURCE)
        .enable_bootstrap(false)
        .auto_start(true)
        .with_outbox_capacity(capacity)
        .with_storage_backend(StorageBackendRef::Named("rocks".into()))
        .with_recovery_policy(RecoveryPolicy::Strict)
        .build()
}

async fn build(
    root: &Path,
    instance: &str,
    query: QueryConfig,
) -> Result<(DrasiLib, ApplicationSourceHandle)> {
    let (source, handle) = ApplicationSource::new(
        SOURCE,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: Some(DurabilityConfig {
                enabled: true,
                max_events: 128,
                capacity_policy: CapacityPolicy::RejectIncoming,
            }),
        },
    )?;
    let core = DrasiLib::builder()
        .with_id(instance)
        .with_execution_mode(ExecutionMode::ComputationGraph)
        .with_source(source)
        .with_query(query)
        .with_wal_provider(Arc::new(RedbWalProvider::new(root.join("wal"))))
        .with_index_provider(
            "rocks",
            Arc::new(RocksDbIndexProvider::new(
                root.join("indexes"),
                false,
                false,
            )),
        )
        .build()
        .await?;
    core.start().await?;
    ready(&core).await?;
    Ok((core, handle))
}

async fn ready(core: &DrasiLib) -> Result<()> {
    timeout(
        Duration::from_secs(15),
        core.computation_component(QUERY)?.wait_started(),
    )
    .await
    .context("query readiness timed out")??;
    Ok(())
}

async fn insert(source: &ApplicationSourceHandle, id: &str) -> Result<()> {
    source
        .send_node_insert(
            id,
            vec!["Person"],
            PropertyMapBuilder::new()
                .with_string("name", id)
                .with_bool("active", true)
                .build(),
        )
        .await
}

async fn snapshot(core: &DrasiLib, minimum: u64) -> Result<SnapshotResponse> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let diagnostic = match core.query_manager().get_query_instance(QUERY).await {
            Ok(query) => match query.fetch_snapshot().await {
                Ok(snapshot) if snapshot.as_of_sequence >= minimum => return Ok(snapshot),
                Ok(snapshot) => format!("output is only at sequence {}", snapshot.as_of_sequence),
                Err(error) => error.to_string(),
            },
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            anyhow::bail!("query snapshot did not reach sequence {minimum}: {diagnostic}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn shutdown(core: DrasiLib, source: ApplicationSourceHandle) -> Result<()> {
    core.shutdown().await?;
    drop(source);
    drop(core);
    Ok(())
}

async fn keyed(snapshot: SnapshotResponse) -> BTreeMap<u64, serde_json::Value> {
    SnapshotStream::from_snapshot(snapshot)
        .collect_keyed_vec()
        .await
        .into_iter()
        .collect()
}

#[tokio::test]
async fn ordinary_persistent_query_reopens_snapshot_and_trimmed_outbox_without_replaying_input(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let instance = "computation-output-reopen";
    let (core, source) = build(directory.path(), instance, config(TEXT, 2)).await?;
    for id in ["one", "two", "three", "four"] {
        insert(&source, id).await?;
    }
    let before = snapshot(&core, 4).await?;
    assert_eq!(before.to_vec().len(), 4);
    let generation = before.output_generation;
    let head = before.as_of_sequence;
    shutdown(core, source).await?;

    let (core, source) = build(directory.path(), instance, config(TEXT, 2)).await?;
    let reopened = snapshot(&core, head).await?;
    assert_eq!(reopened.as_of_sequence, head);
    assert_eq!(reopened.output_generation, generation);
    assert_eq!(keyed(reopened).await, keyed(before).await);
    {
        let query = core
            .query_manager()
            .get_query_instance(QUERY)
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(!query.is_volatile());
        assert_eq!(query.output_generation().await, generation);
        assert!(matches!(
            query.fetch_outbox(0).await,
            Err(FetchError::OutboxGap(_))
        ));
        let retained = query.fetch_outbox(head - 2).await?;
        assert_eq!(
            retained
                .results
                .iter()
                .map(|result| result.sequence)
                .collect::<Vec<_>>(),
            vec![head - 1, head]
        );
        assert_eq!(retained.output_generation, generation);
    }
    insert(&source, "five").await?;
    let after = snapshot(&core, head + 1).await?;
    assert_eq!(after.as_of_sequence, head + 1);
    assert_eq!(after.to_vec().len(), 5);
    shutdown(core, source).await
}

#[tokio::test]
async fn deleting_and_recreating_a_query_preserves_a_distinct_persistent_output_lifetime(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let instance = "computation-output-recreate";
    let query_config = config(TEXT, 8);
    let (core, source) = build(directory.path(), instance, query_config.clone()).await?;
    insert(&source, "old").await?;
    let old = snapshot(&core, 1).await?;
    let old_reader = core
        .query_manager()
        .get_query_instance(QUERY)
        .await
        .map_err(anyhow::Error::msg)?;
    core.remove_query(QUERY).await?;
    assert_eq!(old_reader.output_generation().await, old.output_generation);
    drop(old_reader);
    core.add_query(query_config).await?;
    ready(&core).await?;
    let recreated = snapshot(&core, 0).await?;
    assert!(recreated.to_vec().is_empty());
    assert!(recreated.output_generation > old.output_generation);
    insert(&source, "new").await?;
    let current = snapshot(&core, recreated.as_of_sequence + 1).await?;
    assert_eq!(current.to_vec(), vec![serde_json::json!({"name": "new"})]);
    let generation = current.output_generation;
    let head = current.as_of_sequence;
    shutdown(core, source).await?;

    let (core, source) = build(directory.path(), instance, config(TEXT, 8)).await?;
    let reopened = snapshot(&core, head).await?;
    assert_eq!(reopened.output_generation, generation);
    assert_eq!(keyed(reopened).await, keyed(current).await);
    shutdown(core, source).await
}

#[tokio::test]
async fn reconfiguration_wipes_old_output_and_persists_the_new_lifetime() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let instance = "computation-output-update";
    let (core, source) = build(directory.path(), instance, config(TEXT, 8)).await?;
    insert(&source, "old").await?;
    let old = snapshot(&core, 1).await?;
    core.update_query(QUERY, config(UPDATED_TEXT, 8)).await?;
    ready(&core).await?;
    let reset = snapshot(&core, 0).await?;
    assert!(reset.to_vec().is_empty());
    assert!(reset.output_generation > old.output_generation);
    insert(&source, "new").await?;
    let current = snapshot(&core, reset.as_of_sequence + 1).await?;
    assert_eq!(
        current.to_vec(),
        vec![serde_json::json!({"name": "new", "active": true})]
    );
    shutdown(core, source).await?;

    let (core, source) = build(directory.path(), instance, config(UPDATED_TEXT, 8)).await?;
    let reopened = snapshot(&core, current.as_of_sequence).await?;
    assert_eq!(reopened.output_generation, current.output_generation);
    assert_eq!(keyed(reopened).await, keyed(current).await);
    shutdown(core, source).await
}
