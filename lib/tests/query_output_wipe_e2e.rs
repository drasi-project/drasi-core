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
    let mut query = Query::cypher(
        drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .query(text)
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("test-source").expect("fixture component name"),
    )
    .auto_start(true)
    .enable_bootstrap(false)
    .with_outbox_capacity(100)
    .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
    .build()
    .expect("valid persistent query");
    query.id = Some(
        drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000002b")
            .expect("explicit persistent owner UUID"),
    );
    query
}

async fn insert_person(handle: &MockSourceHandle, id: &str, name: &str, age: i64) -> Result<()> {
    let props = PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", age)
        .build();
    handle.send_node_insert(id, vec!["Person"], props).await
}

async fn wait_for_status(
    core: &DrasiLib,
    id: &drasi_core_types::ComponentName,
    expected: ComponentStatus,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if core.get_query_status_by_name(id).await? == expected {
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
            .get_query_instance_by_name(
                &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            )
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
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id("query-output-wipe-e2e")
        .with_source(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
            mock_source,
        )
        .with_query(persistent_query(query_text))
        .with_index_provider("rocks", rocks)
        .build()
        .await?;
    core.start().await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;
    Ok((core, handle))
}

#[tokio::test]
async fn unassessed_query_change_preserves_output_in_process() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    insert_person(&handle, "p3", "Carol", 40).await?;
    assert_eq!(wait_for_seq(&core, 3).await?, 3);

    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
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

    let error = core
        .update_query_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            persistent_query(QUERY_V2),
        )
        .await
        .expect_err("the registered factory does not assess semantic updates");
    assert!(format!("{error:#}").contains("does not support configuration updates"));
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;

    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(snapshot.as_of_sequence, 3);
    assert_eq!(snapshot.len(), 3, "rejection must retain committed rows");
    assert_eq!(snapshot.config_hash, hash_v1);
    assert_eq!(query.fetch_outbox(0).await?.results.len(), 3);
    let outbox = query.fetch_outbox(3).await?;
    assert!(
        outbox.results.is_empty(),
        "no new result is produced by a rejected configuration"
    );
    assert_eq!(outbox.latest_sequence, 3);

    insert_person(&handle, "p4", "Dana", 22).await?;
    assert_eq!(wait_for_seq(&core, 4).await?, 4);
    let after = query.fetch_outbox(3).await?;
    assert_eq!(
        after.results.iter().map(|r| r.sequence).collect::<Vec<_>>(),
        vec![4]
    );
    assert_eq!(after.config_hash, snapshot.config_hash);

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn delete_and_recreate_same_query_id_starts_at_sequence_zero() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    assert_eq!(wait_for_seq(&core, 2).await?, 2);

    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    core.add_query(persistent_query(QUERY_V1)).await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;

    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
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

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn delete_and_recreate_advances_output_generation() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    assert_eq!(wait_for_seq(&core, 1).await?, 1);
    let gen_before = {
        let query = core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        query.fetch_snapshot().await?.output_generation
    };

    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    core.add_query(persistent_query(QUERY_V1)).await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;

    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    assert!(
        snapshot.output_generation > gen_before,
        "recreate must advance past persisted generation {gen_before}, got {}",
        snapshot.output_generation
    );

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stop_query_same_config_preserves_output() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    assert_eq!(wait_for_seq(&core, 2).await?, 2);

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await?;
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;

    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
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

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejected_change_followed_by_unchanged_query_does_not_lock() -> Result<()> {
    let tmp = TempDir::new()?;
    let (core, handle) = build_core(&tmp, QUERY_V1).await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    assert_eq!(wait_for_seq(&core, 1).await?, 1);

    let error = core
        .update_query_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            persistent_query(QUERY_V2),
        )
        .await
        .expect_err("semantic change needs a factory assessment");
    assert!(format!("{error:#}").contains("does not support configuration updates"));
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;
    let unchanged = core
        .get_query_config_by_name(&drasi_lib::ComponentName::try_new("q1")?)
        .await?
        .into_authoring();
    core.update_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        unchanged,
    )
    .await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;

    insert_person(&handle, "p2", "Bob", 25).await?;
    assert_eq!(wait_for_seq(&core, 2).await?, 2);

    core.shutdown().await?;
    Ok(())
}

fn volatile_query(text: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"))
        .query(text)
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(true)
        .enable_bootstrap(false)
        .with_outbox_capacity(100)
        .build()
        .expect("valid volatile query")
}

#[tokio::test]
async fn volatile_recreation_and_removal_succeed() -> Result<()> {
    let (mock_source, handle) = MockSource::new(SOURCE_ID)?;
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id("query-output-wipe-volatile")
        .with_source(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
            mock_source,
        )
        .with_query(volatile_query(QUERY_V1))
        .build()
        .await?;
    core.start().await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    assert_eq!(wait_for_seq(&core, 2).await?, 2);

    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    core.add_query(volatile_query(QUERY_V2)).await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(snapshot.as_of_sequence, 0);
    assert!(snapshot.is_empty());

    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    core.add_query(volatile_query(QUERY_V1)).await?;
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await?;
    insert_person(&handle, "p3", "Carol", 40).await?;
    assert_eq!(wait_for_seq(&core, 1).await?, 1);

    core.shutdown().await?;
    Ok(())
}
