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

//! Terminal shutdown releases runtime ownership so the same state-store file can
//! be reopened immediately in the same process, while `stop()` keeps components
//! restartable.
//!
//! Every fixture reaction keeps a processing worker that owns a clone of its
//! `ReactionBase` (and therefore the redb state-store handle). Reopening the redb
//! file without a drain or sleep proves that worker was joined, not detached.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::lifecycle::LifecycleError;
use drasi_core::interface::StorePartition;
use drasi_lib::channels::ComponentStatus;
use drasi_lib::{
    ComponentName, DrasiLib, Reaction, ReactionBase, ReactionBaseParams, ReactionRuntimeContext,
    StateStoreProvider,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

const REACTION: &str = "shutdown-fixture";

fn fixture() -> StorePartition {
    StorePartition::named("fixture")
}

fn reaction_name() -> ComponentName {
    ComponentName::try_new(REACTION).expect("fixture component name")
}

#[derive(Default)]
struct Controls {
    fail_start: AtomicBool,
    fail_stop: AtomicBool,
    fail_stop_once_retryable: AtomicBool,
    hold_stop: AtomicBool,
    stop_entered: Notify,
    stop_release: Notify,
    starts: AtomicUsize,
    stops: AtomicUsize,
}

struct FixtureReaction {
    base: ReactionBase,
    controls: Arc<Controls>,
}

#[async_trait]
impl Reaction for FixtureReaction {
    fn id(&self) -> &drasi_lib::ComponentId {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "shutdown-fixture"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_inputs(&self) -> Vec<drasi_lib::config::ComponentReference> {
        Vec::new()
    }

    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn start(&self) -> Result<()> {
        self.controls.starts.fetch_add(1, Ordering::SeqCst);
        // The worker ignores the shutdown signal until stop_common aborts it,
        // and owns the base (and its state-store handle) until it is joined.
        let base = self.base.clone_shared();
        self.base
            .set_processing_task(tokio::spawn(async move {
                std::future::pending::<()>().await;
                drop(base);
            }))
            .await;
        self.base.set_status(ComponentStatus::Running, None).await;
        anyhow::ensure!(
            !self.controls.fail_start.load(Ordering::SeqCst),
            "injected startup failure"
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.controls.stops.fetch_add(1, Ordering::SeqCst);
        if self.controls.hold_stop.load(Ordering::SeqCst) {
            let release = self.controls.stop_release.notified();
            self.controls.stop_entered.notify_one();
            release.await;
        }
        self.base.stop_common().await?;
        anyhow::ensure!(
            !self.controls.fail_stop.load(Ordering::SeqCst),
            "injected stop failure"
        );
        if self
            .controls
            .fail_stop_once_retryable
            .swap(false, Ordering::SeqCst)
        {
            return Err(LifecycleError::retryable(anyhow::anyhow!(
                "injected retryable stop failure"
            ))
            .into());
        }
        Ok(())
    }
}

async fn seeded_store(path: &Path, value: Vec<u8>) -> Result<Arc<RedbStateStoreProvider>> {
    let store = Arc::new(RedbStateStoreProvider::new(path)?);
    store.set(&fixture(), "retained", value).await?;
    Ok(store)
}

async fn build(store: Arc<RedbStateStoreProvider>, controls: Arc<Controls>) -> Result<DrasiLib> {
    Ok(lib_integration_tests::query_builder()
        .with_state_store_provider(store)
        .with_reaction_factory(reaction_name(), Vec::new(), true, move |id, _| {
            Ok(FixtureReaction {
                base: ReactionBase::new(ReactionBaseParams::new(id, Vec::new())),
                controls,
            })
        })
        .build()
        .await?)
}

/// Reopens the redb file (which fails while any owner still holds it), checks
/// the retained value, and proves the reopened store is writable.
async fn assert_reopens(path: &Path, retained: &[u8]) -> Result<()> {
    let store = RedbStateStoreProvider::new(path)?;
    assert_eq!(
        store.get(&fixture(), "retained").await?.as_deref(),
        Some(retained)
    );
    store.set(&fixture(), "after-shutdown", vec![42]).await?;
    assert_eq!(
        store.get(&fixture(), "after-shutdown").await?,
        Some(vec![42])
    );
    Ok(())
}

async fn wait_for_reaction(core: &DrasiLib, expected: ComponentStatus) -> Result<()> {
    let mut events = core.subscribe_all_component_events();
    let id = core.component_by_name(&reaction_name())?.id().clone();
    if core.get_reaction_status_by_name(&reaction_name()).await? == expected {
        return Ok(());
    }
    timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await?;
            if event.component_id == id && event.status == expected {
                return Ok(());
            }
        }
    })
    .await?
}

