// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::collections::BTreeMap;

pub(super) const TEMPORAL: &str = "
MATCH (n:Item)
WITH n, drasi.trueNowOrLater(
  datetime.realtime().epochMillis >= n.deadline, n.deadline) AS expired
RETURN n.value AS value, expired";

pub(super) fn scheduled_input(time: u64, deadline: u64) -> Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "one"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(json!({"value": 7, "deadline": deadline})),
    };
    input(SourceChange::Insert { element }, 1)
}

fn volatile() -> Result<NativeComponentProxy> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/volatile-query"
        })
        .context("missing volatile query factory")?;
    factory.create_component(
        component_id()?,
        json!({"query": TEMPORAL, "volatile": true}),
    )
}

async fn expired(query: &NativeComponentProxy, expected: bool) -> Result<()> {
    let readers = query.output_readers();
    let snapshot = readers
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|port| port.snapshot())
        .context("missing query snapshot")?
        .snapshot()
        .await?;
    assert_eq!(snapshot.records().len(), 1);
    let row = QueryChangeCodec::decode_row(
        snapshot
            .records()
            .values()
            .next()
            .context("missing temporal result row")?,
    )?;
    assert_eq!(
        row.values.get("expired"),
        Some(&if expected {
            VariableValue::Bool(true)
        } else {
            VariableValue::Awaiting
        })
    );
    Ok(())
}

async fn persistent_restart_and_replay(publication: QueryPublicationMode) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let storage: Arc<dyn drasi_core::computation::ComputationIndexProvider> = match publication {
        QueryPublicationMode::Atomic => provider(directory.path())?,
        QueryPublicationMode::NonAtomic => Arc::new(
            super::non_atomic_store::SplitOutputProvider::new(directory.path())?,
        ),
    };
    let create = |generation| {
        factory()?.create_component_with_processing_publication(
        component_id()?,
        json!({
            "query": TEMPORAL,
            "publication": if publication == QueryPublicationMode::Atomic { "atomic" } else { "non_atomic" },
        }),
        scope(generation),
        storage.clone(),
        None,
        publication,
    )
    };
    let mut query = create(1)?;
    query.initialize().await?;
    query.start().await?;
    let initial = query.transform(scheduled_input(1000, 2000)?).await?;
    query.delivery_completed(&initial).await?;
    expired(&query, false).await?;
    assert!(
        query
            .wakeup_source()
            .context("missing native wakeup")?
            .has_pending()
            .await?
    );
    query.stop().await?;
    drop(query);

    let mut query = create(2)?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    expired(&query, false).await?;
    let due = query.on_wakeup().await?;
    assert_eq!(due.len(), 1);
    assert_eq!(QueryChangeCodec::query_sequence(&due[0].envelope)?, 2);
    assert_eq!(
        due[0]
            .envelope
            .system()
            .timestamp()
            .context("missing due-event timestamp")?
            .timestamp_millis(),
        2000
    );
    assert!(matches!(
        QueryChangeCodec::metadata(&due[0].envelope)?.input_origin,
        Some(drasi_computation_contracts::QueryInputOrigin::Scheduled(reference))
            if reference.namespace.as_ref() == "source"
    ));
    expired(&query, true).await?;
    query.stop().await?;
    drop(query);

    let mut query = create(3)?;
    query.initialize().await?;
    query.start().await?;
    let replay = query.continue_transform().await?;
    assert_eq!(replay.len(), 1);
    assert_eq!(QueryChangeCodec::query_sequence(&replay[0].envelope)?, 2);
    assert_eq!(
        replay[0].envelope.changes().operations(),
        due[0].envelope.changes().operations()
    );
    assert!(replay[0].envelope.system().sequence() > due[0].envelope.system().sequence());
    query.delivery_completed(&replay).await?;
    expired(&query, true).await?;
    assert!(query.on_wakeup().await?.is_empty());
    assert!(
        !query
            .wakeup_source()
            .context("missing native wakeup")?
            .has_pending()
            .await?
    );
    query.stop().await?;
    Ok(())
}

async fn volatile_due_and_cancellation() -> Result<()> {
    let mut query = volatile()?;
    query.initialize().await?;
    query.start().await?;
    query.transform(scheduled_input(1000, 2000)?).await?;
    expired(&query, false).await?;
    let due = query.on_wakeup().await?;
    assert_eq!(due.len(), 1);
    expired(&query, true).await?;
    assert!(query.on_wakeup().await?.is_empty());
    query.stop().await?;

    let mut query = volatile()?;
    query.initialize().await?;
    query.start().await?;
    query.transform(scheduled_input(1000, 2000)?).await?;
    let deleted = query
        .transform(input(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "one"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1500,
                },
            },
            2,
        )?)
        .await?;
    assert_eq!(deleted.len(), 1);
    assert!(
        query.on_wakeup().await?.is_empty(),
        "deleted input must not produce a stale expiry"
    );
    query.stop().await?;

    let mut query = volatile()?;
    query.initialize().await?;
    query.start().await?;
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    query.transform(scheduled_input(now, now + 60_000)?).await?;
    assert!(
        query.on_wakeup().await?.is_empty(),
        "future work ran before its deadline"
    );
    expired(&query, false).await?;
    query.stop().await?;
    Ok(())
}

