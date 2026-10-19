// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::FailureMode,
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::*;
use drasi_reaction_application::NativeApplicationReaction;
use drasi_source_application::NativeApplicationSource;
use std::{
    collections::BTreeMap, future::Future, num::NonZeroUsize, path::Path, sync::Arc, time::Duration,
};
use tokio::sync::mpsc;

const GRAPH: &str = "native-application-admission";

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero test bound")
}

fn event(sequence: u64) -> Result<ChangeEnvelope> {
    GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("input", &sequence.to_string()),
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: Default::default(),
            },
        },
        StreamId::try_new("input/out")?,
        sequence,
        chrono::DateTime::from_timestamp_millis(i64::try_from(sequence)?),
    )
    .map_err(Into::into)
}

async fn channel(directory: &Path) -> Result<Arc<QosChannel>> {
    let provider = LegacyIndexProviderAdapter::new(Arc::new(
        RocksDbIndexProvider::new(directory, false, false).with_memory_budget_bytes(32 << 20)?,
    ));
    let mut codec = EnvelopeCodec::new(size(1024 * 1024));
    codec.register_schema(GraphChangeCodec::schema())?;
    Ok(QosChannel::persistent_with_recovery(
        QosChannelDefinition {
            stream: StreamId::try_new("input/out")?,
            capacity: size(4),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("capture".into(), SubscriptionStart::Earliest)]),
        },
        provider.create_indexes(GRAPH, "input-journal").await?,
        codec,
        "application",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: GRAPH.into(),
            graph_id: GRAPH.into(),
            component_id: ComponentId::try_new("input")?,
            failure_scope: FailureMode::ProcessRestart,
            max_producers: size(2),
            receipts_per_producer: size(4),
        }),
    )
    .await?)
}

struct Capture {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<ChangeEnvelope>,
    block: bool,
}

#[async_trait]
impl ComputationComponent for Capture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        if self.block {
            std::future::pending::<()>().await;
        }
        self.output.send(input.envelope).await?;
        Ok(())
    }
}

fn graph(
    admission: &Arc<SourceAdmission>,
    block: bool,
) -> Result<(ComputationGraph, mpsc::Receiver<ChangeEnvelope>)> {
    let source = NativeApplicationSource::durable("input", admission.clone())?;
    assert!(source.get_handle().is_none());
    assert_eq!(source.configuration()?["durable"], true);
    let output = source.output();
    let journal = ResourceId::try_new("journal")?;
    let (sender, receiver) = mpsc::channel(8);
    let graph = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: journal.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "application-input".into(),
        })?
        .provide_resource(journal.clone(), admission.channel().resource())?
        .source(Box::new(source))
        .sink(Box::new(Capture {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("capture")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            output: sender,
            block,
        }))
        .bind_stream(output.clone(), StreamId::try_new("input/out")?)
        .connect(
            EdgeDefinition::new(
                output,
                Endpoint::new(ComponentId::try_new("capture")?, PortId::try_new("in")?),
            ),
            Box::new(admission.channel().definition().pipe(journal, "capture")),
        )
        .build()?;
    let recovery = graph.recovery_report(&RecoveryRequirement {
        consumer: ComponentId::try_new("input")?,
        scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
        guarantees: [RecoveryGuarantee::Acceptance].into(),
    })?;
    assert!(recovery.satisfied(), "{recovery:?}");
    Ok((graph, receiver))
}

async fn admit(
    admission: &SourceAdmission,
    session: &ProducerSession,
    sequence: u64,
    input: &ChangeEnvelope,
) -> Result<AdmissionReceipt, PipeError> {
    loop {
        match admission.admit(session, sequence, input).await {
            Err(PipeError::Backend(error))
                if error.downcast_ref::<AdmissionRejection>()
                    == Some(&AdmissionRejection::Busy) =>
            {
                tokio::task::yield_now().await;
            }
            result => return result,
        }
    }
}

