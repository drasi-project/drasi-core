// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    computation::v1::GraphError,
    config::QueryConfig,
    error::ComponentNotFoundError,
    queries::FetchError,
    DispatchMode, DrasiLib, Query,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use futures::{stream::BoxStream, StreamExt};
use std::{future::Future, sync::Arc, time::Duration};

const DEADLINE: Duration = Duration::from_secs(10);
type Results = BoxStream<'static, drasi_lib::Result<Arc<QueryResult>>>;

fn configuration(mode: DispatchMode, retention: usize) -> QueryConfig {
    let mut config = Query::cypher("query")
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_input("input")
        .enable_bootstrap(false)
        .auto_start(true)
        .with_dispatch_buffer_capacity(1)
        .with_outbox_capacity(retention)
        .build();
    config.dispatch_mode = Some(mode);
    config
}

async fn fixture(
    id: &str,
    mode: DispatchMode,
    retention: usize,
) -> Result<(DrasiLib, ApplicationSourceHandle)> {
    let (source, input) = ApplicationSource::new(
        "input",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let core = DrasiLib::builder()
        .with_id(id)
        .with_priority_queue_capacity(1)
        .with_dispatch_buffer_capacity(1)
        .with_source(source)
        .with_query(configuration(mode, retention))
        .build()
        .await?;
    core.start().await?;
    tokio::time::timeout(
        DEADLINE,
        core.computation_component("query")?.wait_started(),
    )
    .await??;
    Ok((core, input))
}

async fn insert(input: &ApplicationSourceHandle, value: i64) -> Result<()> {
    input
        .send_node_insert(
            value.to_string(),
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_integer("value", value)
                .build(),
        )
        .await
}

async fn rows(core: &DrasiLib, count: usize) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if core.get_query_results("query").await?.len() == count {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .context("query did not reach the expected snapshot")?
}

async fn value(stream: &mut Results, expected: i64) -> Result<u64> {
    let result = tokio::time::timeout(DEADLINE, stream.next())
        .await?
        .context("result stream closed")??;
    assert_eq!(result.query_id, "query");
    assert!(
        matches!(&result.results[..], [ResultDiff::Add { data, .. }] if data["value"] == expected),
        "{result:?}"
    );
    Ok(result.sequence)
}

async fn scoped_readers() -> Result<()> {
    let (first, input) = fixture("first", DispatchMode::Channel, 64).await?;
    let (second, other_input) = fixture("second", DispatchMode::Channel, 64).await?;
    insert(&input, 0).await?;
    rows(&first, 1).await?;
    let control = first.computation_control()?;
    let before = control
        .observed()
        .components
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let subscription = first.subscribe_query_results("query").await?;
    assert_eq!(subscription.snapshot.as_of_sequence, 1);
    assert_eq!(
        subscription.snapshot.to_vec(),
        [serde_json::json!({"value": 0})]
    );
    let mut a = subscription.stream;
    let mut b = first.subscribe_query_results("query").await?.stream;
    let mut other = second.subscribe_query_results("query").await?.stream;
    assert_eq!(
        control
            .observed()
            .components
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        before
    );
    assert!(first.list_reactions().await?.is_empty());

    insert(&other_input, 99).await?;
    assert_eq!(value(&mut other, 99).await?, 1);
    insert(&input, 1).await?;
    assert_eq!(value(&mut a, 1).await?, 2);
    assert_eq!(value(&mut b, 1).await?, 2);
    drop(a);
    insert(&input, 2).await?;
    assert_eq!(value(&mut b, 2).await?, 3);
    first.stop_query("query").await?;
    first.start_query("query").await?;
    first.computation_component("query")?.wait_started().await?;
    insert(&input, 3).await?;
    assert_eq!(value(&mut b, 3).await?, 4);
    drop(b);
    drop(other);
    assert_eq!(
        control
            .observed()
            .components
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        before
    );
    first.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}

async fn bounded_reader_drop() -> Result<()> {
    let (core, input) = fixture("bounded", DispatchMode::Channel, 64).await?;
    let unread = core.subscribe_query_results("query").await?;
    let produce = async {
        for next in 1..=32 {
            insert(&input, next).await?;
        }
        rows(&core, 32).await
    };
    futures::pin_mut!(produce);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut produce)
            .await
            .is_err(),
        "an unread bounded subscription must backpressure publication"
    );
    let count = core.get_query_results("query").await?.len();
    assert!(
        (1..=4).contains(&count),
        "unexpected bounded progress: {count}"
    );
    drop(unread);
    tokio::time::timeout(DEADLINE, produce)
        .await
        .context("dropping an unpolled reader did not release publication")??;
    core.shutdown().await?;
    Ok(())
}

async fn broadcast_recovery_and_replacement() -> Result<()> {
    let (core, input) = fixture("replay", DispatchMode::Broadcast, 64).await?;
    let mut stream = core.subscribe_query_results("query").await?.stream;
    insert(&input, 1).await?;
    assert_eq!(value(&mut stream, 1).await?, 1);
    for next in 2..=8 {
        insert(&input, next).await?;
    }
    rows(&core, 8).await?;
    for expected in 2..=8 {
        assert_eq!(value(&mut stream, expected).await?, expected as u64);
    }
    insert(&input, 9).await?;
    assert_eq!(value(&mut stream, 9).await?, 9);

    for next in 10..=16 {
        insert(&input, next).await?;
    }
    rows(&core, 16).await?;
    assert_eq!(value(&mut stream, 10).await?, 10);
    let mut replacement = configuration(DispatchMode::Broadcast, 64);
    replacement.query = "MATCH (n:Item) RETURN n.value * 2 AS value".into();
    core.update_query("query", replacement).await?;
    let error = tokio::time::timeout(DEADLINE, stream.next())
        .await?
        .context("replacement must explicitly invalidate the reader")?
        .expect_err("queued replay from the old generation must not escape");
    assert!(
        matches!(
            error.downcast_ref::<GraphError>(),
            Some(GraphError::StaleGeneration)
        ),
        "{error:?}"
    );
    assert!(stream.next().await.is_none());
    core.shutdown().await?;
    Ok(())
}

