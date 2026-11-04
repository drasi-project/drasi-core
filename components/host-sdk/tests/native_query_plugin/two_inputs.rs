// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::evaluation::context::QueryVariables;

const QUERY: &str = "MATCH (o:Order), (c:Customer) WHERE o.customerId = c.id \
                    RETURN o.id AS orderId, c.name AS customer, o.amount AS amount";

fn node(source: &str, id: &str, label: &str, properties: serde_json::Value, time: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn order(id: &str, amount: i64, time: u64) -> Element {
    node(
        "orders",
        id,
        "Order",
        json!({"id": id, "customerId": "C1", "amount": amount}),
        time,
    )
}

fn customer(name: &str, time: u64) -> Element {
    node(
        "customers",
        "C1",
        "Customer",
        json!({"id": "C1", "name": name}),
        time,
    )
}

fn row(id: &str, name: &str, amount: i64) -> QueryVariables {
    [
        ("orderId".into(), VariableValue::from(json!(id))),
        ("customer".into(), VariableValue::from(json!(name))),
        ("amount".into(), VariableValue::from(json!(amount))),
    ]
    .into_iter()
    .collect()
}

fn assert_rows(snapshot: &OutputSnapshot, expected: &[QueryVariables]) -> Result<()> {
    let rows = snapshot
        .records()
        .values()
        .map(|record| Ok(QueryChangeCodec::decode_row(record)?.values))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(rows.len(), expected.len());
    for row in expected {
        assert!(rows.contains(row), "missing {row:?} in {rows:?}");
    }
    Ok(())
}

struct Upstream {
    binding: InputBinding,
    snapshot: Arc<Snapshot>,
    handle: ComponentHandle,
    live: tokio::sync::mpsc::Sender<ChangeEnvelope>,
    emitted: tokio::sync::watch::Receiver<u64>,
}

impl Upstream {
    async fn add(
        fixture: &Fixture,
        name: &str,
        element: Element,
        sequence: u64,
        gate: Option<Arc<SnapshotGate>>,
    ) -> Result<Self> {
        let id = ComponentId::try_new(name)?;
        let stream = StreamId::try_new(format!("{name}/data"))?;
        let producer = GraphProducerIdentity::volatile(
            "bootstrap-test".into(),
            fixture.catalogue.graph_id().to_string(),
            id.clone(),
            stream.clone(),
        )?;
        let record = GraphChangeCodec::element_record(&ElementImage::try_from(element)?)?;
        let snapshot = Arc::new(Snapshot {
            value: OutputSnapshot::try_new(
                OutputCursor::try_new(producer, sequence)?,
                name,
                GraphChangeCodec::schema().descriptor().clone(),
                [(record.identity().clone(), record)].into_iter().collect(),
            )?,
            unavailable: AtomicBool::new(false),
        });
        let provider: Arc<dyn SnapshotProvider> = match gate {
            Some(gate) => Arc::new(PausedSnapshot {
                inner: snapshot.clone(),
                gate,
            }),
            None => snapshot.clone(),
        };
        let (live, events) = tokio::sync::mpsc::channel(4);
        let (emitted, observed) = tokio::sync::watch::channel(0);
        let handle = fixture
            .core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::source(Box::new(SnapshotSource {
                    descriptor: ComponentDescriptor::try_new(
                        id.clone(),
                        vec![port("data", PortDirection::Output)?],
                    )?,
                    snapshot: provider,
                    events,
                    emitted: Some(emitted),
                })))
                .auto_start(false)
                .bind_stream(PortId::try_new("data")?, stream.clone()),
            )
            .await?;
        Ok(Self {
            binding: InputBinding {
                input_port: PortId::try_new("in")?,
                output: OutputEndpoint::new(id, PortId::try_new("data")?),
                stream,
            },
            snapshot,
            handle,
            live,
            emitted: observed,
        })
    }

    fn change(&self, change: SourceChange, sequence: u64) -> Result<ChangeEnvelope> {
        let mut envelope = GraphChangeCodec::encode_change(
            change,
            self.binding.stream.clone(),
            sequence + 1000,
            None,
        )?;
        GraphProducerProgress::annotate(
            &mut envelope,
            self.snapshot.value.cursor().producer(),
            sequence,
        )?;
        Ok(envelope)
    }

    async fn send(&self, change: SourceChange, sequence: u64) -> Result<()> {
        self.live.send(self.change(change, sequence)?).await?;
        Ok(())
    }

    async fn wait_emitted(&self, sequence: u64) -> Result<()> {
        let mut observed = self.emitted.clone();
        while *observed.borrow_and_update() < sequence {
            observed.changed().await?;
        }
        Ok(())
    }
}