#[tokio::test(flavor = "current_thread")]
async fn empty_engine_shutdown_releases_state_store() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let core = lib_integration_tests::query_builder()
        .with_state_store_provider(seeded_store(&path, vec![1, 2, 3]).await?)
        .build()
        .await?;
    core.start().await?;
    timeout(Duration::from_secs(10), core.shutdown()).await??;
    core.shutdown().await?;
    assert!(core.start().await.is_err());
    drop(core);

    // No drain or sleep: terminal teardown must finish before returning.
    assert_reopens(&path, &[1, 2, 3]).await?;
    let core = lib_integration_tests::query_builder()
        .with_state_store_provider(Arc::new(RedbStateStoreProvider::new(&path)?))
        .build()
        .await?;
    core.start().await?;
    core.shutdown().await?;
    drop(core);
    assert!(
        RedbStateStoreProvider::new(&path)?
            .contains_key(&fixture(), "after-shutdown")
            .await?
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_after_partial_start_stops_tasks_and_releases_state() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let controls = Arc::new(Controls::default());
    controls.fail_start.store(true, Ordering::SeqCst);
    let core = build(seeded_store(&path, vec![6]).await?, controls.clone()).await?;
    // The reaction spawned its worker before failing start.
    assert!(core.start().await.is_err());
    assert_eq!(controls.starts.load(Ordering::SeqCst), 1);
    timeout(Duration::from_secs(10), core.shutdown()).await??;
    assert!(!core.is_running().await);
    assert!(core.start().await.is_err());
    assert_eq!(
        Arc::strong_count(&controls),
        1,
        "shutdown after a partial start must release the reaction runtime"
    );
    drop(core);
    assert_reopens(&path, &[6]).await
}

/// A terminal stop failure is reported, and is never reported as success on a
/// later call, but shutdown still completes the rest of its cleanup so the
/// state store is released once the instance is dropped.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_reports_stop_failure_and_still_releases_state() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let controls = Arc::new(Controls::default());
    controls.fail_stop.store(true, Ordering::SeqCst);
    let core = build(seeded_store(&path, vec![7]).await?, controls.clone()).await?;
    core.start().await?;
    wait_for_reaction(&core, ComponentStatus::Running).await?;

    let result = timeout(Duration::from_secs(10), core.shutdown()).await?;
    assert!(
        result.is_err(),
        "shutdown must report stop failures after completing other cleanup"
    );
    assert!(controls.stops.load(Ordering::SeqCst) >= 1);
    assert!(!core.is_running().await);
    assert!(core.start().await.is_err());
    assert!(
        timeout(Duration::from_secs(10), core.shutdown())
            .await?
            .is_err(),
        "a terminal cleanup failure must not be reported as success on retry"
    );
    drop(core);
    assert_eq!(
        Arc::strong_count(&controls),
        1,
        "dropping a failed instance retained the reaction runtime"
    );
    assert_reopens(&path, &[7]).await
}