async fn automatic_graph(indexed: bool) -> Result<()> {
    let plugin = computation::load(library())?;
    let name = if indexed {
        "drasi.experimental/indexed-query"
    } else {
        "drasi.experimental/volatile-query"
    };
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == name)
        .cloned()
        .context("missing query factory")?;
    let query = ComponentId::try_new("3b0a9e84-19ac-504c-a6e5-08c3a146b322")?;
    let mut specification = factory.specification(
        query.clone(),
        if indexed {
            json!({"query": TEMPORAL, "stream": "query/out"})
        } else {
            json!({"query": TEMPORAL, "volatile": true, "stream": "query/out"})
        },
    )?;
    let source = ComponentId::try_new("7a691d7b-1655-5ab0-9a77-8b816cbc6e13")?;
    let sink = ComponentId::try_new("0e82e982-d831-5004-b9fe-534f4161c9a6")?;
    let from = Endpoint::new(source.clone(), PortId::try_new("out")?);
    let output = Endpoint::new(query.clone(), PortId::try_new("out")?);
    let directory = tempfile::tempdir()?;
    let mut builder = ComputationGraph::builder("scheduled-query");
    if indexed {
        let resource = ResourceId::try_new("indexes")?;
        specification.dependencies =
            BTreeMap::from([("processing".into(), vec![resource.clone()])]);
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "processing".into(),
            })?
            .provide_resource(
                resource,
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(computation::NativeProcessingResource::new(provider(
                        directory.path(),
                    )?)),
                ),
            )?;
    }
    let received = Arc::new(Mutex::new(Vec::new()));
    let notification = Arc::new(Notify::new());
    let mut graph = builder
        .source(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-source-1")
                .expect("fixture component name"),
            Box::new(Source {
                descriptor: ComponentDescriptor::try_new(
                    source,
                    vec![port("out", PortDirection::Output)?],
                )?,
                envelope: Some(scheduled_input(1000, 2000)?.envelope),
            }),
        )
        .component(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                .expect("fixture component name"),
            specification,
            factory,
        )
        .sink(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-sink-1")
                .expect("fixture component name"),
            Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    sink.clone(),
                    vec![port("in", PortDirection::Input)?],
                )?,
                received: received.clone(),
                notification: notification.clone(),
            }),
        )
        .connect(
            EdgeDefinition::new(from.clone(), Endpoint::new(query, PortId::try_new("in")?)),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(output.clone(), Endpoint::new(sink, PortId::try_new("in")?)),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(from, StreamId::try_new("source/out")?)
        .bind_stream(output, StreamId::try_new("query/out")?)
        .build()?;
    let exercise = async {
        graph.start()?.await?;
        loop {
            let notified = notification.notified();
            if received
                .lock()
                .map_err(|_| anyhow::anyhow!("query capture poisoned"))?
                .len()
                >= 2
            {
                break;
            }
            notified.await;
        }
        let outputs = received
            .lock()
            .map_err(|_| anyhow::anyhow!("query capture poisoned"))?;
        assert_eq!(
            outputs.len(),
            2,
            "one input must produce its change and a later scheduled result"
        );
        assert_eq!(QueryChangeCodec::query_sequence(&outputs[1])?, 2);
        assert_eq!(
            outputs[1]
                .system()
                .timestamp()
                .context("missing scheduled timestamp")?
                .timestamp_millis(),
            2000
        );
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(10), exercise).await;
    let observed = graph.observed();
    let count = received
        .lock()
        .map_err(|_| anyhow::anyhow!("query capture poisoned"))?
        .len();
    let stopped = graph.shutdown().await;
    result.with_context(|| format!("scheduled graph retained {count} outputs: {observed:?}"))??;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_scheduling_persistent_restart_current_thread() -> Result<()> {
    persistent_restart_and_replay(QueryPublicationMode::Atomic).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_scheduling_persistent_restart_multi_thread() -> Result<()> {
    persistent_restart_and_replay(QueryPublicationMode::Atomic).await
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_scheduling_restarts_and_replays_current_thread() -> Result<()> {
    persistent_restart_and_replay(QueryPublicationMode::NonAtomic).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_atomic_scheduling_restarts_and_replays_multi_thread() -> Result<()> {
    persistent_restart_and_replay(QueryPublicationMode::NonAtomic).await
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_scheduling_volatile_due_and_cancelled_input() -> Result<()> {
    volatile_due_and_cancellation().await
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_scheduling_automatic_current_thread() -> Result<()> {
    automatic_graph(false).await?;
    automatic_graph(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_scheduling_automatic_multi_thread() -> Result<()> {
    automatic_graph(false).await?;
    automatic_graph(true).await
}
