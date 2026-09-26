// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};

use anyhow::Result;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{
    channels::{QueryResult, SourceEvent, SourceEventWrapper},
    computation::v1::*,
    ReactionBase, ReactionBaseParams, ReactionRecoveryPolicy,
};

#[tokio::test]
async fn backwards_timestamps_do_not_skip_unprocessed_source_changes() -> Result<()> {
    let queue_id = ResourceId::try_new("inputs")?;
    let queue = RankedInputQueue::new(8)?;
    let mut pipe = RankedInputPipeConfig {
        queue: queue_id.clone(),
        capacity: 8,
        source_rank: 0,
        source_id: Some("source".into()),
        drop_when_full: false,
    }
    .create_with_resources(&BTreeMap::from([(queue_id, queue.resource())]))?;
    for (sequence, timestamp) in [(1, 2000), (2, 1000)] {
        let wrapper = Arc::new(SourceEventWrapper::with_sequence(
            "source".into(),
            SourceEvent::Change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: timestamp as u64,
                    },
                    properties: ElementPropertyMap::default(),
                },
            }),
            chrono::DateTime::from_timestamp_millis(timestamp).unwrap(),
            sequence,
            None,
        ));
        pipe.pipe
            .sender()
            .send(GraphChangeCodec::encode_source_event(
                wrapper,
                &ComponentId::try_new("source")?,
                StreamId::try_new("source/out")?,
                sequence,
                None,
            )?)
            .await?;
    }
    let mut query = TransactionTransformer::query(
        ContinuousQueryDefinition {
            graph_id: "order".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) RETURN n".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await?;
    query.start().await?;
    let mut receiver = pipe.pipe.take_receiver()?;
    for expected in [1, 2] {
        let delivery = receiver.receive().await?.unwrap();
        assert_eq!(
            GraphChangeCodec::source_metadata(delivery.envelope())?
                .unwrap()
                .sequence,
            Some(expected)
        );
        let output = query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: delivery.into_parts().0,
            })
            .await?;
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].envelope.changes().operations().len(), 1);
        query.delivery_completed(&output).await?;
    }
    assert_eq!(query.query_results().unwrap().snapshot()?.rows.len(), 2);
    query.stop().await?;
    queue.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn backwards_result_timestamps_do_not_skip_reaction_side_effects() -> Result<()> {
    let base = ReactionBase::new(ReactionBaseParams::new("reaction", vec!["query".into()]));
    let (updates, _receiver) = tokio::sync::mpsc::channel(32);
    base.initialize(drasi_lib::context::ReactionRuntimeContext::new(
        "ordering",
        "reaction",
        Some(Arc::new(
            drasi_lib::state_store::MemoryStateStoreProvider::new(),
        )),
        updates,
        None,
    ))
    .await;
    for (sequence, timestamp) in [(1, 2000), (2, 1000), (3, 3000)] {
        base.enqueue_query_result(QueryResult {
            query_id: "query".into(),
            sequence,
            timestamp: chrono::DateTime::from_timestamp_millis(timestamp).unwrap(),
            results: Vec::new(),
            metadata: Default::default(),
            profiling: None,
        })
        .await?;
    }
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let finished = Arc::new(tokio::sync::Notify::new());
    let handled = seen.clone();
    let done = finished.clone();
    let run = base.run_standard_loop(
        base.create_shutdown_channel().await,
        Default::default(),
        ReactionRecoveryPolicy::Strict,
        move |event| {
            let handled = handled.clone();
            let done = done.clone();
            async move {
                handled.lock().await.push(event.sequence);
                if event.sequence == 3 {
                    done.notify_one();
                }
                Ok(())
            }
        },
    );
    let stop = async {
        tokio::time::timeout(Duration::from_secs(3), finished.notified()).await?;
        base.stop_common().await
    };
    let (run, stop) = tokio::join!(run, stop);
    run?;
    stop?;
    assert_eq!(*seen.lock().await, [1, 2, 3]);
    assert_eq!(base.read_checkpoint("query").await?.unwrap().sequence, 3);
    Ok(())
}

#[tokio::test]
async fn wal_subscription_recovers_without_a_native_cursor_and_reports_real_gaps() -> Result<()> {
    use drasi_lib::{
        sources::{SourceBase, SourceBaseParams, SourceError},
        wal::{WalProvider, WriteAheadLogConfig},
    };
    let directory = tempfile::tempdir()?;
    let wal = drasi_wal_redb::RedbWalProvider::new(directory.path());
    wal.register("source", WriteAheadLogConfig::default())
        .await?;
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "one"),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 1,
            },
            properties: ElementPropertyMap::default(),
        },
    };
    assert_eq!(wal.append("source", &change).await?, 1);
    let base = SourceBase::new(SourceBaseParams::new("source"))?;
    let settings = drasi_lib::SourceSubscriptionSettings {
        source_id: "source".into(),
        query_id: "query".into(),
        enable_bootstrap: false,
        nodes: Default::default(),
        relations: Default::default(),
        resume_from: None,
        resume_sequence: Some(0),
        request_position_handle: true,
    };
    let mut subscription = base.subscribe_with_wal(&settings, &wal, "test").await?;
    let replayed =
        tokio::time::timeout(Duration::from_secs(3), subscription.receiver.recv()).await??;
    assert_eq!(replayed.sequence, 1);
    assert_eq!(
        replayed.source_position,
        Some(bytes::Bytes::copy_from_slice(&1u64.to_be_bytes()))
    );
    assert!(matches!(&replayed.event, SourceEvent::Change(actual) if actual == &change));
    wal.prune_up_to("source", 1).await?;
    assert_eq!(wal.append("source", &change).await?, 2);
    let error = match base.subscribe_with_wal(&settings, &wal, "test").await {
        Ok(_) => anyhow::bail!("pruned history must not turn into an empty successful replay"),
        Err(error) => error,
    };
    assert!(matches!(
        error.downcast_ref::<SourceError>(),
        Some(SourceError::PositionUnavailable { .. })
    ));
    base.stop_common().await?;
    Ok(())
}
