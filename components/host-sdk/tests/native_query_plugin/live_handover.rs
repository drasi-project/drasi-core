// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use tokio::sync::watch;

// Evaluation and live output still cross the actual plugin ABI. Only delivery
// of the already-captured snapshot is paused, without holding its processing slot.
struct ObservedQuery {
    inner: NativeComponentProxy,
    snapshots: Arc<PausedSnapshot>,
    completed: watch::Sender<u64>,
}

#[async_trait]
impl ComputationComponent for ObservedQuery {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.inner.configuration()
    }
    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_snapshot(
            PortId::try_new("out").expect("constant port"),
            self.snapshots.clone(),
        )
    }
    async fn start(&mut self) -> Result<()> {
        self.inner.start().await
    }
    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl Transformer for ObservedQuery {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        let outputs = self.inner.transform(input).await?;
        for output in &outputs {
            self.completed
                .send_replace(output.envelope.system().sequence());
        }
        Ok(outputs)
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> Result<()> {
        self.inner.delivery_completed(outputs).await
    }
}

fn endpoint(component: &str, port: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
}

async fn add_query(core: &DrasiLib, query: Box<dyn Transformer>) -> Result<ComponentHandle> {
    let stream = StreamId::try_new(format!("{}/out", query.descriptor().id()))?;
    Ok(core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::transformer(query))
                .auto_start(false)
                .bind_stream(PortId::try_new("out")?, stream),
        )
        .await?)
}

async fn add_capture(
    core: &DrasiLib,
    name: &str,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    notification: Arc<Notify>,
) -> Result<ComponentHandle> {
    Ok(core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new(name)?,
                    vec![port("in", PortDirection::Input)?],
                )?,
                received,
                notification,
            })))
            .auto_start(false),
        )
        .await?)
}

async fn wait_processed(progress: &mut watch::Receiver<u64>, sequence: u64) -> Result<()> {
    while *progress.borrow_and_update() < sequence {
        progress.changed().await?;
    }
    Ok(())
}

async fn wait_received(
    received: &Mutex<Vec<ChangeEnvelope>>,
    notification: &Notify,
    upstream_sequence: u64,
) -> Result<()> {
    loop {
        let changed = notification.notified();
        let complete = received
            .lock()
            .map_err(|_| anyhow::anyhow!("handover capture poisoned"))?
            .last()
            .and_then(|output| output.lineage())
            .is_some_and(|input| {
                input.system().stream().as_str() == "query/out"
                    && input.system().sequence() == upstream_sequence
            });
        if complete {
            return Ok(());
        }
        changed.await;
    }
}

fn changes() -> Vec<SourceChange> {
    let insert = |id, value, time| SourceChange::Insert {
        element: item_with_id(id, value, time),
    };
    let update = |id, value, time| SourceChange::Replace {
        element: item_with_id(id, value, time),
    };
    let delete = |id, time| SourceChange::Delete {
        metadata: item_with_id(id, 0, time).get_metadata().clone(),
    };
    vec![
        insert("three", 5, 15),
        update("two", 7, 16),
        delete("one", 17),
        insert("four", 11, 18),
        update("three", 13, 19),
        delete("two", 20),
        insert("five", 17, 21),
        update("four", 19, 22),
        delete("three", 23),
        insert("six", 23, 24),
        update("five", 29, 25),
        delete("four", 26),
        insert("seven", 31, 27),
        update("six", 37, 28),
        delete("five", 29),
        delete("seven", 30),
    ]
}