async fn receive(receiver: &mut mpsc::Receiver<ChangeEnvelope>, id: &str) -> Result<()> {
    let envelope = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await?
        .context("application delivery closed")?;
    assert_eq!(
        GraphChangeCodec::decode_changes(&envelope)?[0]
            .get_reference()
            .element_id
            .as_ref(),
        id
    );
    let progress =
        GraphProducerProgress::from_envelope(&envelope)?.context("durable producer identity")?;
    assert!(progress.identity().persistent());
    Ok(())
}

async fn active(admission: &SourceAdmission) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !admission.is_active()? {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn lifecycle_contract() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let channel = channel(directory.path()).await?;
    let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
    assert!(matches!(
        admission
            .register_producer(ComponentId::try_new("client")?)
            .await,
        Err(PipeError::Closed)
    ));
    assert!(NativeApplicationSource::durable("another-source", admission.clone()).is_err());
    let wrong_port = SourceAdmission::new(channel.clone(), PortId::try_new("elsewhere")?).await?;
    assert!(NativeApplicationSource::durable("input", wrong_port).is_err());
    {
        let (mut graph, mut delivered) = graph(&admission, false)?;
        let run = graph.start()?;
        let control = run.control();
        let work = async {
            let result = async {
                control.startup_report().await?;
                active(&admission).await?;
                let session = admission.register_producer(ComponentId::try_new("client")?).await?;
                let input = event(1)?;
                let receipt = admit(&admission, &session, 1, &input).await?;
                receive(&mut delivered, "1").await?;
                assert_eq!(admit(&admission, &session, 1, &input).await?, receipt);
                assert_eq!(admission.admission_receipt(&session, 1).await?, Some(receipt));
                let conflict = admit(&admission, &session, 1, &event(2)?).await;
                assert!(matches!(conflict, Err(PipeError::Backend(error))
                    if error.downcast_ref::<AdmissionRejection>() == Some(&AdmissionRejection::PayloadConflict(1))));
                assert_eq!(channel.progress().await?.accepted, 1);
                let source = control.component_handle(&ComponentId::try_new("input")?)?;
                source.stop().await?;
                assert!(matches!(admission.admit(&session, 2, &event(2)?).await, Err(PipeError::Closed)));
                assert_eq!(channel.progress().await?.accepted, 1);
                source.start().await?;
                active(&admission).await?;
                assert_eq!(admission.register_producer(ComponentId::try_new("client")?).await?, session);
                admit(&admission, &session, 2, &event(2)?).await?;
                receive(&mut delivered, "2").await?;
                Ok::<_, anyhow::Error>(())
            }.await;
            control.cancel();
            result
        };
        let (run, work) =
            tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(run, work) })
                .await?;
        let cleanup = graph.shutdown().await;
        work?;
        assert!(matches!(run, Err(GraphError::Cancelled)));
        cleanup?;
    }
    channel.shutdown().await?;
    Ok(())
}

async fn reopen_contract() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut accepted = None;
    for blocked in [true, false] {
        let channel = channel(directory.path()).await?;
        let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
        {
            let (mut graph, mut delivered) = graph(&admission, blocked)?;
            let run = graph.start()?;
            let control = run.control();
            let work = async {
                let result = async {
                    control.startup_report().await?;
                    active(&admission).await?;
                    let session = admission
                        .register_producer(ComponentId::try_new("client")?)
                        .await?;
                    let receipt = admit(&admission, &session, 1, &event(1)?).await?;
                    if let Some(original) = &accepted {
                        assert_eq!(&receipt, original);
                    } else {
                        accepted = Some(receipt.clone());
                    }
                    assert_eq!(
                        admission.admission_receipt(&session, 1).await?,
                        Some(receipt)
                    );
                    assert_eq!(channel.progress().await?.accepted, 1);
                    if !blocked {
                        receive(&mut delivered, "1").await?;
                        assert!(
                            tokio::time::timeout(Duration::from_millis(20), delivered.recv())
                                .await
                                .is_err()
                        );
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                control.cancel();
                result
            };
            let (run, work) =
                tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(run, work) })
                    .await?;
            let cleanup = graph.shutdown().await;
            work?;
            assert!(matches!(run, Err(GraphError::Cancelled)));
            cleanup?;
        }
        channel.shutdown().await?;
    }
    Ok(())
}