/// A retryable stop failure is reported; a later shutdown completes the
/// cleanup and releases the runtime object and the state store.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_retries_failed_stop_before_releasing_runtime() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let controls = Arc::new(Controls::default());
    controls
        .fail_stop_once_retryable
        .store(true, Ordering::SeqCst);
    let core = build(seeded_store(&path, vec![5]).await?, controls.clone()).await?;
    core.start().await?;
    wait_for_reaction(&core, ComponentStatus::Running).await?;

    assert!(timeout(Duration::from_secs(10), core.shutdown())
        .await?
        .is_err());
    // The failed stop was retried at least once (by this call's adapter
    // cleanup or by the next call) before the runtime was released.
    assert!(core.start().await.is_err());
    timeout(Duration::from_secs(10), core.shutdown()).await??;
    let completed_attempts = controls.stops.load(Ordering::SeqCst);
    assert!(
        completed_attempts >= 2,
        "the failed cleanup must be run again"
    );
    assert_eq!(
        Arc::strong_count(&controls),
        1,
        "successful shutdown must release the reaction runtime"
    );
    // Completed shutdown is terminal: another call does no further cleanup.
    core.shutdown().await?;
    assert_eq!(controls.stops.load(Ordering::SeqCst), completed_attempts);
    assert!(core.start().await.is_err());
    drop(core);
    assert_reopens(&path, &[5]).await
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cleans_unstarted_and_individually_started_instances() -> Result<()> {
    for start_reaction in [false, true] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.redb");
        let controls = Arc::new(Controls::default());
        let core = build(seeded_store(&path, vec![9]).await?, controls.clone()).await?;
        if start_reaction {
            core.start_reaction_by_name(&reaction_name()).await?;
            wait_for_reaction(&core, ComponentStatus::Running).await?;
        }
        assert!(!core.is_running().await);
        timeout(Duration::from_secs(10), core.shutdown()).await??;
        assert_eq!(
            controls.stops.load(Ordering::SeqCst),
            usize::from(start_reaction)
        );
        drop(core);
        assert_reopens(&path, &[9]).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_shutdown_waits_for_teardown() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let controls = Arc::new(Controls::default());
    controls.hold_stop.store(true, Ordering::SeqCst);
    let core = build(seeded_store(&path, vec![1]).await?, controls.clone()).await?;
    core.start().await?;

    let mut first = Box::pin(core.shutdown());
    let mut second = Box::pin(core.shutdown());
    tokio::select! {
        result = &mut first => panic!("shutdown returned before teardown: {result:?}"),
        _ = controls.stop_entered.notified() => {}
    }
    // Neither caller may report completion while teardown is still blocked.
    assert!(poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx)))
        .await
        .is_pending());
    assert!(poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx)))
        .await
        .is_pending());
    controls.stop_release.notify_one();
    let (first, second) = timeout(Duration::from_secs(10), async {
        tokio::join!(first, second)
    })
    .await?;
    first?;
    second?;
    assert_eq!(controls.stops.load(Ordering::SeqCst), 1);
    drop(core);
    assert_reopens(&path, &[1]).await
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_shutdown_can_finish_on_retry() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let controls = Arc::new(Controls::default());
    controls.hold_stop.store(true, Ordering::SeqCst);
    let core = build(seeded_store(&path, vec![2]).await?, controls.clone()).await?;
    core.start().await?;

    let mut shutdown = Box::pin(core.shutdown());
    tokio::select! {
        result = &mut shutdown => panic!("shutdown returned before teardown: {result:?}"),
        _ = controls.stop_entered.notified() => {}
    }
    drop(shutdown);
    assert!(core.start().await.is_err());
    controls.stop_release.notify_one();
    timeout(Duration::from_secs(10), core.shutdown()).await??;
    // The retry joins the accepted teardown instead of stopping again.
    assert_eq!(controls.stops.load(Ordering::SeqCst), 1);
    drop(core);
    assert_reopens(&path, &[2]).await
}

