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

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::ComponentStatus;
use drasi_lib::{
    DrasiLib, Reaction, ReactionBase, ReactionBaseParams, ReactionRuntimeContext,
    StateStoreProvider,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn empty_engine_shutdown_releases_state_store() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let store = Arc::new(RedbStateStoreProvider::new(&path)?);
    store.set("fixture", "retained", vec![1, 2, 3]).await?;
    let core = DrasiLib::builder()
        .with_state_store_provider(store)
        .build()
        .await?;
    core.start().await?;
    timeout(Duration::from_secs(10), core.shutdown()).await??;
    core.shutdown().await?;
    assert!(core.start().await.is_err());
    drop(core);

    // No drain or sleep: terminal teardown must finish before returning.
    let reopened = Arc::new(RedbStateStoreProvider::new(&path)?);
    assert_eq!(
        reopened.get("fixture", "retained").await?,
        Some(vec![1, 2, 3])
    );
    reopened.set("fixture", "new", vec![4, 5]).await?;
    assert_eq!(reopened.get("fixture", "new").await?, Some(vec![4, 5]));
    let core = DrasiLib::builder()
        .with_state_store_provider(reopened)
        .build()
        .await?;
    core.start().await?;
    core.shutdown().await?;
    drop(core);
    assert!(
        RedbStateStoreProvider::new(&path)?
            .contains_key("fixture", "new")
            .await?
    );
    Ok(())
}

struct FailingReaction {
    base: ReactionBase,
    fail_start: bool,
}

#[async_trait]
impl Reaction for FailingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "shutdown-failure-fixture"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        Vec::new()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn start(&self) -> Result<()> {
        let shutdown = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let task = tokio::spawn(async move {
            let _ = shutdown.await;
            drop(base);
        });
        self.base.set_processing_task(task).await;
        self.base.set_status(ComponentStatus::Running, None).await;
        if self.fail_start {
            anyhow::bail!("injected startup failure");
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;
        anyhow::bail!("injected stop failure")
    }
}

async fn shutdown_after_failure(fail_start: bool) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let store = Arc::new(RedbStateStoreProvider::new(&path)?);
    store.set("fixture", "retained", vec![7]).await?;
    let reaction = FailingReaction {
        base: ReactionBase::new(ReactionBaseParams::new("failing-reaction", Vec::new())),
        fail_start,
    };
    let core = DrasiLib::builder()
        .with_state_store_provider(store)
        .with_reaction(reaction)
        .build()
        .await?;
    let started = core.start().await;
    assert_eq!(started.is_err(), fail_start);
    let result = timeout(Duration::from_secs(10), core.shutdown()).await?;
    assert!(
        result.is_err(),
        "shutdown must report stop failures after completing other cleanup"
    );
    assert!(!core.is_running().await);
    core.shutdown().await?;
    assert!(core.start().await.is_err());
    drop(core);
    let reopened = RedbStateStoreProvider::new(&path)?;
    assert_eq!(reopened.get("fixture", "retained").await?, Some(vec![7]));
    reopened.set("fixture", "new", vec![8]).await?;
    assert_eq!(reopened.get("fixture", "new").await?, Some(vec![8]));
    Ok(())
}

#[tokio::test]
async fn shutdown_after_partial_start_stops_tasks_and_releases_state() -> Result<()> {
    shutdown_after_failure(true).await
}

#[tokio::test]
async fn shutdown_reports_stop_failure_and_still_releases_state() -> Result<()> {
    shutdown_after_failure(false).await
}

#[tokio::test]
async fn shutdown_cleans_unstarted_and_individually_started_instances() -> Result<()> {
    for start_source in [false, true] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.redb");
        let store = Arc::new(RedbStateStoreProvider::new(&path)?);
        store.set("fixture", "retained", vec![9]).await?;
        let core = DrasiLib::builder()
            .with_state_store_provider(store)
            .build()
            .await?;
        if start_source {
            core.start_source("__component_graph__").await?;
        }
        assert!(!core.is_running().await);
        core.shutdown().await?;
        drop(core);
        let store = RedbStateStoreProvider::new(&path)?;
        assert_eq!(store.get("fixture", "retained").await?, Some(vec![9]));
        store.set("fixture", "retained", vec![10]).await?;
        assert_eq!(store.get("fixture", "retained").await?, Some(vec![10]));
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_shutdown_waits_for_teardown() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let core = DrasiLib::builder()
        .with_state_store_provider(Arc::new(RedbStateStoreProvider::new(&path)?))
        .build()
        .await?;
    let graph = core.component_graph();
    let guard = graph.write().await;
    let mut first = Box::pin(core.shutdown());
    let mut second = Box::pin(core.shutdown());
    // Hold the graph so the first shutdown cannot finish its teardown.
    assert!(poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx)))
        .await
        .is_pending());
    assert!(poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx)))
        .await
        .is_pending());
    drop(guard);
    let (first_result, second_result) = tokio::join!(first, second);
    first_result?;
    second_result?;
    drop(graph);
    drop(core);
    let store = RedbStateStoreProvider::new(&path)?;
    store.set("fixture", "new", vec![1]).await?;
    assert_eq!(store.get("fixture", "new").await?, Some(vec![1]));
    Ok(())
}

#[tokio::test]
async fn cancelled_shutdown_can_finish_on_retry() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.redb");
    let core = DrasiLib::builder()
        .with_state_store_provider(Arc::new(RedbStateStoreProvider::new(&path)?))
        .build()
        .await?;
    let graph = core.component_graph();
    let guard = graph.write().await;
    let mut shutdown = Box::pin(core.shutdown());
    assert!(poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx)))
        .await
        .is_pending());
    drop(shutdown);
    drop(guard);
    core.shutdown().await?;
    drop(graph);
    drop(core);
    let store = RedbStateStoreProvider::new(&path)?;
    store.set("fixture", "new", vec![2]).await?;
    assert_eq!(store.get("fixture", "new").await?, Some(vec![2]));
    Ok(())
}

#[tokio::test]
async fn stop_preserves_restartable_components() -> Result<()> {
    let core = DrasiLib::builder().build().await?;
    core.start().await?;
    let mut events = core.subscribe_all_component_events();
    core.stop().await?;
    timeout(Duration::from_secs(5), async {
        while let Ok(event) = events.recv().await {
            if event.component_id == "__component_graph__"
                && event.status == ComponentStatus::Stopped
            {
                return;
            }
        }
        panic!("component event channel closed before source stopped");
    })
    .await?;
    core.start().await?;
    assert!(core.is_running().await);
    core.shutdown().await?;
    assert!(core.start().await.is_err());
    Ok(())
}
