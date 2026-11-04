// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

fn ordinary_input(
    logical: u64,
    transport: u64,
    value: i64,
    position: &[u8],
) -> Result<InputEnvelope> {
    use drasi_computation_contracts::exchange::{SourceEvent, SourceEventWrapper};
    let event = SourceEventWrapper::new(
        "source".into(),
        SourceEvent::Change(SourceChange::Replace {
            element: item(value, logical),
        }),
        chrono::Utc::now(),
        logical,
    )
    .with_source_position(position.to_vec().into());
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(event),
            &ComponentId::try_new("source")?,
            StreamId::try_new("source/out")?,
            transport,
            None,
        )?,
    })
}

async fn shared_ordinary_progress() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let owner = Arc::new(QuerySourceProgress::new(
        GRAPH,
        ComponentId::try_new(COMPONENT)?,
    )?);
    let mut query = factory()?.create_component_with_processing_owner(
        ComponentId::try_new(COMPONENT)?,
        json!({"query": QUERY}),
        scope(1),
        provider(directory.path())?,
        Some(owner.clone()),
    )?;
    assert!(Arc::ptr_eq(
        &owner,
        &query.processing_progress_owner().context("actual owner")?
    ));
    assert!(
        factory()?
            .create_component_with_processing_owner(
                ComponentId::try_new(COMPONENT)?,
                json!({"query": QUERY}),
                scope(1),
                provider(directory.path())?,
                Some(owner.clone()),
            )
            .is_err(),
        "a second constructor cannot claim this actual progress resource"
    );
    query.start().await?;
    let first = query
        .transform(ordinary_input(5, 20, 10, &[0, 255, 5])?)
        .await?;
    aggregate(&first, Some(0.0), 10.0, 1)?;
    query.delivery_completed(&first).await?;
    let key = SourceProgressKey::Source("source".into());
    let stream = StreamId::try_new("source/out")?;
    assert_eq!(owner.snapshot().checkpoints[&key].sequence, 5);
    assert_eq!(
        owner.snapshot().checkpoints[&key]
            .source_position
            .as_deref(),
        Some(&[0, 255, 5][..])
    );
    assert!(query
        .transform(ordinary_input(5, 50, 999, b"duplicate-position")?)
        .await?
        .is_empty());
    assert_eq!(
        owner.snapshot().checkpoints[&key]
            .source_position
            .as_deref(),
        Some(&[0, 255, 5][..])
    );
    assert_eq!(owner.snapshot().transport_sequences[&stream], 50);
    let next = query
        .transform(ordinary_input(6, 40, 12, &[0, 255, 6])?)
        .await?;
    aggregate(&next, Some(10.0), 12.0, 2)?;
    query.delivery_completed(&next).await?;
    assert_eq!(owner.snapshot().transport_sequences[&stream], 50);
    query.stop().await?;
    drop(query);
    let mut query = factory()?.create_component_with_processing_owner(
        ComponentId::try_new(COMPONENT)?,
        json!({"query": QUERY}),
        scope(2),
        provider(directory.path())?,
        Some(owner.clone()),
    )?;
    query.start().await?;
    assert_eq!(owner.snapshot().checkpoints[&key].sequence, 6);
    assert_eq!(owner.snapshot().transport_sequences[&stream], 50);
    let next = query
        .transform(ordinary_input(7, 80, 15, &[0, 255, 7])?)
        .await?;
    aggregate(&next, Some(12.0), 15.0, 3)?;
    query.delivery_completed(&next).await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_reuses_ordinary_source_progress_current_thread() -> Result<()> {
    shared_ordinary_progress().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_query_reuses_ordinary_source_progress_multi_thread() -> Result<()> {
    shared_ordinary_progress().await
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_reuses_core_persistent_producer_gap_checks() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), QUERY, 1)?;
    let owner = query.processing_progress_owner().context("actual owner")?;
    query.start().await?;
    let identity = GraphProducerIdentity::new(
        GRAPH.into(),
        GRAPH.into(),
        ComponentId::try_new("source")?,
        StreamId::try_new("source/out")?,
        true,
    )?;
    let mut first = input(
        SourceChange::Replace {
            element: item(1, 1),
        },
        10,
    )?;
    GraphProducerProgress::annotate(&mut first.envelope, &identity, 1)?;
    let output = query.transform(first).await?;
    query.delivery_completed(&output).await?;
    let mut gap = input(
        SourceChange::Replace {
            element: item(3, 3),
        },
        20,
    )?;
    GraphProducerProgress::annotate(&mut gap.envelope, &identity, 3)?;
    let error = query
        .transform(gap)
        .await
        .expect_err("missing logical output must fail");
    assert!(format!("{error:#}").contains("gap"));
    let key = SourceProgressKey::Stream(identity.stream().clone());
    assert_eq!(owner.snapshot().checkpoints[&key].sequence, 1);
    query.stop().await?;
    drop(query);
    let mut query = create(directory.path(), QUERY, 2)?;
    query.start().await?;
    let mut next = input(
        SourceChange::Replace {
            element: item(2, 2),
        },
        30,
    )?;
    GraphProducerProgress::annotate(&mut next.envelope, &identity, 2)?;
    let output = query.transform(next).await?;
    aggregate(&output, Some(1.0), 2.0, 2)?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_complete_transaction_mode_requires_framed_input() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let limits = SourceTransactionLimits {
        max_changes: std::num::NonZeroUsize::new(8).context("positive change limit")?,
        max_bytes: std::num::NonZeroUsize::new(16 << 10).context("positive byte limit")?,
        max_duration_ms: std::num::NonZeroU64::new(10_000).context("positive deadline")?,
    };
    let mut query = factory()?.create_component_with_processing(
        ComponentId::try_new(COMPONENT)?,
        json!({"query": QUERY, "source_transactions": limits}),
        scope(1),
        provider(directory.path())?,
    )?;
    let owner = query.processing_progress_owner().context("actual owner")?;
    query.start().await?;
    assert!(query
        .transform(input(
            SourceChange::Replace {
                element: item(5, 1)
            },
            1
        )?)
        .await
        .is_err());
    assert!(owner.snapshot().checkpoints.is_empty());
    query.stop().await?;
    Ok(())
}