struct TwoInputs {
    fixture: Fixture,
    orders: Upstream,
    customers: Upstream,
    query: ComponentHandle,
    reader: Arc<dyn SnapshotProvider>,
    capture: ComponentHandle,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    notification: Arc<Notify>,
    customer_gate: Arc<SnapshotGate>,
}

impl TwoInputs {
    fn configuration(reverse: bool) -> serde_json::Value {
        let mut outputs = vec![
            json!({"component": "orders", "port": "data"}),
            json!({"component": "customers", "port": "data"}),
        ];
        if reverse {
            outputs.reverse();
        }
        json!({"query": QUERY, "volatile": true, "bootstrap": outputs})
    }

    async fn new(reverse: bool) -> Result<Self> {
        let fixture = Fixture::new().await?;
        let orders = Upstream::add(&fixture, "orders", order("O1", 10, 1000), 100, None).await?;
        let customer_gate = Arc::new(SnapshotGate::default());
        let customers = Upstream::add(
            &fixture,
            "customers",
            customer("Alice", 1000),
            3,
            Some(customer_gate.clone()),
        )
        .await?;
        let query = fixture.query_with_inputs(
            "joined",
            Self::configuration(reverse),
            vec![orders.binding.clone(), customers.binding.clone()],
        )?;
        let reader = snapshot_reader(&query)?;
        let query = fixture
            .core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::transformer(Box::new(query)))
                    .auto_start(false)
                    .bind_stream(PortId::try_new("out")?, StreamId::try_new("joined/out")?),
            )
            .await?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let notification = Arc::new(Notify::new());
        let capture = fixture
            .core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("joined-results")?,
                        vec![port("in", PortDirection::Input)?],
                    )?,
                    received: received.clone(),
                    notification: notification.clone(),
                })))
                .auto_start(false),
            )
            .await?;
        connect(
            &fixture.core,
            vec![
                EdgeDefinition::new(
                    Endpoint::new(orders.handle.id().clone(), PortId::try_new("data")?),
                    Endpoint::new(query.id().clone(), PortId::try_new("in")?),
                ),
                EdgeDefinition::new(
                    Endpoint::new(customers.handle.id().clone(), PortId::try_new("data")?),
                    Endpoint::new(query.id().clone(), PortId::try_new("in")?),
                ),
                EdgeDefinition::new(
                    Endpoint::new(query.id().clone(), PortId::try_new("out")?),
                    Endpoint::new(capture.id().clone(), PortId::try_new("in")?),
                ),
            ],
        )
        .await?;
        Ok(Self {
            fixture,
            orders,
            customers,
            query,
            reader,
            capture,
            received,
            notification,
            customer_gate,
        })
    }

    async fn result(&self, count: usize) -> Result<Vec<QueryPartEvaluationContext>> {
        loop {
            let changed = self.notification.notified();
            let output = self
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("join capture poisoned"))?
                .get(count - 1)
                .cloned();
            if let Some(output) = output {
                assert_eq!(output.system().sequence(), count as u64);
                return Ok(QueryChangeCodec::decode_evaluation(&output)?);
            }
            changed.await;
        }
    }
}

fn assert_update(
    results: &[QueryPartEvaluationContext],
    before: QueryVariables,
    after: QueryVariables,
) {
    match results {
        [QueryPartEvaluationContext::Updating {
            before: actual_before,
            after: actual_after,
            ..
        }] => {
            assert_eq!(actual_before, &before);
            assert_eq!(actual_after, &after);
        }
        other => panic!("unexpected join update: {other:?}"),
    }
}

