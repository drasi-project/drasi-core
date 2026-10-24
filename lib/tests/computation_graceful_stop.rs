// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::{computation::v1::*, DrasiLib};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

struct Source {
    descriptor: ComponentDescriptor,
    input: mpsc::Receiver<OutputEnvelope>,
}
#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.input.recv().await)
    }
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
    dropped: AtomicUsize,
    completed: AtomicUsize,
    stopped: AtomicUsize,
}
struct Pending(Arc<Gate>, bool);
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.1 {
            self.0.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Sink {
    descriptor: ComponentDescriptor,
    gate: Arc<Gate>,
    output: mpsc::Sender<u64>,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.gate.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let mut pending = Pending(self.gate.clone(), false);
        self.gate.entered.notify_one();
        self.gate.release.notified().await;
        self.output.send(input.envelope.system().sequence()).await?;
        self.gate.completed.fetch_add(1, Ordering::SeqCst);
        pending.1 = true;
        Ok(())
    }
}

async fn in_flight_stop(single: bool) {
    let gate = Arc::new(Gate::default());
    let (input, events) = mpsc::channel(4);
    let (output, mut received) = mpsc::channel(4);
    let batch = ComponentBatch::builder()
        .source(Box::new(Source {
            descriptor: descriptor("source", &[], &["out"]),
            input: events,
        }))
        .sink(Box::new(Sink {
            descriptor: descriptor("sink", &["in"], &[]),
            gate: gate.clone(),
            output,
        }))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let drasi = DrasiLib::builder()
        .with_components(batch)
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();
    input.send(output_event(1)).await.unwrap();
    gate.entered.notified().await;
    let control = drasi.computation_control().unwrap();
    let mut observed = control.subscribe_observed();
    let sink = drasi.computation_component("sink").unwrap();
    let stop = async {
        if single {
            sink.stop().await.map(|_| ()).map_err(anyhow::Error::from)
        } else {
            drasi.stop().await.map_err(anyhow::Error::from)
        }
    };
    tokio::pin!(stop);
    tokio::select! {
        result = &mut stop => panic!("stop interrupted admitted work: {result:?}"),
        result = observed.wait_for(|state| {
            if single {
                state.components[&component("sink")].lifecycle == ComponentLifecycle::Quiescing
            } else {
                state.components[&component("source")].lifecycle == ComponentLifecycle::Quiesced
            }
        }) => { result.unwrap(); }
    }
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
    assert_eq!(gate.stopped.load(Ordering::SeqCst), 0);
    gate.release.notify_one();
    stop.await.unwrap();
    assert_eq!(received.recv().await, Some(1));
    assert_eq!(gate.completed.load(Ordering::SeqCst), 1);
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
    if single {
        sink.start().await.unwrap();
    } else {
        drasi.start().await.unwrap();
    }
    input.send(output_event(2)).await.unwrap();
    gate.entered.notified().await;
    gate.release.notify_one();
    assert_eq!(received.recv().await, Some(2));
    drasi.stop().await.unwrap();
    drasi.shutdown().await.unwrap();
}
fn output_event(sequence: u64) -> OutputEnvelope {
    output(root("source", sequence, &[sequence as u16]))
}

