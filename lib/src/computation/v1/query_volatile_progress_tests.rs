// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{
    BootstrapWatermark, ChangeEnvelope, ComputationBootstrapSnapshot, QueryOutputMetadata,
};
use drasi_computation_contracts::exchange::{SourceEvent, SourceEventWrapper};
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use futures::StreamExt;
use std::{pin::Pin, time::Duration};

fn event(logical: u64, transport: u64, position: &'static [u8]) -> anyhow::Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "person"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: logical,
        },
        properties: ElementPropertyMap::from(serde_json::json!({"name": format!("row-{logical}")})),
    };
    let source = SourceEventWrapper::new(
        "source".into(),
        SourceEvent::Change(SourceChange::Insert { element }),
        Utc::now(),
        logical,
    )
    .with_source_position(Bytes::from_static(position));
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(source),
            &ComponentId::try_new("source")?,
            StreamId::try_new("source/out")?,
            transport,
            None,
        )?,
    })
}

async fn query() -> anyhow::Result<(ContinuousQueryTransformer, Arc<QuerySourceProgress>)> {
    let owner = Arc::new(QuerySourceProgress::new(
        "volatile",
        ComponentId::try_new("query")?,
    )?);
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "volatile".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?
    .with_source_progress(owner.clone())?;
    Ok((query, owner))
}

#[tokio::test(flavor = "current_thread")]
async fn volatile_progress_retains_handled_cursor_and_monotonic_duplicate_transport(
) -> anyhow::Result<()> {
    let (mut query, owner) = query().await?;
    query.start().await?;
    query.transform(event(1, 10, &[0, 255, 1])?).await?;
    let key = SourceProgressKey::Source("source".into());
    let stream = StreamId::try_new("source/out")?;
    assert!(!owner.snapshot().persistent);
    assert_eq!(
        owner.snapshot().checkpoints[&key]
            .source_position
            .as_deref(),
        Some(&[0, 255, 1][..])
    );
    assert_eq!(owner.snapshot().transport_sequences[&stream], 10);
    assert!(query
        .transform(event(1, 90, b"must-not-replace-committed-position")?)
        .await?
        .is_empty());
    assert_eq!(owner.snapshot().checkpoints[&key].sequence, 1);
    assert_eq!(
        owner.snapshot().checkpoints[&key]
            .source_position
            .as_deref(),
        Some(&[0, 255, 1][..])
    );
    assert_eq!(owner.snapshot().transport_sequences[&stream], 90);
    assert!(query
        .transform(event(1, 20, b"older-transport")?)
        .await?
        .is_empty());
    assert_eq!(owner.snapshot().transport_sequences[&stream], 90);
    let invalid = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("foreign")?,
            SystemMetadata::new(StreamId::try_new("foreign/out")?, 100),
            &[drasi_core::evaluation::context::QueryPartEvaluationContext::Adding {
                after: BTreeMap::new(),
                row_signature: 1,
            }],
            QueryOutputMetadata {
                query_id: "foreign".into(),
                source_id: None,
                timestamp: Utc::now(),
                metadata: Default::default(),
                profiling: None,
            },
        )?
        .ok_or_else(|| anyhow::anyhow!("missing invalid-schema input"))?,
    };
    assert!(query.transform(invalid).await.is_err());
    assert_eq!(owner.snapshot().checkpoints[&key].sequence, 1);
    assert_eq!(owner.snapshot().transport_sequences[&stream], 90);
    assert!(owner.snapshot().failure.is_some());
    query.stop().await?;
    Ok(())
}

enum End {
    Complete,
    Failure,
    Cancel(Arc<tokio::sync::Notify>),
}
struct Snapshot(End);
#[async_trait]
impl ComputationBootstrapProvider for Snapshot {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let first = futures::stream::iter([Ok(event(1, 1, b"row-not-a-watermark")?.envelope)]);
        let end: Pin<Box<dyn futures::Stream<Item = anyhow::Result<ChangeEnvelope>> + Send>> =
            match &self.0 {
                End::Complete => Box::pin(futures::stream::empty()),
                End::Failure => Box::pin(futures::stream::iter([Err(anyhow::anyhow!(
                    "snapshot failed"
                ))])),
                End::Cancel(ready) => {
                    let ready = ready.clone();
                    Box::pin(futures::stream::once(async move {
                        ready.notify_one();
                        std::future::pending().await
                    }))
                }
            };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(first.chain(end)),
            watermarks: vec![BootstrapWatermark {
                stream: StreamId::try_new("source/out")?,
                source_id: Some("source".into()),
                sequence: 7,
                position: Some(Bytes::from_static(b"actual-bootstrap-boundary")),
            }],
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn volatile_bootstrap_cursor_is_published_only_after_complete_snapshot() -> anyhow::Result<()>
{
    let (query, owner) = query().await?;
    let mut query = query.with_bootstrap(Arc::new(Snapshot(End::Complete)));
    query.start().await?;
    let key = SourceProgressKey::Source("source".into());
    assert!(owner.snapshot().ready && owner.snapshot().bootstrap_complete);
    assert!(!owner.snapshot().persistent);
    assert_eq!(owner.snapshot().checkpoints[&key].sequence, 7);
    assert_eq!(
        owner.snapshot().checkpoints[&key]
            .source_position
            .as_deref(),
        Some(b"actual-bootstrap-boundary".as_slice())
    );
    query.transform(event(8, 80, b"handled-live-row")?).await?;
    assert_eq!(
        owner.snapshot().checkpoints[&key]
            .source_position
            .as_deref(),
        Some(b"handled-live-row".as_slice())
    );
    assert_eq!(
        owner.snapshot().transport_sequences[&StreamId::try_new("source/out")?],
        80
    );
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_or_cancelled_volatile_bootstrap_never_confirms_its_watermark() -> anyhow::Result<()>
{
    for cancelled in [false, true] {
        let (query, owner) = query().await?;
        let ready = Arc::new(tokio::sync::Notify::new());
        let end = if cancelled {
            End::Cancel(ready.clone())
        } else {
            End::Failure
        };
        let mut query = query.with_bootstrap(Arc::new(Snapshot(end)));
        if cancelled {
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::select! {
                    result = query.start() => anyhow::bail!("unexpected completion: {result:?}"),
                    _ = ready.notified() => Ok(()),
                }
            })
            .await??;
        } else {
            assert!(query.start().await.is_err());
        }
        let progress = owner.snapshot();
        assert!(!progress.ready && !progress.persistent && !progress.bootstrap_complete);
        assert!(progress.failure.is_some());
        assert!(progress.checkpoints.is_empty() && progress.transport_sequences.is_empty());
        query.stop().await?;
    }
    Ok(())
}