async fn quiet_join(reverse: bool) -> Result<()> {
    let inputs = TwoInputs::new(reverse).await?;
    let exercise = async {
        inputs.capture.start().await?;
        inputs.query.start().await?;
        let initial = inputs.reader.snapshot().await?;
        assert_rows(&initial, &[row("O1", "Alice", 10)])?;
        assert_eq!(initial.cursor().sequence(), 0);
        inputs.orders.handle.start().await?;
        inputs.customers.handle.start().await?;
        inputs
            .orders
            .send(
                SourceChange::Replace {
                    element: order("O1", 999, 5000),
                },
                100,
            )
            .await?;
        inputs
            .customers
            .send(
                SourceChange::Replace {
                    element: customer("Covered", 5001),
                },
                3,
            )
            .await?;
        inputs
            .customers
            .send(
                SourceChange::Replace {
                    element: customer("Bob", 1100),
                },
                4,
            )
            .await?;
        assert_update(
            &inputs.result(1).await?,
            row("O1", "Alice", 10),
            row("O1", "Bob", 10),
        );
        inputs
            .orders
            .send(
                SourceChange::Replace {
                    element: order("O1", 20, 1200),
                },
                101,
            )
            .await?;
        assert_update(
            &inputs.result(2).await?,
            row("O1", "Bob", 10),
            row("O1", "Bob", 20),
        );
        inputs
            .customers
            .send(
                SourceChange::Delete {
                    metadata: customer("Bob", 1300).get_metadata().clone(),
                },
                5,
            )
            .await?;
        match inputs.result(3).await?.as_slice() {
            [QueryPartEvaluationContext::Removing { before, .. }] => {
                assert_eq!(before, &row("O1", "Bob", 20))
            }
            other => anyhow::bail!("unexpected join removal: {other:?}"),
        }
        assert_rows(&inputs.reader.snapshot().await?, &[])?;
        inputs
            .customers
            .send(
                SourceChange::Insert {
                    element: customer("Carol", 1400),
                },
                6,
            )
            .await?;
        match inputs.result(4).await?.as_slice() {
            [QueryPartEvaluationContext::Adding { after, .. }] => {
                assert_eq!(after, &row("O1", "Carol", 20))
            }
            other => anyhow::bail!("unexpected join addition: {other:?}"),
        }
        inputs
            .orders
            .send(
                SourceChange::Insert {
                    element: order("O2", 30, 1500),
                },
                102,
            )
            .await?;
        match inputs.result(5).await?.as_slice() {
            [QueryPartEvaluationContext::Adding { after, .. }] => {
                assert_eq!(after, &row("O2", "Carol", 30))
            }
            other => anyhow::bail!("unexpected second order: {other:?}"),
        }
        let current = inputs.reader.snapshot().await?;
        assert_rows(&current, &[row("O1", "Carol", 20), row("O2", "Carol", 30)])?;
        assert_eq!(current.cursor().sequence(), 5);
        assert_eq!(
            inputs
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("join capture poisoned"))?
                .len(),
            5
        );
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(10), exercise).await;
    inputs.fixture.core.shutdown().await?;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn two_upstream_snapshots_and_live_positions_are_independent_current_thread() -> Result<()> {
    quiet_join(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_upstream_snapshots_and_live_positions_are_independent_multi_thread() -> Result<()> {
    quiet_join(true).await
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_list_requires_complete_unique_unambiguous_input_bindings() -> Result<()> {
    let inputs = TwoInputs::new(false).await?;
    let bindings = vec![
        inputs.orders.binding.clone(),
        inputs.customers.binding.clone(),
    ];
    for bootstrap in [
        json!([]),
        json!([{"component":"orders","port":"data"}]),
        json!([{"component":"orders","port":"data"},{"component":"orders","port":"data"}]),
        json!([{"component":"orders","port":"data"},{"component":"missing","port":"data"}]),
        json!({"component":"orders","port":"data"}),
    ] {
        let mut configuration = TwoInputs::configuration(false);
        configuration["bootstrap"] = bootstrap;
        assert!(inputs
            .fixture
            .query_with_inputs("invalid", configuration, bindings.clone())
            .is_err());
    }
    let mut ambiguous = bindings;
    ambiguous[1].stream = ambiguous[0].stream.clone();
    assert!(inputs
        .fixture
        .query_with_inputs("invalid", TwoInputs::configuration(false), ambiguous)
        .is_err());
    inputs.fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_second_snapshot_does_not_publish_partial_initialization() -> Result<()> {
    let inputs = TwoInputs::new(false).await?;
    inputs
        .customers
        .snapshot
        .unavailable
        .store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        inputs.capture.start().await?;
        assert!(inputs.query.start().await.is_err());
        assert!(inputs.reader.snapshot().await.is_err());
        assert!(inputs
            .received
            .lock()
            .map_err(|_| anyhow::anyhow!("join capture poisoned"))?
            .is_empty());
        inputs.query.stop().await?;
        inputs
            .customers
            .snapshot
            .unavailable
            .store(false, Ordering::Release);
        inputs.query.start().await?;
        assert_rows(&inputs.reader.snapshot().await?, &[row("O1", "Alice", 10)])?;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    inputs.fixture.core.shutdown().await?;
    result??;
    Ok(())
}

async fn live_input_during_second_snapshot() -> Result<()> {
    let inputs = TwoInputs::new(false).await?;
    let gate = inputs.customer_gate.clone();
    let _release = ReleaseOnDrop(gate.clone());
    let exercise = async {
        inputs.capture.start().await?;
        inputs.orders.handle.start().await?;
        inputs.customers.handle.start().await?;
        gate.armed.store(true, Ordering::Release);
        let while_snapshot_held = async {
            gate.captured.notified().await;
            let captured = gate
                .snapshot
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot gate poisoned"))?
                .clone()
                .context("second snapshot was not captured")?;
            assert_eq!(captured.cursor().sequence(), 3);
            assert!(inputs.reader.snapshot().await.is_err());
            inputs
                .orders
                .send(
                    SourceChange::Replace {
                        element: order("O1", 999, 5000),
                    },
                    100,
                )
                .await?;
            inputs
                .orders
                .send(
                    SourceChange::Replace {
                        element: order("O1", 20, 1100),
                    },
                    101,
                )
                .await?;
            inputs.orders.wait_emitted(101).await?;
            inputs
                .customers
                .send(
                    SourceChange::Replace {
                        element: customer("Covered", 5001),
                    },
                    3,
                )
                .await?;
            inputs.customers.wait_emitted(3).await?;
            assert!(inputs.reader.snapshot().await.is_err());
            assert!(inputs
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("join capture poisoned"))?
                .is_empty());
            gate.release.notify_one();
            Ok::<(), anyhow::Error>(())
        };
        tokio::try_join!(
            async {
                inputs.query.start().await?;
                Ok::<(), anyhow::Error>(())
            },
            while_snapshot_held,
        )?;
        assert_update(
            &inputs.result(1).await?,
            row("O1", "Alice", 10),
            row("O1", "Alice", 20),
        );
        inputs
            .customers
            .send(
                SourceChange::Replace {
                    element: customer("Bob", 1200),
                },
                4,
            )
            .await?;
        assert_update(
            &inputs.result(2).await?,
            row("O1", "Alice", 20),
            row("O1", "Bob", 20),
        );
        inputs
            .orders
            .send(
                SourceChange::Replace {
                    element: order("O1", 30, 1300),
                },
                102,
            )
            .await?;
        assert_update(
            &inputs.result(3).await?,
            row("O1", "Bob", 20),
            row("O1", "Bob", 30),
        );
        let snapshot = inputs.reader.snapshot().await?;
        assert_rows(&snapshot, &[row("O1", "Bob", 30)])?;
        assert_eq!(snapshot.cursor().sequence(), 3);
        assert_eq!(
            inputs
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("join capture poisoned"))?
                .len(),
            3
        );
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(10), exercise).await;
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), inputs.fixture.core.shutdown()).await??;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn first_input_changes_while_second_snapshot_is_held_current_thread() -> Result<()> {
    live_input_during_second_snapshot().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_input_changes_while_second_snapshot_is_held_multi_thread() -> Result<()> {
    live_input_during_second_snapshot().await
}