#[tokio::test(flavor = "current_thread")]
async fn stop_drains_admitted_work_and_restarts_without_loss_current_thread() {
    for single in [true, false] {
        tokio::time::timeout(Duration::from_secs(10), in_flight_stop(single))
            .await
            .unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_drains_admitted_work_and_restarts_without_loss_multi_thread() {
    for single in [true, false] {
        tokio::time::timeout(Duration::from_secs(10), in_flight_stop(single))
            .await
            .unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn graceful_deadline_refuses_stop_without_aborting_the_in_flight_handler() {
    let gate = Arc::new(Gate::default());
    let (input, events) = mpsc::channel(2);
    let (output, mut received) = mpsc::channel(2);
    let mut graph = ComputationGraph::builder("stop-timeout")
        .cleanup_timeout(Duration::from_millis(50))
        .source(Box::new(Source {
            descriptor: descriptor("source", &[], &["out"]),
            input: events,
        }))
        .sink(Box::new(Sink {
            descriptor: descriptor("sink", &["in"], &[]),
            gate: gate.clone(),
            output,
        }))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        input.send(output_event(1)).await.unwrap();
        gate.entered.notified().await;
        let error = control
            .stop_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap_err();
        assert!(matches!(error, GraphError::ReconciliationTimeout));
        assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
        assert_eq!(gate.stopped.load(Ordering::SeqCst), 0);
        assert_eq!(
            control.observed().components[&component("sink")].lifecycle,
            ComponentLifecycle::Running
        );
        gate.release.notify_one();
        assert_eq!(received.recv().await, Some(1));
        assert_eq!(gate.completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            control
                .stop_components(GraphRevision(1), GraphSelection::All)
                .await
                .unwrap()
                .summary,
            OperationSummary::Completed
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "computation-rocksdb-tests")]
mod durable {
    use super::*;
    use drasi_core::{
        computation::{ComputationIndexProvider, ComputationQuery},
        evaluation::functions::FunctionRegistry,
        query::QueryBuilder,
    };
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use drasi_query_cypher::CypherParser;

    struct Continuation {
        descriptor: ComponentDescriptor,
        gate: Arc<Gate>,
        query: ComputationQuery,
        pending: bool,
    }
    #[async_trait]
    impl ComputationComponent for Continuation {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            self.query.shutdown().await?;
            Ok(())
        }
    }
    #[async_trait]
    impl Transformer for Continuation {
        async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
            self.pending = true;
            Ok(vec![output(root("transform", 1, &[1]))])
        }
        fn has_pending_emissions(&self) -> bool {
            self.pending
        }
        async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
            let mut work = Pending(self.gate.clone(), false);
            self.query
                .resource_transaction(|| async {
                    self.query
                        .resources()
                        .checkpoint_store()
                        .unwrap()
                        .stage_checkpoint("continuation", 1, None)
                        .await?;
                    self.gate.entered.notify_one();
                    self.gate.release.notified().await;
                    Ok(())
                })
                .await?;
            self.pending = false;
            work.1 = true;
            Ok(vec![output(root("transform", 2, &[2]))])
        }
    }

    #[tokio::test]
    async fn stop_cannot_cancel_a_durable_continuation_between_output_and_its_checkpoint_commit() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(RocksDbComputationProvider::new(
            temp.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
            ),
        ));
        let functions = Arc::new(FunctionRegistry::new());
        let query = ComputationQuery::try_build(
            QueryBuilder::new(
                "MATCH (n:Item) RETURN n",
                Arc::new(CypherParser::new(functions.clone())),
            )
            .with_function_registry(functions),
            provider.create_indexes("drain", "durable").await.unwrap(),
        )
        .await
        .unwrap();
        let gate = Arc::new(Gate::default());
        let received = Received::default();
        let (sender, events) = mpsc::channel(2);
        sender.send(output_event(1)).await.unwrap();
        let mut graph = ComputationGraph::builder("durable-drain")
            .source(Box::new(Source {
                descriptor: descriptor("source", &[], &["out"]),
                input: events,
            }))
            .transformer(Box::new(Continuation {
                descriptor: descriptor("transform", &["in"], &["out"]),
                gate: gate.clone(),
                query,
                pending: false,
            }))
            .sink(Box::new(CollectSink::new(
                "sink",
                &["in"],
                received.clone(),
            )))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .bind_stream(endpoint("transform", "out"), stream("transform"))
            .connect(
                edge("source", "transform"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge("transform", "sink"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .unwrap();
        let run = graph.run().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.unwrap();
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await
                .unwrap();
            gate.entered.notified().await;
            let mut observed = control.subscribe_observed();
            let stop = control.stop_components(GraphRevision(1), GraphSelection::All);
            tokio::pin!(stop);
            tokio::select! {
                result=&mut stop=>panic!("stop cut through a durable continuation: {result:?}"),
                state=observed.wait_for(|state| state.components[&component("source")].lifecycle==ComponentLifecycle::Quiesced)=>{state.unwrap();}
            }
            gate.release.notify_one();
            assert_eq!(stop.await.unwrap().summary, OperationSummary::Completed);
            assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
        drop(graph);
        let stored = provider.create_indexes("drain", "durable").await.unwrap();
        assert_eq!(
            stored
                .checkpoint_store()
                .unwrap()
                .read_checkpoint("continuation")
                .await
                .unwrap()
                .unwrap()
                .sequence,
            1
        );
        stored.cleanup().unwrap().shutdown().await.unwrap();
        assert_eq!(
            received
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.envelope.system().sequence())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
