// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    queries::FetchError,
    DispatchMode,
};
use futures_util::{stream::BoxStream, StreamExt};

const DEADLINE: Duration = Duration::from_secs(10);
type Results = BoxStream<'static, drasi_lib::Result<Arc<QueryResult>>>;

async fn fixture(mode: DispatchMode, retention: usize) -> Result<(tempfile::TempDir, Graph)> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::with_delivery(
        &database,
        &directory.path().join("indexes"),
        1,
        None,
        json!({
            "query": TEXT,
            "bootstrap": true,
            "source_transactions": limits(),
            "outbox_capacity": retention,
        }),
        vec!["items".into()],
        Some((mode, 1)),
    )
    .await?;
    Ok((directory, graph))
}

async fn update(graph: &Graph, sequence: u64) -> Result<()> {
    graph
        .apply(json!([{
            "op": "update", "table": "items", "id": "1",
            "data": {"amount": 10 + sequence}
        }]))
        .await
}

fn assert_result(result: &QueryResult, sequence: u64) {
    assert_eq!(result.query_id, COMPONENT);
    assert_eq!(result.sequence, sequence);
    let [diff] = result.results.as_slice() else {
        panic!("expected one aggregate change: {result:?}");
    };
    let after = match diff {
        ResultDiff::Update { after, .. } | ResultDiff::Aggregation { after, .. } => after,
        ResultDiff::Add { data, .. } => data,
        other => panic!("unexpected aggregate change: {other:?}"),
    };
    assert_eq!(after["total"].as_f64(), Some(30.0 + sequence as f64));
}

async fn next(stream: &mut Results, sequence: u64) -> Result<()> {
    let result = tokio::time::timeout(DEADLINE, stream.next())
        .await?
        .context("query result stream ended")??;
    assert_result(&result, sequence);
    Ok(())
}

async fn subscription_and_restart() -> Result<()> {
    for mode in [DispatchMode::Channel, DispatchMode::Broadcast] {
        let (_directory, graph) = fixture(mode, 64).await?;
        let exercise = async {
            graph.start().await?;
            let control = graph.core.computation_control()?;
            let before = control
                .observed()
                .components
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            let subscription = graph.core.subscribe_query_results(COMPONENT).await?;
            assert_eq!(subscription.snapshot.as_of_sequence, 0);
            assert_eq!(subscription.snapshot.to_vec(), [json!({"total": 30.0})]);
            let mut first = subscription.stream;
            let mut second = graph.core.subscribe_query_results(COMPONENT).await?.stream;
            assert_eq!(
                control
                    .observed()
                    .components
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
                before,
                "readers must not create query, reaction or forwarding components"
            );
            assert!(graph.core.list_reactions().await?.is_empty());

            update(&graph, 1).await?;
            next(&mut first, 1).await?;
            next(&mut second, 1).await?;
            drop(first);
            update(&graph, 2).await?;
            next(&mut second, 2).await?;

            let catalog = graph.catalog.as_ref().context("missing result catalogue")?;
            let view = catalog.snapshot(COMPONENT, DEADLINE).await?;
            assert_eq!(view.as_of_sequence, 2);
            assert_eq!(view.rows.len(), 1);
            assert_eq!(catalog.replay(COMPONENT, 0, DEADLINE).await?.len(), 2);

            // Coordinated capture and its sole progress owner have a shared lifecycle.
            graph.source.stop().await?;
            graph.core.stop_query(COMPONENT).await?;
            {
                let waiting = second.next();
                tokio::pin!(waiting);
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), &mut waiting)
                        .await
                        .is_err()
                );
                graph.core.start_query(COMPONENT).await?;
                graph.query.wait_started().await?;
                graph.provider.freeze_for_retirement()?.resume();
                graph.source.start().await?;
                update(&graph, 3).await?;
                let result = tokio::time::timeout(DEADLINE, &mut waiting)
                    .await?
                    .context("restart closed the subscription")??;
                assert_result(&result, 3);
            }

            let query = graph
                .core
                .query_manager()
                .get_query_instance(COMPONENT)
                .await?;
            let mut ordinary = query.subscribe("ordinary-reader".into()).await?;
            assert_eq!(ordinary.as_of_sequence, 3);
            update(&graph, 4).await?;
            let result = tokio::time::timeout(DEADLINE, ordinary.receiver.recv()).await??;
            assert_result(&result, 4);
            next(&mut second, 4).await?;
            drop(ordinary);
            graph.core.shutdown().await?;
            assert!(tokio::time::timeout(DEADLINE, second.next())
                .await?
                .context("shutdown must invalidate the reader")?
                .is_err());
            assert!(second.next().await.is_none());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = graph.core.shutdown().await;
        exercise?;
        stopped?;
    }
    Ok(())
}

