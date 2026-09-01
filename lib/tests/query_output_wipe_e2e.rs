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

//! In-process wipe tests for #823 using a persistent RocksDB index.

mod mock_source;

use anyhow::Result;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::ComponentStatus;
use drasi_lib::queries::Query as QueryInstance;
use drasi_lib::{DrasiLib, IndexBackendPlugin, Query, StorageBackendRef};
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const SOURCE_ID: &str = "test-source";
const QUERY_ID: &str = "q1";
const QUERY_V1: &str = "MATCH (p:Person) RETURN p.name AS name, p.age AS age";
const QUERY_V2: &str = "MATCH (p:Person) RETURN p.name AS name, p.age AS age, p.age AS age2";

fn persistent_query(text: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(QUERY_ID)
        .query(text)
        .from_source(SOURCE_ID)
        .auto_start(true)
        .enable_bootstrap(false)
        .with_outbox_capacity(100)
        .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
        .build()
}

async fn insert_person(handle: &MockSourceHandle, id: &str, name: &str, age: i64) -> Result<()> {
    let props = PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", age)
        .build();
    handle.send_node_insert(id, vec!["Person"], props).await
}

async fn wait_for_status(core: &DrasiLib, id: &str, expected: ComponentStatus) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if core.get_query_status(id).await? == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("component '{id}' did not reach {expected:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_seq(core: &DrasiLib, min_seq: u64) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let query = core
            .query_manager()
            .get_query_instance(QUERY_ID)
            .await
            .map_err(anyhow::Error::msg)?;
        let snapshot = query.fetch_snapshot().await?;
        if snapshot.as_of_sequence >= min_seq {
            return Ok(snapshot.as_of_sequence);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "snapshot did not reach seq {min_seq}; got {}",
                snapshot.as_of_sequence
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn build_core(tmp: &TempDir, query_text: &str) -> Result<(DrasiLib, MockSourceHandle)> {
    let (mock_source, handle) = MockSource::new(SOURCE_ID)?;
    let rocks: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(tmp.path(), false, false));
    let core = DrasiLib::builder()
        .with_id("query-output-wipe-e2e")
        .with_source(mock_source)
        .with_query(persistent_query(query_text))
        .with_index_provider("rocks", rocks)
        .build()
        .await?;
    core.start().await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;
    Ok((core, handle))
}

#[tokio::test]
async fn update_query_text_wipes_output_in_process() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    insert_person(&handle, "p3", "Carol", 40).await?;
    assert_eq!(wait_for_seq(&core, 3).await?, 3);

    let query = core
        .query_manager()
        .get_query_instance(QUERY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let before = query.fetch_outbox(0).await?;
    assert_eq!(
        before
            .results
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let hash_v1 = before.config_hash;

    core.update_query(QUERY_ID, persistent_query(QUERY_V2))
        .await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;

    let query = core
        .query_manager()
        .get_query_instance(QUERY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(snapshot.as_of_sequence, 0);
    assert!(
        snapshot.is_empty(),
        "snapshot must not contain old-config rows: {:?}",
        snapshot.to_vec()
    );
    assert_ne!(snapshot.config_hash, hash_v1);
    let outbox = query.fetch_outbox(0).await?;
    assert!(
        outbox.results.is_empty(),
        "fetch_outbox(0) must not return old-config QueryResults"
    );
    assert_eq!(outbox.latest_sequence, 0);

    insert_person(&handle, "p4", "Dana", 22).await?;
    assert_eq!(wait_for_seq(&core, 1).await?, 1);
    let after = query.fetch_outbox(0).await?;
    assert_eq!(
        after.results.iter().map(|r| r.sequence).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(after.config_hash, snapshot.config_hash);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn delete_and_recreate_same_query_id_starts_at_sequence_zero() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    assert_eq!(wait_for_seq(&core, 2).await?, 2);

    core.remove_query(QUERY_ID).await?;
    core.add_query(persistent_query(QUERY_V1)).await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;

    let query = core
        .query_manager()
        .get_query_instance(QUERY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(snapshot.as_of_sequence, 0);
    assert!(snapshot.is_empty());
    let outbox = query.fetch_outbox(0).await?;
    assert!(outbox.results.is_empty());
    assert_eq!(outbox.latest_sequence, 0);

    insert_person(&handle, "p3", "Carol", 40).await?;
    assert_eq!(wait_for_seq(&core, 1).await?, 1);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn stop_query_same_config_preserves_output() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    assert_eq!(wait_for_seq(&core, 2).await?, 2);

    core.stop_query(QUERY_ID).await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Stopped).await?;
    core.start_query(QUERY_ID).await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;

    let query = core
        .query_manager()
        .get_query_instance(QUERY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(snapshot.as_of_sequence, 2);
    assert_eq!(snapshot.len(), 2);
    let outbox = query.fetch_outbox(0).await?;
    assert_eq!(
        outbox
            .results
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    insert_person(&handle, "p3", "Carol", 40).await?;
    assert_eq!(wait_for_seq(&core, 3).await?, 3);

    core.stop().await?;
    Ok(())
}
