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
use drasi_lib::channels::{ComponentEvent, ComponentStatus, QueryResult};
use drasi_lib::{DrasiLib, MemoryStateStoreProvider, Query, QueryConfig, Reaction, StateStoreProvider};
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
        store_id: &str,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store_id, key).await
    }

    async fn set(
        &self,
        store_id: &str,
        key: &str,
        value: Vec<u8>,
    ) -> drasi_lib::state_store::StateStoreResult<()> {
        self.inner.set(store_id, key, value).await
    }

    async fn delete(
        &self,
        store_id: &str,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(
        &self,
        store_id: &str,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> drasi_lib::state_store::StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(
        &self,
        store_id: &str,
        entries: &[(&str, &[u8])],
    ) -> drasi_lib::state_store::StateStoreResult<()> {
        self.inner.set_many(store_id, entries).await
    }

    async fn delete_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(&self, store_id: &str) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(
        &self,
        store_id: &str,
    ) -> drasi_lib::state_store::StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(&self, store_id: &str) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(&self, store_id: &str) -> drasi_lib::state_store::StateStoreResult<usize> {
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
    reaction: R,
) -> Result<(
    Arc<DrasiLib>,
    MockSourceHandle,
    Arc<DurableMemoryStateStoreProvider>,
)>
where
    R: Reaction + 'static,
{
    let source_id = format!("{test_id}-source");
    let (mock_source, handle) = crate::mock_source::MockSource::new(source_id)?;

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id(test_id)
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store.clone())
            .build()
            .await?,
    );

    Ok((core, handle, state_store))
}

/// Build the standard Person query used by the recovery test scenarios.
pub fn person_query(source_id: &str, outbox_capacity: usize) -> QueryConfig {
    Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source(source_id)
        .with_outbox_capacity(outbox_capacity)
        .auto_start(true)
        .build()
}

/// Poll the state store until a checkpoint exists for the given reaction+query pair.
pub async fn wait_for_checkpoint(
    state_store: &DurableMemoryStateStoreProvider,
    reaction_id: &str,
    query_id: &str,
    timeout_duration: Duration,
) -> Result<()> {
    let key = format!("checkpoint:{query_id}");
    let deadline = tokio::time::Instant::now() + timeout_duration;
    loop {
        if state_store.contains_key(reaction_id, &key).await? {
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
    component_id: &str,
    expected_status: ComponentStatus,
    timeout_duration: Duration,
) -> Result<ComponentEvent> {
    let description = format!("component '{component_id}' to reach {expected_status:?}");
    match timeout(timeout_duration, async {
        loop {
            match event_rx.recv().await {
                Ok(event)
                    if event.component_id == component_id && event.status == expected_status =>
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
    component_id: &str,
    quiet_period: Duration,
) -> Result<()> {
    match timeout(quiet_period, async {
        loop {
            match event_rx.recv().await {
                Ok(event)
                    if event.component_id == component_id
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
pub async fn stop_reaction_and_wait(core: &DrasiLib, id: &str) -> Result<()> {
    core.stop_reaction(id).await?;
    for _ in 0..50 {
        let statuses = core.list_reactions().await?;
        if let Some((_, status)) = statuses.iter().find(|(rid, _)| rid == id) {
            if *status == ComponentStatus::Stopped {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let final_status = core
        .list_reactions()
        .await?
        .into_iter()
        .find(|(rid, _)| rid == id)
        .map(|(_, s)| s);
    anyhow::bail!(
        "Reaction {id} did not reach Stopped state within timeout (last status: {final_status:?})"
    );
}

/// Assert that a reaction is in the Running state.
pub async fn assert_reaction_running(core: &DrasiLib, reaction_id: &str) -> Result<()> {
    assert_eq!(
        core.get_reaction_status(reaction_id).await?,
        ComponentStatus::Running,
        "Reaction {reaction_id} should be running"
    );
    Ok(())
}

/// Exercise the AutoSkipGap recovery scenario: stop reaction, overflow outbox, restart, verify it resumes.
pub async fn exercise_autoskipgap_recovery<R>(
    test_id: &str,
    reaction_id: &str,
    reaction: R,
) -> Result<()>
where
    R: Reaction + 'static,
{
    let query = person_query(&format!("{test_id}-source"), 2);
    let (core, handle, state_store) = build_core_with_reaction(test_id, query, reaction).await?;
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        reaction_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    wait_for_checkpoint(&state_store, reaction_id, "q1", Duration::from_secs(5)).await?;

    stop_reaction_and_wait(&core, reaction_id).await?;

    for i in 0..5 {
        insert_person(
            &handle,
            &format!("gap-{i}"),
            &format!("Gap {i}"),
            40 + i as i64,
        )
        .await?;
    }

    core.start_reaction(reaction_id).await?;
    wait_for_component_status(
        &mut event_rx,
        reaction_id,
        ComponentStatus::Running,
        Duration::from_secs(5),
    )
    .await?;

    insert_person(&handle, "live", "Live", 99).await?;
    // Verify no error event arrives after processing the live event,
    // then confirm the reaction is still in the Running state.
    assert_no_error_event(&mut event_rx, reaction_id, Duration::from_millis(500)).await?;
    assert_reaction_running(&core, reaction_id).await?;

    core.stop().await?;
    Ok(())
}

/// Exercise the Strict recovery gap-failure scenario: stop reaction, overflow outbox, verify restart fails.
pub async fn exercise_strict_gap_failure<R>(
    test_id: &str,
    reaction_id: &str,
    reaction: R,
) -> Result<()>
where
    R: Reaction + 'static,
{
    let query = person_query(&format!("{test_id}-source"), 2);
    let (core, handle, state_store) = build_core_with_reaction(test_id, query, reaction).await?;
    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;
    wait_for_component_status(
        &mut event_rx,
        reaction_id,
        ComponentStatus::Running,
        Duration::from_secs(10),
    )
    .await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    wait_for_checkpoint(&state_store, reaction_id, "q1", Duration::from_secs(5)).await?;

    stop_reaction_and_wait(&core, reaction_id).await?;

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
    let _restart_result = core.start_reaction(reaction_id).await;

    wait_for_component_status(
        &mut event_rx,
        reaction_id,
        ComponentStatus::Error,
        Duration::from_secs(5),
    )
    .await?;

    core.stop().await?;
    Ok(())
}
