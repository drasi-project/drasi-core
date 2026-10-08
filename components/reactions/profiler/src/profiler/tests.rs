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

use super::*;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::context::workers::{spawn_owned_worker, WorkerAlreadyOwned, WorkerCleanupError};
use std::collections::HashMap;
use std::time::Duration;

async fn enqueue_sample(reaction: &ProfilerReaction, sequence: u64) -> Result<()> {
    let mut result = QueryResult::new(
        "q1".into(),
        sequence,
        chrono::Utc::now(),
        vec![ResultDiff::Noop],
        HashMap::new(),
    );
    result.profiling = Some(ProfilingMetadata::new());
    reaction.enqueue_query_result(result).await?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !reaction.base.priority_queue.is_empty().await {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn pending_sample_survives_cancelled_and_timed_out_stop() -> Result<()> {
    let reaction = ProfilerReaction::builder("held-sample")
        .with_query("q1")
        .build()?;
    let held = reaction.stats.read().await;
    reaction.start().await?;
    enqueue_sample(&reaction, 1).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.stop())
            .await
            .is_err()
    );
    let error = reaction.stop().await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<WorkerCleanupError>(),
        Some(WorkerCleanupError::TimedOut { .. })
    ));
    tokio::task::yield_now().await;
    assert!(!reaction
        .base
        .processing_task
        .read()
        .await
        .as_ref()
        .unwrap()
        .is_finished());
    assert_eq!(reaction.status().await, ComponentStatus::Stopping);
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    assert_eq!(held.count, 0);
    drop(held);
    reaction.stop().await?;
    assert_eq!(reaction.stats.read().await.count, 1);
    assert!(reaction.base.processing_task.read().await.is_none());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn profiler_stops_promptly_and_collects_samples_after_each_restart() -> Result<()> {
    let reaction = ProfilerReaction::builder("restarted-samples")
        .with_query("q1")
        .build()?;
    reaction.stop().await?;
    for sequence in 1..=3 {
        reaction.start().await?;
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .is::<WorkerAlreadyOwned>());
        enqueue_sample(&reaction, sequence).await?;
        tokio::time::timeout(Duration::from_secs(1), reaction.stop()).await??;
        assert_eq!(
            reaction.stats.read().await.count,
            usize::try_from(sequence)?
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        assert!(reaction.base.processing_task.read().await.is_none());
    }
    reaction.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_profiler_start_and_worker_panic_block_replacement() -> Result<()> {
    let reaction = ProfilerReaction::builder("cancelled-profiler").build()?;
    let shutdown = reaction.base.shutdown_tx.write().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.start())
            .await
            .is_err()
    );
    assert!(reaction.base.processing_task.read().await.is_none());
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    drop(shutdown);
    reaction.stop().await?;
    spawn_owned_worker(&reaction.base.processing_task, async {
        panic!("profiler worker failed")
    })
    .await?;
    let error = reaction.stop().await.unwrap_err();
    assert!(
        matches!(error.downcast_ref::<WorkerCleanupError>(), Some(WorkerCleanupError::Join(cause)) if cause.is_panic())
    );
    assert!(reaction.base.processing_task.read().await.is_none());
    assert!(reaction
        .start()
        .await
        .unwrap_err()
        .is::<WorkerAlreadyOwned>());
    reaction.stop().await?;
    reaction.start().await?;
    reaction.stop().await?;
    Ok(())
}