async fn channel_backpressure() -> Result<()> {
    let (_directory, graph) = fixture(DispatchMode::Channel, 64).await?;
    let exercise = async {
        graph.start().await?;
        let unread = graph.core.subscribe_query_results(COMPONENT).await?;
        let producing = async {
            for sequence in 1..=24 {
                update(&graph, sequence).await?;
            }
            graph.output(23).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(producing);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut producing)
                .await
                .is_err()
        );
        let sequence = graph.snapshot().await?.cursor().sequence();
        assert!(
            (1..=4).contains(&sequence),
            "unread channel did not bound publication: {sequence}"
        );
        drop(unread);
        tokio::time::timeout(DEADLINE, &mut producing).await??;
        assert_eq!(total(&graph.snapshot().await?)?, 54.0);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

async fn broadcast_recovery() -> Result<()> {
    for retention in [64, 2] {
        let (_directory, graph) = fixture(DispatchMode::Broadcast, retention).await?;
        let exercise = async {
            graph.start().await?;
            let mut stream = graph.core.subscribe_query_results(COMPONENT).await?.stream;
            for sequence in 1..=8 {
                update(&graph, sequence).await?;
            }
            graph.output(7).await?;
            if retention == 64 {
                for sequence in 1..=8 {
                    next(&mut stream, sequence).await?;
                }
                update(&graph, 9).await?;
                next(&mut stream, 9).await?;
            } else {
                let error = tokio::time::timeout(DEADLINE, stream.next())
                    .await?
                    .context("history loss must be reported")?
                    .expect_err("history loss was hidden");
                assert!(
                    matches!(error.downcast_ref::<FetchError>(), Some(FetchError::OutboxGap(gap))
                        if gap.requested == 0 && gap.latest_sequence == 8),
                    "{error:?}"
                );
                assert!(stream.next().await.is_none());
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = graph.core.shutdown().await;
        exercise?;
        stopped?;
    }
    Ok(())
}

async fn snapshot_handoff() -> Result<()> {
    let (_directory, graph) = fixture(DispatchMode::Broadcast, 64).await?;
    let exercise = async {
        graph.start().await?;
        let produce = async {
            for sequence in 1..=16 {
                update(&graph, sequence).await?;
            }
            graph.output(15).await?;
            Ok::<_, anyhow::Error>(())
        };
        let (subscription, produced) =
            tokio::join!(graph.core.subscribe_query_results(COMPONENT), produce,);
        produced?;
        let subscription = subscription?;
        let cutoff = subscription.snapshot.as_of_sequence;
        assert!(cutoff <= 16);
        assert_eq!(
            subscription.snapshot.to_vec(),
            [json!({"total": 30.0 + cutoff as f64})]
        );
        let mut stream = subscription.stream;
        for sequence in cutoff + 1..=16 {
            next(&mut stream, sequence).await?;
        }
        update(&graph, 17).await?;
        next(&mut stream, 17).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

async fn subscription_during_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let gate = super::super::lifecycle::Gate::new(false);
    let graph = Graph::with_delivery(
        &database,
        &directory.path().join("indexes"),
        1,
        Some(gate.clone()),
        json!({
            "query": TEXT, "bootstrap": true, "source_transactions": limits(),
            "outbox_capacity": 64,
        }),
        vec!["items".into()],
        Some((DispatchMode::Channel, 1)),
    )
    .await?;
    let exercise = async {
        graph.sink.start().await?;
        graph.outlet.as_ref().context("missing result outlet")?.start().await?;
        let starting = graph.query.start();
        tokio::pin!(starting);
        tokio::select! {
            result = &mut starting => anyhow::bail!("bootstrap did not pause: {result:?}"),
            _ = gate.reached.notified() => {}
        }
        let subscribing = graph.core.subscribe_query_results(COMPONENT);
        tokio::pin!(subscribing);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut subscribing).await.is_err(),
            "subscription must wait for bootstrap rather than expose partial rows or fail an inactive native read"
        );
        gate.release();
        starting.await?;
        let subscription = tokio::time::timeout(DEADLINE, subscribing).await??;
        assert_eq!(subscription.snapshot.as_of_sequence, 0);
        assert_eq!(subscription.snapshot.to_vec(), [json!({"total": 30.0})]);
        let mut stream = subscription.stream;
        graph.provider.freeze_for_retirement()?.resume();
        graph.source.start().await?;
        update(&graph, 1).await?;
        next(&mut stream, 1).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    gate.release();
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

async fn ordinary_reaction() -> Result<()> {
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use drasi_reaction_http::HttpReaction;

    async fn capture(
        State(sender): State<tokio::sync::mpsc::Sender<serde_json::Value>>,
        Json(body): Json<serde_json::Value>,
    ) -> StatusCode {
        match sender.send(body).await {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    let (_directory, graph) = fixture(DispatchMode::Channel, 64).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let (sender, mut received) = tokio::sync::mpsc::channel(8);
    let router = Router::new().fallback(post(capture)).with_state(sender);
    let (shutdown, stopping) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopping.await;
            })
            .await
    });
    let exercise = async {
        graph.start().await?;
        update(&graph, 1).await?;
        graph.output(0).await?;
        graph
            .core
            .add_reaction(
                HttpReaction::builder("ordinary-http")
                    .with_query(COMPONENT)
                    .with_base_url(url)
                    .with_auto_start(false)
                    .build()?,
            )
            .await?;
        graph.core.start_reaction("ordinary-http").await?;
        update(&graph, 2).await?;
        let notification = tokio::time::timeout(DEADLINE, received.recv())
            .await?
            .context("ordinary reaction did not receive the native query result")?;
        assert_eq!(notification["queryId"], COMPONENT);
        assert_eq!(notification["sequenceId"], 2);
        assert_eq!(notification["before"]["total"].as_f64(), Some(31.0));
        assert_eq!(notification["after"]["total"].as_f64(), Some(32.0));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    shutdown
        .send(())
        .map_err(|_| anyhow::anyhow!("HTTP capture stopped unexpectedly"))?;
    let joined = tokio::time::timeout(DEADLINE, server).await?;
    exercise?;
    stopped?;
    joined??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_subscriptions_restart_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), subscription_and_restart()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_subscriptions_restart_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), subscription_and_restart()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_subscriptions_backpressure_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), channel_backpressure()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_subscriptions_backpressure_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), channel_backpressure()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_subscriptions_broadcast_replay_and_gap_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), broadcast_recovery()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_subscriptions_broadcast_replay_and_gap_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), broadcast_recovery()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_subscriptions_snapshot_handoff_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), snapshot_handoff()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_subscriptions_snapshot_handoff_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), snapshot_handoff()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_subscriptions_ordinary_reaction_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), ordinary_reaction()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_subscriptions_ordinary_reaction_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), ordinary_reaction()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_subscriptions_wait_for_bootstrap_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), subscription_during_bootstrap()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_subscriptions_wait_for_bootstrap_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), subscription_during_bootstrap()).await?
}
