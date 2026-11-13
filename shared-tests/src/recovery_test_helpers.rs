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

//! Shared test helpers for reaction recovery E2E tests.

use anyhow::{anyhow, bail, Result};
use drasi_core_types::StorePartition;
use drasi_lib::channels::{ComponentEvent, ComponentStatus, QueryResult};
use drasi_lib::{
    config::{ComponentId, ComponentName},
    DrasiLib, MemoryStateStoreProvider, Query, QueryConfig, Reaction, StateStoreProvider,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::mock_source::{MockSourceHandle, PropertyMapBuilder};

/// A durable wrapper around `MemoryStateStoreProvider` that reports `is_durable() = true`.
pub struct DurableMemoryStateStoreProvider {
    inner: MemoryStateStoreProvider,
}

impl DurableMemoryStateStoreProvider {
    /// Creates a new `DurableMemoryStateStoreProvider` backed by an in-memory store.
    pub fn new() -> Self {
        Self {
            inner: MemoryStateStoreProvider::new(),
        }
    }
}

impl Default for DurableMemoryStateStoreProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl drasi_lib::state_store::StateStoreProvider for DurableMemoryStateStoreProvider {
    async fn get(
        &self,
        store_id: &StorePartition,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store_id, key).await
    }

    async fn set(
        &self,
        store_id: &StorePartition,
        key: &str,
        value: Vec<u8>,
    ) -> drasi_lib::state_store::StateStoreResult<()> {
        self.inner.set(store_id, key, value).await
    }

    async fn delete(
        &self,
        store_id: &StorePartition,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(
        &self,
        store_id: &StorePartition,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &StorePartition,
        keys: &[&str],
    ) -> drasi_lib::state_store::StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(
        &self,
        store_id: &StorePartition,
        entries: &[(&str, &[u8])],
    ) -> drasi_lib::state_store::StateStoreResult<()> {
        self.inner.set_many(store_id, entries).await
    }

    async fn delete_many(
        &self,
        store_id: &StorePartition,
        keys: &[&str],
    ) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(
        &self,
        store_id: &StorePartition,
    ) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(
        &self,
        store_id: &StorePartition,
    ) -> drasi_lib::state_store::StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(
        &self,
        store_id: &StorePartition,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(
        &self,
        store_id: &StorePartition,
    ) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.key_count(store_id).await
    }

    fn is_durable(&self) -> bool {
        true
    }
}

/// Build a `DrasiLib` instance wired to a `MockSource`, the given query, reaction, and a durable state store.
///
/// Returns the core, source handle, and state store (for checkpoint polling).
pub async fn build_core_with_reaction<R>(
    test_id: &str,
    query: QueryConfig,
    reaction_name: &ComponentName,
    reaction: R,
) -> Result<(
    Arc<DrasiLib>,
    MockSourceHandle,
    Arc<DurableMemoryStateStoreProvider>,
)>
where
    R: Reaction + 'static,
{
    build_core_with_reaction_queries(test_id, vec![query], reaction_name, reaction).await
}

/// Build a recovery fixture with every query declared before the reaction is admitted.
pub async fn build_core_with_reaction_queries<R>(
    test_id: &str,
    queries: Vec<QueryConfig>,
    reaction_name: &ComponentName,
    reaction: R,
) -> Result<(
    Arc<DrasiLib>,
    MockSourceHandle,
    Arc<DurableMemoryStateStoreProvider>,
)>
where
    R: Reaction + 'static,
{
    let source_name = ComponentName::try_new(format!("{test_id}-source"))?;
    let source_id = ComponentId::generate();
    let (mock_source, handle) = crate::mock_source::MockSource::new(source_id)?;

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let mut builder = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_host_sdk::computation::NativePlugin::from_definition(
                drasi_computation_transformer_query::plugin()?,
            )?
            .query_factory()?,
        )
        .with_id(test_id)
        .with_source(source_name, mock_source)
        .with_state_store_provider(state_store.clone());
    for query in queries {
        builder = builder.with_query(query);
    }
    let core = Arc::new(
        builder
            .with_reaction(reaction_name.clone(), reaction)
            .build()
            .await?,
    );

    Ok((core, handle, state_store))
}