async fn unavailable_history() -> Result<()> {
    let (core, input) = fixture("history-gap", DispatchMode::Broadcast, 2).await?;
    let mut stream = core.subscribe_query_results("query").await?.stream;
    for next in 1..=8 {
        insert(&input, next).await?;
    }
    rows(&core, 8).await?;
    let error = tokio::time::timeout(DEADLINE, stream.next())
        .await?
        .context("history loss must be an explicit error")?
        .expect_err("history loss must not silently skip to the newest result");
    assert!(
        matches!(error.downcast_ref::<FetchError>(), Some(FetchError::OutboxGap(gap)) if gap.requested == 0 && gap.latest_sequence == 8),
        "{error:?}"
    );
    assert!(stream.next().await.is_none());
    core.shutdown().await?;
    Ok(())
}

async fn snapshot_handoff() -> Result<()> {
    let (core, input) = fixture("snapshot-handoff", DispatchMode::Broadcast, 64).await?;
    insert(&input, 1).await?;
    rows(&core, 1).await?;
    let producing = async {
        for next in 2..=32 {
            insert(&input, next).await?;
            tokio::task::yield_now().await;
        }
        rows(&core, 32).await
    };
    let (subscription, produced) = tokio::join!(core.subscribe_query_results("query"), producing,);
    produced?;
    let subscription = subscription?;
    let cutoff = subscription.snapshot.as_of_sequence;
    assert!((1..=32).contains(&cutoff));
    let mut snapshot = subscription.snapshot.to_vec();
    snapshot.sort_by_key(|row| row["value"].as_i64());
    assert_eq!(
        snapshot,
        (1..=cutoff)
            .map(|value| serde_json::json!({"value": value}))
            .collect::<Vec<_>>()
    );
    let mut stream = subscription.stream;
    for expected in cutoff + 1..=32 {
        assert_eq!(value(&mut stream, expected as i64).await?, expected);
    }
    insert(&input, 33).await?;
    assert_eq!(value(&mut stream, 33).await?, 33);
    core.shutdown().await?;
    Ok(())
}

async fn unavailable_queries_and_shutdown() -> Result<()> {
    let (core, _) = fixture("lifecycle", DispatchMode::Channel, 64).await?;
    let error = match core.subscribe_query_results("missing").await {
        Err(error) => error,
        Ok(_) => anyhow::bail!("missing query unexpectedly exposed a reader"),
    };
    assert!(error.downcast_ref::<ComponentNotFoundError>().is_some());
    core.stop_query("query").await?;
    let error = match core.subscribe_query_results("query").await {
        Err(error) => error,
        Ok(_) => anyhow::bail!("stopped query unexpectedly exposed a reader"),
    };
    assert!(matches!(
        error.downcast_ref::<FetchError>(),
        Some(FetchError::NotRunning { .. })
    ));
    core.start_query("query").await?;
    core.computation_component("query")?.wait_started().await?;
    let mut stream = core.subscribe_query_results("query").await?.stream;
    core.remove_query("query").await?;
    assert!(tokio::time::timeout(DEADLINE, stream.next())
        .await?
        .context("removal must explicitly invalidate the reader")?
        .is_err());
    assert!(stream.next().await.is_none());

    let query = core
        .add_query_with_handle(configuration(DispatchMode::Channel, 64))
        .await?;
    query.wait_started().await?;
    let mut stream = core.subscribe_query_results("query").await?.stream;
    core.shutdown().await?;
    assert!(tokio::time::timeout(DEADLINE, stream.next())
        .await?
        .context("shutdown must explicitly invalidate the reader")?
        .is_err());
    assert!(stream.next().await.is_none());
    Ok(())
}

fn both_runtimes<F, Fut>(case: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    for current_thread in [true, false] {
        let mut builder = if current_thread {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        builder.enable_all().build()?.block_on(async {
            tokio::time::timeout(Duration::from_secs(20), case())
                .await
                .context("subscription scenario timed out")?
        })?;
    }
    Ok(())
}

#[test]
fn query_readers_are_scoped_to_the_current_head_and_instance_without_reaction_nodes() -> Result<()>
{
    both_runtimes(scoped_readers)
}

#[test]
fn channel_readers_backpressure_and_release_even_without_being_polled() -> Result<()> {
    both_runtimes(bounded_reader_drop)
}

#[test]
fn broadcast_readers_replay_gaps_once_and_fence_buffered_results_after_replacement() -> Result<()> {
    both_runtimes(broadcast_recovery_and_replacement)
}

#[test]
fn broadcast_readers_surface_unavailable_history_without_skipping_results() -> Result<()> {
    both_runtimes(unavailable_history)
}

#[test]
fn snapshot_and_live_reader_cover_concurrent_updates_without_gaps_or_duplicates() -> Result<()> {
    both_runtimes(snapshot_handoff)
}

#[test]
fn query_readers_preserve_unavailable_query_errors_and_close_on_shutdown() -> Result<()> {
    both_runtimes(unavailable_queries_and_shutdown)
}