fn positioned_input(
    stream: &str,
    value: serde_json::Value,
    sequence: u64,
    position: Option<&[u8]>,
) -> Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(stream, "one"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: sequence,
        },
        properties: ElementPropertyMap::from(json!({"value":value})),
    };
    let original = GraphChangeCodec::encode_change(
        SourceChange::Replace { element },
        StreamId::try_new(stream)?,
        sequence,
        None,
    )?;
    let mut system = original.system().as_ref().clone();
    if let Some(position) = position {
        system = system.with_source_position(position.to_vec().into());
    }
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: ChangeEnvelope::new(original.id().clone(), original.changes().clone(), system),
    })
}

async fn actual_loaded_progress() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let text = "MATCH (n:Item) RETURN abs(n.value) AS value";
    let mut query = create(directory.path(), text, 1)?;
    let view = query
        .committed_input_progress()?
        .context("actual indexed progress reader")?;
    assert!(view.same_owner(&query.committed_input_progress()?.context("same reader")?));
    assert_eq!(view.graph_id(), GRAPH);
    assert_eq!(view.component_id().as_str(), COMPONENT);
    assert!(!view.snapshot()?.recovered);
    query.start().await?;
    let ready = view.wait_ready().await?;
    assert!(ready.recovered && ready.persistent && ready.admitting && ready.bootstrap_complete);
    assert!(ready.checkpoints.is_empty());
    let first = positioned_input("source/out", json!(-10), 0, Some(b"\0real-zero"))?;
    let output = query.transform(first.clone()).await?;
    let committed = view.snapshot()?;
    let stream = StreamId::try_new("source/out")?;
    let key = SourceProgressKey::Stream(stream.clone());
    assert_eq!(committed.checkpoints[&key].sequence, 0);
    assert_eq!(
        committed.checkpoints[&key].source_position.as_deref(),
        Some(b"\0real-zero".as_slice())
    );
    assert_eq!(committed.transport_sequences[&stream], 0);
    query.delivery_completed(&output).await?;
    assert!(query.transform(first).await?.is_empty());
    let other = query
        .transform(positioned_input("other/out", json!(5), 7, None)?)
        .await?;
    query.delivery_completed(&other).await?;
    let current = view.snapshot()?;
    assert_eq!(current.checkpoints.len(), 2);
    assert_eq!(current.checkpoints[&key].sequence, 0);
    assert_eq!(
        current.checkpoints[&SourceProgressKey::Stream(StreamId::try_new("other/out")?)].sequence,
        7
    );
    assert!(query
        .transform(positioned_input(
            "source/out",
            json!("invalid"),
            1,
            Some(b"uncommitted")
        )?)
        .await
        .is_err());
    let failed = view.snapshot()?;
    assert!(!failed.ready && !failed.admitting && failed.failure.is_some());
    assert_eq!(failed.checkpoints, current.checkpoints);
    query.stop().await?;
    assert!(view.snapshot().is_err());
    drop(query);
    let mut query = create(directory.path(), text, 2)?;
    let restored = query
        .committed_input_progress()?
        .context("restored progress")?;
    query.start().await?;
    let recovered = restored.wait_ready().await?;
    assert_eq!(recovered.checkpoints, current.checkpoints);
    assert_eq!(recovered.transport_sequences, current.transport_sequences);
    assert!(recovered.failure.is_none());
    let output = query
        .transform(positioned_input(
            "source/out",
            json!(-12),
            1,
            Some(b"committed"),
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    assert_eq!(restored.snapshot()?.checkpoints[&key].sequence, 1);
    let mut unsupported = positioned_input("source/out", json!(15), 2, None)?;
    let producer = GraphProducerIdentity::new(
        GRAPH.into(),
        GRAPH.into(),
        ComponentId::try_new("source")?,
        StreamId::try_new("source/out")?,
        true,
    )?;
    GraphProducerProgress::annotate(&mut unsupported.envelope, &producer, 2)?;
    assert!(
        query.transform(unsupported).await.is_err(),
        "unsupported input must fail before storage"
    );
    let rejected = restored.snapshot()?;
    assert!(
        rejected.failure.is_some(),
        "outer component failure must fence its progress view"
    );
    assert_eq!(rejected.checkpoints[&key].sequence, 1);
    query.stop().await?;
    assert!(restored.snapshot().is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_query_publishes_actual_committed_progress_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), actual_loaded_progress()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_query_publishes_actual_committed_progress_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), actual_loaded_progress()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn volatile_query_does_not_acquire_a_committed_progress_view() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/volatile-query"
        })
        .context("volatile factory")?;
    let mut query = factory.create_component(
        ComponentId::try_new("volatile")?,
        json!({"query":QUERY, "volatile":true}),
    )?;
    assert!(query.committed_input_progress()?.is_none());
    query.start().await?;
    assert!(query.committed_input_progress()?.is_none());
    query.stop().await?;
    Ok(())
}