#[tokio::test(flavor = "current_thread")]
async fn stop_preserves_restartable_components() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let controls = Arc::new(Controls::default());
    let core = build(seeded_store(&path, vec![3]).await?, controls.clone()).await?;
    core.start().await?;
    wait_for_reaction(&core, ComponentStatus::Running).await?;
    core.stop().await?;
    wait_for_reaction(&core, ComponentStatus::Stopped).await?;
    // stop() must not release the store: a second opener is still refused.
    assert!(RedbStateStoreProvider::new(&path).is_err());

    core.start().await?;
    assert!(core.is_running().await);
    wait_for_reaction(&core, ComponentStatus::Running).await?;
    // The factory runs once, so a second start proves stop kept the same object.
    assert_eq!(controls.starts.load(Ordering::SeqCst), 2);
    assert!(Arc::strong_count(&controls) > 1);
    core.shutdown().await?;
    // Terminal shutdown releases the runtime object itself, not just its tasks.
    assert_eq!(
        Arc::strong_count(&controls),
        1,
        "shutdown retained the reaction runtime"
    );
    assert!(core.start().await.is_err());
    drop(core);
    assert_reopens(&path, &[3]).await
}

/// A retained query-result subscription that is never read backpressures
/// publication, but must not block `stop_query` or terminal shutdown.
#[tokio::test(flavor = "current_thread")]
async fn retained_subscription_does_not_block_query_stop_or_shutdown() -> Result<()> {
    retained_subscription_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_subscription_does_not_block_query_stop_or_shutdown_multi_thread() -> Result<()> {
    retained_subscription_case().await
}

async fn retained_subscription_case() -> Result<()> {
    use drasi_source_application::{
        ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder,
    };

    let query_name = ComponentName::try_new("query").expect("fixture component name");
    for terminal_shutdown in [false, true] {
        let (source, input) = ApplicationSource::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000005d1")
                .expect("fixture UUID"),
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )?;
        let core = lib_integration_tests::query_builder()
            .with_priority_queue_capacity(1)
            .with_dispatch_buffer_capacity(1)
            .with_source(
                ComponentName::try_new("input").expect("fixture component name"),
                source,
            )
            .with_query(
                drasi_lib::Query::cypher(query_name.clone())
                    .query("MATCH (n:Item) RETURN n.value AS value")
                    .from_input_by_name(
                        ComponentName::try_new("input").expect("fixture component name"),
                    )
                    .enable_bootstrap(false)
                    .auto_start(true)
                    .with_dispatch_mode(drasi_lib::DispatchMode::Channel)
                    .with_dispatch_buffer_capacity(1)
                    .build()?,
            )
            .build()
            .await?;
        core.start().await?;
        timeout(
            Duration::from_secs(10),
            core.component_by_name(&query_name)?.wait_started(),
        )
        .await??;
        let retained = core.subscribe_query_results_by_name(&query_name).await?;
        let producer = tokio::spawn(async move {
            for value in 0..32_i64 {
                let properties = PropertyMapBuilder::new()
                    .with_integer("value", value)
                    .build();
                if input
                    .send_node_insert(value.to_string(), vec!["Item"], properties)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        // Let publication fill the unread subscriber's bounded buffer.
        assert!(
            timeout(Duration::from_millis(200), async {
                while core.get_query_results_by_name(&query_name).await?.len() < 32 {
                    tokio::task::yield_now().await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .is_err(),
            "an unread bounded subscription must backpressure publication"
        );

        if terminal_shutdown {
            timeout(Duration::from_secs(10), core.shutdown())
                .await
                .context("retained subscription blocked shutdown")??;
        } else {
            timeout(
                Duration::from_secs(10),
                core.stop_query_by_name(&query_name),
            )
            .await
            .context("retained subscription blocked stop_query")??;
            timeout(Duration::from_secs(10), core.shutdown()).await??;
        }
        producer.abort();
        let _ = producer.await;
        drop(retained);
    }
    Ok(())
}