fn both_runtimes<F, Fut>(scenario: F) -> Result<()>
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
        builder.enable_all().build()?.block_on(scenario())?;
    }
    Ok(())
}

#[test]
fn native_application_admission_preserves_receipts_and_rejects_stopped_sends() -> Result<()> {
    both_runtimes(lifecycle_contract)
}

#[test]
fn native_application_reopen_replays_unhandled_input_without_accepting_retries_twice() -> Result<()>
{
    both_runtimes(reopen_contract)
}

async fn query_contract() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let journal = channel(directory.path()).await?;
    let admission = SourceAdmission::new(journal.clone(), PortId::try_new("out")?).await?;
    let source = NativeApplicationSource::durable("input", admission.clone())?;
    let output = source.output();
    let provider = LegacyIndexProviderAdapter::new(Arc::new(
        RocksDbIndexProvider::new(directory.path(), false, false)
            .with_memory_budget_bytes(32 << 20)?,
    ));
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) RETURN count(n) AS count".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: size(16),
        },
        provider,
    )
    .await?
    .with_recovery_scope(GRAPH)?
    .with_delivery_tracking();
    let rows = query.results();
    let (reaction, mut received) = NativeApplicationReaction::channel(
        "application",
        QueryChangeCodec::schema().descriptor().clone(),
        size(8),
    )?;
    let reaction_input = reaction.input();
    let resource = ResourceId::try_new("journal")?;
    let query_output = Endpoint::new(ComponentId::try_new("query")?, PortId::try_new("out")?);
    let mut graph = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "application-input".into(),
        })?
        .provide_resource(resource.clone(), journal.resource())?
        .source(Box::new(source))
        .transformer(Box::new(query))
        .sink(Box::new(reaction))
        .bind_stream(output.clone(), StreamId::try_new("input/out")?)
        .bind_stream(query_output.clone(), StreamId::try_new("query/out")?)
        .connect(
            EdgeDefinition::new(
                output,
                Endpoint::new(ComponentId::try_new("query")?, PortId::try_new("in")?),
            ),
            Box::new(journal.definition().pipe(resource, "capture")),
        )
        .connect(
            EdgeDefinition::new(query_output, reaction_input),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        )
        .build()?;
    let run = graph.start()?;
    let control = run.control();
    let work = async {
        let result = async {
            control.startup_report().await?;
            active(&admission).await?;
            let session = admission
                .register_producer(ComponentId::try_new("client")?)
                .await?;
            let receipt = admit(&admission, &session, 1, &event(1)?).await?;
            let result = tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await?
                .context("native application did not receive query output")?;
            assert_eq!(result.envelope.system().stream().as_str(), "query/out");
            assert_eq!(
                QueryChangeCodec::metadata(&result.envelope)?.query_id,
                "query"
            );
            let delivered = result
                .envelope
                .changes()
                .operations()
                .iter()
                .filter_map(|operation| match operation {
                    ChangeOperation::Added { after, .. }
                    | ChangeOperation::Updated { after, .. } => Some(after),
                    _ => None,
                })
                .map(|record| {
                    Ok(QueryChangeCodec::row_values_to_json(
                        &QueryChangeCodec::decode_row(record)?.values,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(delivered, [serde_json::json!({"count": 1})]);
            let snapshot = rows.snapshot()?;
            assert_eq!(snapshot.rows.len(), 1);
            let row = snapshot.rows.values().next().context("aggregate row")?;
            assert_eq!(
                QueryChangeCodec::row_values_to_json(&QueryChangeCodec::decode_row(row)?.values)
                    ["count"],
                1
            );
            assert_eq!(admit(&admission, &session, 1, &event(1)?).await?, receipt);
            assert_eq!(journal.progress().await?.accepted, 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        control.cancel();
        result
    };
    let (run, work) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(run, work) }).await?;
    let cleanup = graph.shutdown().await;
    work?;
    assert!(matches!(run, Err(GraphError::Cancelled)));
    cleanup?;
    drop(graph);
    journal.shutdown().await?;
    Ok(())
}

#[test]
fn native_durable_application_input_runs_through_a_persistent_query_and_native_reaction(
) -> Result<()> {
    both_runtimes(query_contract)
}
