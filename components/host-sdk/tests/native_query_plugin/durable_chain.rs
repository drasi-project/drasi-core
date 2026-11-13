// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

struct Child {
    handle: ComponentHandle,
    progress: Arc<QuerySourceProgress>,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    changed: Arc<Notify>,
}

impl Child {
    async fn new(graph: &Graph, path: &Path) -> Result<Self> {
        let scope = Scope {
            instance_id: GRAPH.into(),
            graph_id: graph.core.output_catalogue()?.graph_id().to_string(),
            generation: 1,
        };
        let mut query = factory()?.create_component_with_processing(ComponentId::try_new("8badba5d-be28-5339-a4de-413fc739c325")?,
            json!({"query":"MATCH (n) RETURN sum(n.total) AS total", "bootstrap":true, "recovery":"auto_reset",
                "stream":"downstream/out"}),
            scope, provider(path)?)?;
        let progress = query
            .processing_progress_owner()
            .context("missing downstream progress")?;
        let bootstrap = OutputBootstrapProvider::new(
            graph.core.output_catalogue()?,
            vec![OutputEndpoint::new(
                graph.query.id().clone(),
                PortId::try_new("out")?,
            )],
            progress.clone(),
            Duration::from_secs(5),
        )?;
        query.bind_bootstrap(Arc::new(NativeBootstrapResource(bootstrap)))?;
        let handle = graph
            .core
            .add_computation_component(
                ComponentAddition::new(
                    drasi_lib::computation::v1::ComponentName::try_new("downstream")
                        .expect("fixture component name"),
                    ConstructedComponent::transformer(Box::new(query)),
                )
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out")?,
                    StreamId::try_new("downstream/out")?,
                ),
            )
            .await?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(Notify::new());
        let sink = graph
            .core
            .add_computation_component(
                ComponentAddition::new(
                    drasi_lib::computation::v1::ComponentName::try_new("downstream-results")
                        .expect("fixture component name"),
                    ConstructedComponent::sink(Box::new(Sink {
                        descriptor: ComponentDescriptor::try_new(
                            ComponentId::try_new("6f9b567b-37a1-5cf5-ba29-fa86ef698e32")?,
                            vec![port_descriptor("in", PortDirection::Input)?],
                        )?,
                        received: received.clone(),
                        notification: changed.clone(),
                    })),
                )
                .auto_start(false),
            )
            .await?;
        crate::bootstrap::connect_with_capacity(
            &graph.core,
            vec![
                EdgeDefinition::new(
                    Endpoint::new(graph.query.id().clone(), PortId::try_new("out")?),
                    Endpoint::new(handle.id().clone(), PortId::try_new("in")?),
                ),
                EdgeDefinition::new(
                    Endpoint::new(handle.id().clone(), PortId::try_new("out")?),
                    Endpoint::new(sink.id().clone(), PortId::try_new("in")?),
                ),
            ],
            8,
        )
        .await?;
        sink.start().await?;
        Ok(Self {
            handle,
            progress,
            received,
            changed,
        })
    }

    async fn output(&self, index: usize) -> Result<ChangeEnvelope> {
        loop {
            let changed = self.changed.notified();
            if let Some(output) = self
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("child output capture poisoned"))?
                .get(index)
                .cloned()
            {
                return Ok(output);
            }
            changed.await;
        }
    }

    async fn snapshot(&self, graph: &Graph) -> Result<OutputSnapshot> {
        Ok(graph
            .core
            .output_catalogue()?
            .resolve(&OutputEndpoint::new(
                self.handle.id().clone(),
                PortId::try_new("out")?,
            ))?
            .snapshot()
            .await?)
    }
}

async fn replay_and_gap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &directory.path().join("upstream"), 1).await?;
    let exercise = async {
        graph.start().await?;
        let child = Child::new(&graph, &directory.path().join("downstream")).await?;
        child
            .handle
            .start()
            .await
            .context("initial downstream bootstrap")?;
        let initial = child.snapshot(&graph).await?;
        assert_eq!(total(&initial)?, 30.0);
        assert_eq!(initial.cursor().sequence(), 0);
        let producer = initial.cursor().producer().clone();
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":12}}]))
            .await?;
        let output = child.output(0).await?;
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(30.0),
            32.0,
            1,
        )?;
        child.handle.stop().await?;
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":13}}]))
            .await?;
        graph.output(1).await?;
        child
            .handle
            .start()
            .await
            .context("downstream replay restart")?;
        let replayed = child.output(1).await?;
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: replayed,
            }],
            Some(32.0),
            33.0,
            2,
        )?;
        assert_eq!(child.snapshot(&graph).await?.cursor().producer(), &producer);
        child.handle.stop().await?;
        for (index, amount) in [(2, 14), (3, 15)] {
            graph
                .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":amount}}]))
                .await?;
            graph.output(index).await?;
        }
        child
            .handle
            .start()
            .await
            .context("downstream retention-gap reset")?;
        let reset = child.snapshot(&graph).await?;
        assert_eq!(total(&reset)?, 35.0);
        assert_ne!(reset.cursor().producer(), &producer);
        assert_eq!(
            reset.cursor().sequence(),
            2,
            "reset must retain the logical high-water mark"
        );
        assert_eq!(child.progress.snapshot().reset_generation, 1);
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":16}}]))
            .await?;
        let output = child.output(2).await?;
        assert_eq!(
            output.system().sequence(),
            3,
            "reset must retain the transport high-water mark"
        );
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(35.0),
            36.0,
            3,
        )?;
        let progress = child.progress.snapshot();
        assert_eq!(
            progress.checkpoints
                [&SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?)]
                .sequence,
            5
        );
        assert_eq!(
            child
                .received
                .lock()
                .expect("child result capture poisoned")
                .len(),
            3,
            "buffered duplicates were evaluated again"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

async fn waits_for_upstream() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &directory.path().join("upstream"), 1).await?;
    let exercise = async {
        let child = Child::new(&graph, &directory.path().join("downstream")).await?;
        let mut progress = child.progress.subscribe();
        let start = child.handle.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => anyhow::bail!("downstream started before its upstream: {result:?}"),
            result = async {
                while !progress.borrow_and_update().recovered { progress.changed().await?; }
                Ok::<_, anyhow::Error>(())
            } => result?,
        }
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut start).await.is_err());
        assert!(child.snapshot(&graph).await.is_err());
        let before = graph.query.observed()?;
        graph.start().await?;
        start.await.with_context(|| {
            let after = graph.query.observed().expect("upstream observation");
            format!("upstream initialization changed {:?} to {:?}",
                (before.generation, before.operation, before.lifecycle, before.realization),
                (after.generation, after.operation, after.lifecycle, after.realization))
        })?;
        assert_eq!(total(&child.snapshot(&graph).await?)?, 30.0);
        Ok::<_, anyhow::Error>(())
    }.await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_chain_waits_for_upstream_initialization_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), waits_for_upstream()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_chain_waits_for_upstream_initialization_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), waits_for_upstream()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_chain_bootstrap_replay_and_gap_reset_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), replay_and_gap()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_chain_bootstrap_replay_and_gap_reset_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), replay_and_gap()).await?
}