async fn running_upstream_handover() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration = Fixture::configuration();
    configuration["query"] = json!("MATCH (n:Item) RETURN n.value AS value");
    let upstream = fixture.query_with("query", configuration, fixture.binding.clone())?;
    let upstream_reader = snapshot_reader(&upstream)?;
    let gate = Arc::new(SnapshotGate::default());
    let _release = ReleaseOnDrop(gate.clone());
    let (completed, mut progress) = watch::channel(0);
    let upstream = add_query(
        &fixture.core,
        Box::new(ObservedQuery {
            inner: upstream,
            snapshots: Arc::new(PausedSnapshot {
                inner: upstream_reader.clone(),
                gate: gate.clone(),
            }),
            completed,
        }),
    )
    .await?;
    let configuration = json!({
        "query": "MATCH (n:Result) RETURN sum(n.value) AS total",
        "volatile": true,
        "bootstrap": [{"component": "query", "port": "out"}]
    });
    let input = InputBinding {
        input_port: PortId::try_new("in")?,
        output: OutputEndpoint::new(ComponentId::try_new("query")?, PortId::try_new("out")?),
        stream: StreamId::try_new("query/out")?,
    };
    let downstream = fixture.query_with("downstream", configuration.clone(), input.clone())?;
    let downstream_reader = snapshot_reader(&downstream)?;
    let downstream = add_query(&fixture.core, Box::new(downstream)).await?;
    let reference = add_query(
        &fixture.core,
        Box::new(fixture.query_with("reference", configuration, input)?),
    )
    .await?;
    let received = Arc::new(Mutex::new(Vec::new()));
    let notification = Arc::new(Notify::new());
    let reference_received = Arc::new(Mutex::new(Vec::new()));
    let reference_notification = Arc::new(Notify::new());
    let capture = add_capture(
        &fixture.core,
        "delayed-results",
        received.clone(),
        notification.clone(),
    )
    .await?;
    let reference_capture = add_capture(
        &fixture.core,
        "reference-results",
        reference_received.clone(),
        reference_notification.clone(),
    )
    .await?;
    connect_with_capacity(
        &fixture.core,
        vec![
            EdgeDefinition::new(endpoint("source", "data")?, endpoint("query", "in")?),
            EdgeDefinition::new(endpoint("query", "out")?, endpoint("downstream", "in")?),
            EdgeDefinition::new(endpoint("query", "out")?, endpoint("reference", "in")?),
            EdgeDefinition::new(
                endpoint("downstream", "out")?,
                endpoint("delayed-results", "in")?,
            ),
            EdgeDefinition::new(
                endpoint("reference", "out")?,
                endpoint("reference-results", "in")?,
            ),
        ],
        3,
    )
    .await?;
    let exercise = async {
        capture.start().await?;
        reference_capture.start().await?;
        fixture
            .core
            .computation_component("unused-sink")?
            .start()
            .await?;
        upstream.start().await?;
        reference.start().await?;
        fixture
            .core
            .computation_component("source")?
            .start()
            .await?;
        fixture.live.send(fixture.input(10, 3)?.envelope).await?;
        fixture.live.send(fixture.input(20, 4)?.envelope).await?;
        wait_processed(&mut progress, 2).await?;
        gate.armed.store(true, Ordering::Release);

        let during_bootstrap = async {
            gate.captured.notified().await;
            let captured = gate
                .snapshot
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot gate poisoned"))?
                .clone()
                .context("snapshot cut missing")?;
            assert_eq!(captured.cursor().sequence(), 2);
            assert_eq!(captured.records().len(), 2);
            assert!(downstream_reader.snapshot().await.is_err());

            let pending = changes()
                .into_iter()
                .enumerate()
                .map(|(index, change)| Ok(fixture.change(change, index as u64 + 5)?.envelope))
                .collect::<Result<Vec<_>>>()?;
            let mut pending = pending.into_iter();
            for envelope in pending.by_ref().take(2) {
                fixture.live.send(envelope).await?;
            }
            wait_processed(&mut progress, 4).await?;
            let newer = upstream_reader.snapshot().await?;
            assert_eq!(newer.cursor().sequence(), 4);
            assert_eq!(captured.cursor().sequence(), 2);
            assert_ne!(newer.records(), captured.records());

            let mut blocked = None;
            for envelope in pending.by_ref() {
                match fixture.live.try_send(envelope) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(envelope)) => {
                        blocked = Some(envelope);
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let blocked = blocked.context("live ingress did not reach bounded backpressure")?;
            assert_eq!(*progress.borrow(), 4);
            assert!(received
                .lock()
                .map_err(|_| anyhow::anyhow!("handover capture poisoned"))?
                .is_empty());

            gate.release.notify_one();
            fixture.live.send(blocked).await?;
            for envelope in pending {
                fixture.live.send(envelope).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::try_join!(
            async {
                downstream.start().await?;
                Ok::<(), anyhow::Error>(())
            },
            during_bootstrap,
        )?;
        wait_processed(&mut progress, 18).await?;
        tokio::try_join!(
            wait_received(&received, &notification, 18),
            wait_received(&reference_received, &reference_notification, 18),
        )?;

        let final_snapshot = downstream_reader.snapshot().await?;
        assert_snapshot_total(&final_snapshot, 37.0)?;
        assert_eq!(final_snapshot.cursor().sequence(), 16);
        let reference_snapshot = fixture
            .catalogue
            .resolve(&OutputEndpoint::new(
                reference.id().clone(),
                PortId::try_new("out")?,
            ))?
            .snapshot()
            .await?;
        assert_snapshot_total(&reference_snapshot, 37.0)?;
        assert_eq!(reference_snapshot.cursor().sequence(), 18);

        let received = received
            .lock()
            .map_err(|_| anyhow::anyhow!("handover capture poisoned"))?;
        let reference_received = reference_received
            .lock()
            .map_err(|_| anyhow::anyhow!("reference capture poisoned"))?;
        assert_eq!(received.len(), 16);
        assert_eq!(reference_received.len(), 18);
        let totals = [
            27.0, 32.0, 12.0, 23.0, 31.0, 24.0, 41.0, 49.0, 36.0, 59.0, 71.0, 52.0, 83.0, 97.0,
            68.0, 37.0,
        ];
        let mut before = 22.0;
        for (index, ((output, reference), after)) in received
            .iter()
            .zip(reference_received.iter().skip(2))
            .zip(totals)
            .enumerate()
        {
            assert_eq!(
                QueryChangeCodec::decode_evaluation(output)?,
                QueryChangeCodec::decode_evaluation(reference)?
            );
            assert_total(
                &[OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: output.clone(),
                }],
                before,
                after,
            )?;
            assert_eq!(output.system().sequence(), index as u64 + 1);
            before = after;
        }
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(10), exercise).await;
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), fixture.core.shutdown()).await??;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn running_upstream_snapshot_handover_is_lossless_under_backpressure_current_thread(
) -> Result<()> {
    running_upstream_handover().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_upstream_snapshot_handover_is_lossless_under_backpressure_multi_thread(
) -> Result<()> {
    running_upstream_handover().await
}