/// Build the standard Person query used by the recovery test scenarios.
pub fn person_query(source_name: &ComponentName, outbox_capacity: usize) -> QueryConfig {
    Query::cypher(ComponentName::try_new("q1").expect("fixed fixture name"))
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_input_by_name(source_name.clone())
        .with_outbox_capacity(outbox_capacity)
        .auto_start(true)
        .build()
        .expect("shared recovery query must be valid")
}

/// Poll the state store until a checkpoint exists for the given reaction+query pair.
pub async fn wait_for_checkpoint(
    state_store: &DurableMemoryStateStoreProvider,
    reaction_id: &ComponentId,
    query_id: &ComponentId,
    timeout_duration: Duration,
) -> Result<()> {
    let key = format!("checkpoint:{query_id}");
    let deadline = tokio::time::Instant::now() + timeout_duration;
    loop {
        if state_store
            .contains_key(&StorePartition::component(reaction_id.clone()), &key)
            .await?
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "Timed out ({timeout_duration:?}) waiting for checkpoint '{key}' on reaction '{reaction_id}'"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until a component emits the expected status event.
pub async fn wait_for_component_status(
    event_rx: &mut broadcast::Receiver<ComponentEvent>,
    component_id: &ComponentId,
    expected_status: ComponentStatus,
    timeout_duration: Duration,
) -> Result<ComponentEvent> {
    let description = format!("component '{component_id}' to reach {expected_status:?}");
    match timeout(timeout_duration, async {
        loop {
            match event_rx.recv().await {
                Ok(event)
                    if &event.component_id == component_id && event.status == expected_status =>
                {
                    return Ok(event);
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("Event channel closed while waiting for {description}")
                }
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => bail!("Timed out ({timeout_duration:?}) waiting for {description}"),
    }
}

/// Assert that no error event arrives for a component during a quiet period.
pub async fn assert_no_error_event(
    event_rx: &mut broadcast::Receiver<ComponentEvent>,
    component_id: &ComponentId,
    quiet_period: Duration,
) -> Result<()> {
    match timeout(quiet_period, async {
        loop {
            match event_rx.recv().await {
                Ok(event)
                    if &event.component_id == component_id
                        && event.status == ComponentStatus::Error =>
                {
                    return Err(anyhow!(
                        "Reaction {component_id} entered Error state: {}",
                        event.message.unwrap_or_else(|| "no message".to_string())
                    ));
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!(
                        "Event channel closed unexpectedly while monitoring {component_id}"
                    ));
                }
            }
        }
    })
    .await
    {
        Ok(Err(e)) => Err(e),
        Ok(Ok(())) => Ok(()), // unreachable, but keeps the compiler happy
        Err(_) => Ok(()),     // timeout expired with no error event — success
    }
}

/// Insert a Person node via the mock source handle.
pub async fn insert_person(
    handle: &MockSourceHandle,
    id: &str,
    name: &str,
    age: i64,
) -> Result<()> {
    let props = PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", age)
        .build();
    handle.send_node_insert(id, vec!["Person"], props).await
}

/// Stop a reaction and poll until it reaches the Stopped state.
pub async fn stop_reaction_and_wait(core: &DrasiLib, id: &ComponentId) -> Result<()> {
    core.stop_reaction_by_id(id).await?;
    let mut final_status = None;
    for _ in 0..50 {
        if let Ok(status) = core.get_reaction_status_by_id(id).await {
            if status == ComponentStatus::Stopped {
                return Ok(());
            }
            final_status = Some(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!(
        "Reaction {id} did not reach Stopped state within timeout (last status: {final_status:?})"
    );
}

/// Assert that a reaction is in the Running state.
pub async fn assert_reaction_running(core: &DrasiLib, reaction_id: &ComponentId) -> Result<()> {
    assert_eq!(
        core.get_reaction_status_by_id(reaction_id).await?,
        ComponentStatus::Running,
        "Reaction {reaction_id} should be running"
    );
    Ok(())
}

/// Exercise the AutoSkipGap recovery scenario: stop reaction, overflow outbox, restart, verify it resumes.
pub async fn exercise_autoskipgap_recovery<R>(
    test_id: &str,
    reaction_name: &ComponentName,
    reaction: R,
) -> Result<()>
where
    R: Reaction + 'static,
{
    let query = person_query(&ComponentName::try_new(format!("{test_id}-source"))?, 2);
    let (core, handle, state_store) =
        build_core_with_reaction(test_id, query, reaction_name, reaction).await?;
    let reaction_component_id = core.component_by_name(reaction_name)?.id().clone();
    let query_id = core
        .component_by_name(&ComponentName::try_new("q1")?)?
        .id()
        .clone();
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    let query = core
        .query_manager()
        .get_query_instance_by_id(&query_id)
        .await?;
    let startup = query.fetch_snapshot().await?;
    let startup_checkpoint = drasi_lib::ReactionCheckpoint {
        sequence: startup.as_of_sequence,
        config_hash: startup.config_hash,
    };
    autoskipgap_wait_checkpoint(
        &core,
        state_store.clone(),
        &reaction_component_id,
        &query_id,
        &startup_checkpoint,
    )
    .await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    let mut expected = vec![("Alice".to_string(), 30)];
    let first = autoskipgap_wait_rows(query.as_ref(), &expected).await?;
    assert!(first.as_of_sequence > startup.as_of_sequence);
    assert_eq!(first.config_hash, startup.config_hash);
    let first_checkpoint = drasi_lib::ReactionCheckpoint {
        sequence: first.as_of_sequence,
        config_hash: first.config_hash,
    };
    autoskipgap_wait_checkpoint(
        &core,
        state_store.clone(),
        &reaction_component_id,
        &query_id,
        &first_checkpoint,
    )
    .await?;

    stop_reaction_and_wait(&core, &reaction_component_id).await?;

    for i in 0..5 {
        let name = format!("Gap {i}");
        let age = 40 + i as i64;
        insert_person(&handle, &format!("gap-{i}"), &name, age).await?;
        expected.push((name, age));
    }
    let gap_head = autoskipgap_wait_rows(query.as_ref(), &expected).await?;
    assert!(gap_head.as_of_sequence >= first.as_of_sequence + 5);
    assert_eq!(gap_head.config_hash, first.config_hash);
    let gap = match query.fetch_outbox(first_checkpoint.sequence).await {
        Err(drasi_lib::queries::output_state::FetchError::OutboxGap(gap)) => gap,
        Err(error) => return Err(error.into()),
        Ok(outbox) => bail!(
            "Expected an actual capacity-2 outbox gap after five committed inputs; retained {} results",
            outbox.results.len()
        ),
    };
    assert_eq!(gap.requested, first_checkpoint.sequence);
    assert!(gap.earliest_available > first_checkpoint.sequence + 1);
    assert_eq!(gap.latest_sequence, gap_head.as_of_sequence);
    assert_eq!(gap.config_hash, gap_head.config_hash);
    autoskipgap_wait_checkpoint(
        &core,
        state_store.clone(),
        &reaction_component_id,
        &query_id,
        &first_checkpoint,
    )
    .await?;

    core.start_reaction_by_id(&reaction_component_id).await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    let skipped_checkpoint = drasi_lib::ReactionCheckpoint {
        sequence: gap_head.as_of_sequence,
        config_hash: gap_head.config_hash,
    };
    autoskipgap_wait_checkpoint(
        &core,
        state_store.clone(),
        &reaction_component_id,
        &query_id,
        &skipped_checkpoint,
    )
    .await?;

    insert_person(&handle, "live", "Live", 99).await?;
    expected.push(("Live".to_string(), 99));
    let live = autoskipgap_wait_rows(query.as_ref(), &expected).await?;
    assert!(live.as_of_sequence > skipped_checkpoint.sequence);
    assert_eq!(live.config_hash, skipped_checkpoint.config_hash);
    // Bundled plugins acknowledge acceptance, not completion of external effects.
    autoskipgap_wait_checkpoint(
        &core,
        state_store,
        &reaction_component_id,
        &query_id,
        &drasi_lib::ReactionCheckpoint {
            sequence: live.as_of_sequence,
            config_hash: live.config_hash,
        },
    )
    .await?;
    assert_no_error_event(
        &mut event_rx,
        &reaction_component_id,
        Duration::from_millis(500),
    )
    .await?;
    assert_reaction_running(&core, &reaction_component_id).await?;

    let reaction = core
        .reaction_manager()
        .get_reaction_instance_by_id(&reaction_component_id)
        .await?;
    timeout(Duration::from_secs(10), core.shutdown()).await??;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    assert!(!core.is_running().await);
    assert!(core.start().await.is_err(), "Shutdown must be permanent");
    Ok(())
}

async fn autoskipgap_wait_rows(
    query: &dyn drasi_lib::queries::Query,
    expected: &[(String, i64)],
) -> Result<drasi_lib::queries::output_state::SnapshotResponse> {
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = query.fetch_snapshot().await?;
            let rows = snapshot.to_vec();
            if rows.len() == expected.len()
                && expected.iter().all(|(name, age)| {
                    rows.iter().any(|row| {
                        row["name"].as_str() == Some(name.as_str())
                            && row["age"].as_i64() == Some(*age)
                    })
                })
            {
                return Ok(snapshot);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("Timed out waiting for committed Person rows {expected:?}"))?
}

async fn autoskipgap_wait_checkpoint(
    core: &DrasiLib,
    state_store: Arc<DurableMemoryStateStoreProvider>,
    reaction_id: &ComponentId,
    query_id: &ComponentId,
    expected: &drasi_lib::ReactionCheckpoint,
) -> Result<()> {
    timeout(Duration::from_secs(5), async {
        loop {
            // Use the public in-process checkpoint reader, not a copied storage codec.
            let context = drasi_lib::reactions::bootstrap_context::BootstrapContext::new(
                query_id.clone(),
                false,
                core.query_manager().get_query_instance_by_id(query_id).await?,
                reaction_id.clone(),
                Some(state_store.clone()),
            );
            if let Some(actual) = context.read_checkpoint().await? {
                if &actual == expected {
                    return Ok(());
                }
                if actual.sequence > expected.sequence || actual.config_hash != expected.config_hash {
                    bail!("Unexpected checkpoint for {reaction_id}/{query_id}: {actual:?}; expected {expected:?}");
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("Timed out waiting for {reaction_id}/{query_id} checkpoint {expected:?}"))?
}

/// Exercise the Strict recovery gap-failure scenario: stop reaction, overflow outbox, verify restart fails.
pub async fn exercise_strict_gap_failure<R>(
    test_id: &str,
    reaction_name: &ComponentName,
    reaction: R,
) -> Result<()>
where
    R: Reaction + 'static,
{
    let query = person_query(&ComponentName::try_new(format!("{test_id}-source"))?, 2);
    let (core, handle, state_store) =
        build_core_with_reaction(test_id, query, reaction_name, reaction).await?;
    let reaction_component_id = core.component_by_name(reaction_name)?.id().clone();
    let query_id = core
        .component_by_name(&ComponentName::try_new("q1")?)?
        .id()
        .clone();
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Running,
        Duration::from_secs(10),
    )
    .await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    wait_for_checkpoint(
        &state_store,
        &reaction_component_id,
        &query_id,
        Duration::from_secs(5),
    )
    .await?;

    stop_reaction_and_wait(&core, &reaction_component_id).await?;

    for i in 0..5 {
        insert_person(
            &handle,
            &format!("strict-gap-{i}"),
            &format!("Strict Gap {i}"),
            50 + i as i64,
        )
        .await?;
    }

    // start_reaction may return Err synchronously (current behavior) or Ok(())
    // with an async transition to Error. Either way, we verify the reaction
    // reaches Error status.
    let _restart_result = core.start_reaction_by_id(&reaction_component_id).await;

    wait_for_component_status(
        &mut event_rx,
        &reaction_component_id,
        ComponentStatus::Error,
        Duration::from_secs(5),
    )
    .await?;

    core.stop().await?;
    Ok(())
}
