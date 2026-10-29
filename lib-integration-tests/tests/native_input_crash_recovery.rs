// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native input crash qualification, not a source-WAL or external exactly-once test.
//! The parent SIGKILLs a worker with accepted input beyond a held query transaction;
//! a different process opens the same real RocksDB stores and finishes the graph.
//! Profiles cover lossless admission QoS, standalone retained input, and a shared
//! journal owned by one built-in durable middleware processor (not by the query).
//! Volatile/lossy inputs, memory query state and connector-specific durability are
//! intentionally outside this contract. Capture is an observed test destination;
//! the selected boundaries do not claim atomic external effects.

#![cfg(unix)]

#[path = "../../lib/tests/computation_support/persistent_faults.rs"]
mod persistent_faults;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{FailureMode, IndexError, IndexSet, OutboxPageLimits, OutboxWriter},
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use drasi_source_application::{NativeApplicationSource, PropertyMapBuilder};
use persistent_faults::{
    process::{write_boundary, OwnedChild},
    provider, Fault, FaultProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::mpsc;

const GRAPH: &str = "native-input-crash";
const WORKER: &str = "native_input_crash_worker";
const TOTAL: u64 = 12;
const PREFIX: u64 = 3;
const DEADLINE: Duration = Duration::from_secs(15);

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero fixture capacity")
}

fn endpoint(component: &str, port: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
}

fn codec() -> Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(size(1 << 20));
    codec.register_schema(GraphChangeCodec::schema())?;
    Ok(codec)
}

fn event(sequence: u64) -> Result<ChangeEnvelope> {
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("input", &sequence.to_string()),
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: PropertyMapBuilder::new()
                    .with_string("id", format!("item-{sequence:02}"))
                    .with_integer("value", (sequence * 17) as i64)
                    .build(),
            },
        },
        StreamId::try_new("input/out")?,
        sequence,
        chrono::DateTime::from_timestamp_millis(sequence as i64),
    )?)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum Boundary {
    QueryBeforeCommit,
    QueryAfterCommit,
    AdmissionLostResponse,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Plan {
    current_thread: bool,
    boundary: Boundary,
    profile: Profile,
    paged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Profile {
    Admission,
    Retained,
    Shared,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = File::create(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Only the actual result append arms the session fault. Startup, output handoff
/// and other metadata transactions cannot accidentally become the crash boundary.
struct ArmingOutbox {
    inner: Arc<dyn OutboxWriter>,
    fault: Arc<Fault>,
    mode: u8,
}

impl ArmingOutbox {
    fn arm(&self, key: &str, sequence: u64) {
        if key == "query" && sequence == PREFIX + 1 {
            self.fault.mode.store(self.mode, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl OutboxWriter for ArmingOutbox {
    async fn append(&self, key: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        self.inner.append(key, sequence, data).await?;
        self.arm(key, sequence);
        Ok(())
    }
    async fn append_and_trim(
        &self,
        key: &str,
        sequence: u64,
        data: &[u8],
        floor: u64,
    ) -> Result<usize, IndexError> {
        let result = self
            .inner
            .append_and_trim(key, sequence, data, floor)
            .await?;
        self.arm(key, sequence);
        Ok(result)
    }
    async fn read_from(&self, key: &str, after: u64) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(key, after).await
    }
    async fn read_page(
        &self,
        key: &str,
        after: u64,
        limits: OutboxPageLimits,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_page(key, after, limits).await
    }
    async fn read_latest_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(key).await
    }
    async fn trim_before(&self, key: &str, floor: u64) -> Result<usize, IndexError> {
        self.inner.trim_before(key, floor).await
    }
    async fn trim_to_capacity(&self, key: &str, capacity: usize) -> Result<usize, IndexError> {
        self.inner.trim_to_capacity(key, capacity).await
    }
    async fn clear(&self, key: &str) -> Result<(), IndexError> {
        self.inner.clear(key).await
    }
}

struct QueryProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    fault: Arc<Fault>,
    mode: u8,
}

#[async_trait]
impl ComputationIndexProvider for QueryProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = FaultProvider {
            inner: self.inner.clone(),
            fault: self.fault.clone(),
        }
        .create_indexes(graph, id)
        .await?;
        let domain = TransactionDomain::new(original.indexes().session_control.clone());
        let outbox: Arc<dyn OutboxWriter> = Arc::new(ArmingOutbox {
            inner: original.outbox_writer().expect("actual outbox").clone(),
            fault: self.fault.clone(),
            mode: self.mode,
        });
        let set = original.indexes();
        let wrapped = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: set.session_control.clone(),
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original
                    .checkpoint_store()
                    .expect("actual checkpoints")
                    .clone(),
                &domain,
            )),
            Some(ComputationResource::participating(outbox, &domain)),
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("actual projection")
                    .clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(wrapped
            .with_cleanup(original.cleanup().expect("actual cleanup owner").clone())
            .with_durability(original.durability()))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

struct Capture {
    descriptor: ComponentDescriptor,
    seen: mpsc::Sender<Value>,
    file: File,
}

fn output_evidence(envelope: &ChangeEnvelope) -> Result<Value> {
    let producer = GraphProducerProgress::from_query_envelope(envelope)?
        .context("persistent query producer")?;
    assert!(producer.identity().persistent());
    let identity = QueryRecoveryIdentity::from_envelope(envelope)?;
    assert_eq!(identity.incarnation(), None);
    let operations = envelope
        .changes()
        .operations()
        .iter()
        .map(|operation| match operation {
            ChangeOperation::Added { after, .. } => Ok(json!({
                "kind": "added",
                "row": QueryChangeCodec::row_values_to_json(
                    &QueryChangeCodec::decode_row(after)?.values
                ),
            })),
            other => anyhow::bail!("unexpected output operation: {other:?}"),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "identity": identity,
        "producer": producer,
        "sequence": QueryChangeCodec::query_sequence(envelope)?,
        "generation": QueryChangeCodec::query_generation(envelope)?,
        "operations": operations,
    }))
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
        let evidence = output_evidence(&input.envelope)?;
        serde_json::to_writer(&mut self.file, &evidence)?;
        self.file.write_all(b"\n")?;
        self.file.sync_all()?;
        self.seen.send(evidence).await?;
        Ok(())
    }
}

struct Fixture {
    graph: ComputationGraph,
    journal: Arc<QosChannel>,
    admission: Arc<SourceAdmission>,
    rows: QueryResults,
    progress: Arc<QuerySourceProgress>,
    seen: mpsc::Receiver<Value>,
    query_input: QueryInput,
    shared: Option<Arc<SharedStorageGroup>>,
}

enum QueryInput {
    Qos(Arc<QosChannel>, Option<Arc<SharedStorageGroup>>),
    Retained(Arc<IndexedEnvelopeStore>),
}

impl QueryInput {
    async fn processed(&self) -> Result<u64> {
        match self {
            Self::Qos(journal, _) => Ok(journal.progress().await?.processed["query"]),
            Self::Retained(store) => Ok(store.progress(store.generation()).await?),
        }
    }

    async fn accepted(&self) -> Result<u64> {
        match self {
            Self::Qos(journal, _) => Ok(journal.progress().await?.accepted),
            Self::Retained(store) => {
                let mut head = 0;
                while let Some(entry) = store.next(store.generation(), head).await? {
                    head = entry.position;
                }
                Ok(head)
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        match self {
            Self::Qos(journal, _) => journal.shutdown().await,
            Self::Retained(store) => store.shutdown().await,
        }
    }

    async fn audit_processor(&self, path: &Path, first: u64, last: u64) -> Result<()> {
        let mut pipe = match self {
            Self::Qos(journal, Some(group)) => {
                let journal_id = ResourceId::try_new("audit-journal")?;
                let storage_id = ResourceId::try_new("audit-storage")?;
                Some(
                    journal
                        .definition()
                        .pipe(journal_id.clone(), "audit")
                        .with_shared_storage(storage_id.clone())
                        .create_with_resources(&BTreeMap::from([
                            (journal_id, journal.resource()),
                            (storage_id, group.resource()),
                        ]))?,
                )
            }
            Self::Qos(_, None) => return Ok(()),
            Self::Retained(_) => None,
        };
        let mut receiver = pipe
            .as_mut()
            .map(|pipe| pipe.pipe.take_receiver())
            .transpose()?;
        let mut identity: Option<GraphProducerIdentity> = if first == 1 {
            None
        } else {
            Some(read_json(&path.join("processor-identity.json"))?)
        };
        for sequence in first..=last {
            let (envelope, acknowledgement) = match self {
                Self::Qos(_, _) => receiver
                    .as_mut()
                    .expect("shared audit receiver")
                    .receive()
                    .await?
                    .context("accepted shared input")?
                    .into_parts(),
                Self::Retained(store) => (
                    store
                        .next(store.generation(), sequence - 1)
                        .await?
                        .context("accepted retained input")?
                        .envelope,
                    None,
                ),
            };
            assert_eq!(
                GraphChangeCodec::decode_changes(&envelope)?,
                GraphChangeCodec::decode_changes(&event(sequence)?)?
            );
            let progress =
                GraphProducerProgress::from_envelope(&envelope)?.context("processor identity")?;
            assert_eq!(progress.sequence(), sequence);
            assert_eq!(progress.identity().component_id().as_str(), "processor");
            assert_eq!(progress.identity().stream().as_str(), "processor/out");
            assert!(progress.identity().persistent());
            if let Some(identity) = &identity {
                assert_eq!(
                    progress.identity(),
                    identity,
                    "processor identity changed across recovery"
                );
            } else {
                identity = Some(progress.identity().clone());
                write_json(&path.join("processor-identity.json"), &identity)?;
            }
            if let Some(acknowledgement) = acknowledgement {
                acknowledgement.complete(HandlingOutcome::Handled).await?;
            }
        }
        if let Some(pipe) = pipe {
            pipe.control.cancel();
        }
        Ok(())
    }
}

async fn fixture(
    path: &Path,
    query_fault: Arc<Fault>,
    input_fault: Arc<Fault>,
    mode: u8,
    profile: Profile,
    paged: bool,
) -> Result<Fixture> {
    let input_provider = FaultProvider {
        inner: provider(&path.join("input")),
        fault: input_fault,
    };
    let page_limits = paged.then_some(OutboxPageLimits {
        max_records: size(2),
        max_bytes: size(1 << 20),
    });
    let journal = QosChannel::persistent_with_options(
        QosChannelDefinition {
            stream: StreamId::try_new("input/out")?,
            capacity: size(TOTAL as usize + 4),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([
                (
                    (if profile == Profile::Admission {
                        "query"
                    } else {
                        "processor"
                    })
                    .into(),
                    SubscriptionStart::Earliest,
                ),
                ("audit".into(), SubscriptionStart::Earliest),
            ]),
        },
        input_provider
            .create_indexes(GRAPH, "input-journal")
            .await?,
        codec()?,
        "application",
        QosJournalOptions {
            recovery: Some(QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: GRAPH.into(),
                graph_id: GRAPH.into(),
                component_id: ComponentId::try_new("input")?,
                failure_scope: FailureMode::ProcessRestart,
                max_producers: size(2),
                receipts_per_producer: size(TOTAL as usize),
            })),
            page_limits,
            ..Default::default()
        },
    )
    .await?;
    let admission = SourceAdmission::new(journal.clone(), PortId::try_new("out")?).await?;
    let source = NativeApplicationSource::durable("input", admission.clone())?;
    let progress = Arc::new(QuerySourceProgress::new(
        GRAPH,
        ComponentId::try_new("query")?,
    )?);
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) RETURN n.id AS id, n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: size(TOTAL as usize + 4),
        },
        Arc::new(QueryProvider {
            inner: provider(&path.join("query")),
            fault: query_fault,
            mode,
        }),
    )
    .await?
    .with_recovery_scope(GRAPH)?
    .with_source_progress(progress.clone())?
    .with_delivery_tracking();
    let rows = query.results();
    let (sender, seen) = mpsc::channel(TOTAL as usize + 4);
    let journal_resource = ResourceId::try_new("journal")?;
    let mut builder = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: journal_resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "native-input".into(),
        })?
        .provide_resource(journal_resource.clone(), journal.resource())?
        .source(Box::new(source))
        .transformer(Box::new(query))
        .sink(Box::new(Capture {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("capture")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            seen: sender,
            file: OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.join("outputs.jsonl"))?,
        }))
        .bind_stream(endpoint("input", "out")?, StreamId::try_new("input/out")?)
        .bind_stream(endpoint("query", "out")?, StreamId::try_new("query/out")?)
        .connect(
            EdgeDefinition::new(endpoint("query", "out")?, endpoint("capture", "in")?),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    let (query_input, shared) = if profile == Profile::Admission {
        builder = builder.connect(
            EdgeDefinition::new(endpoint("input", "out")?, endpoint("query", "in")?),
            Box::new(journal.definition().pipe(journal_resource, "query")),
        );
        (QueryInput::Qos(journal.clone(), None), None)
    } else {
        let storage = ResourceId::try_new("processor-storage")?;
        let registry = ResourceId::try_new("middleware-registry")?;
        let definition = MiddlewareTransformerDefinition {
            id: ComponentId::try_new("processor")?,
            output_stream: StreamId::try_new("processor/out")?,
            middleware: vec![],
            pipeline: vec![],
        };
        let processor_provider = provider(&path.join("processor"));
        let group = if profile == Profile::Shared {
            Some(SharedStorageGroup::new(
                GRAPH,
                ComponentId::try_new("processor")?,
                processor_provider
                    .create_indexes(GRAPH, "processor")
                    .await?,
            )?)
        } else {
            None
        };
        let storage_resource = match &group {
            Some(group) => group.resource(),
            None => ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(processor_provider)),
            ),
        };
        builder = builder
            .declare_resource(ResourceSpecification {
                id: storage.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Graph,
                binding: "processor-storage".into(),
            })?
            .provide_resource(storage.clone(), storage_resource)?
            .declare_resource(ResourceSpecification {
                id: registry.clone(),
                role: ResourceRole::Middleware,
                ownership: ResourceOwnership::Borrowed,
                binding: "middleware-registry".into(),
            })?
            .provide_resource(
                registry.clone(),
                ResourceHandle::new(
                    ResourceRole::Middleware,
                    Arc::new(MiddlewareRegistryResource(Arc::new(
                        drasi_core::middleware::MiddlewareTypeRegistry::new(),
                    ))),
                ),
            )?
            .component(
                definition.durable_specification(
                    registry,
                    storage.clone(),
                    DurableMiddlewareOptions {
                        graph_id: GRAPH.into(),
                        outbox_capacity: size(TOTAL as usize + 4),
                    },
                )?,
                Arc::new(MiddlewareTransformerFactory::default()),
            )
            .bind_stream(
                endpoint("processor", "out")?,
                StreamId::try_new("processor/out")?,
            )
            .connect(
                EdgeDefinition::new(endpoint("input", "out")?, endpoint("processor", "in")?),
                Box::new(journal.definition().pipe(journal_resource, "processor")),
            );
        let input_resource = ResourceId::try_new("query-input")?;
        let (query_input, handle, pipe): (_, _, Box<dyn PipeProvider>) = if let Some(group) = &group
        {
            let channel_definition = QosChannelDefinition {
                stream: StreamId::try_new("processor/out")?,
                capacity: size(TOTAL as usize + 4),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([
                    ("query".into(), SubscriptionStart::Earliest),
                    ("audit".into(), SubscriptionStart::Earliest),
                ]),
            };
            let replay = ReplayOptions {
                failure_scope: FailureMode::ProcessRestart,
                receipt_capacity: size(TOTAL as usize + 4),
            };
            let channel = if let Some(limits) = page_limits {
                group
                    .channel_with_page_limits(
                        channel_definition,
                        "processor-output",
                        codec()?,
                        replay,
                        limits,
                    )
                    .await?
            } else {
                group
                    .channel(channel_definition, "processor-output", codec()?, replay)
                    .await?
            };
            let pipe = channel
                .definition()
                .pipe(input_resource.clone(), "query")
                .with_shared_storage(storage);
            (
                QueryInput::Qos(channel.clone(), Some(group.clone())),
                channel.resource(),
                Box::new(pipe),
            )
        } else {
            let store = Arc::new(IndexedEnvelopeStore::try_new_with_options(
                provider(&path.join("retained"))
                    .create_indexes(GRAPH, "retained-input")
                    .await?,
                Arc::new(codec()?),
                "processor-output",
                size(TOTAL as usize + 4),
                RetentionPolicy::Backpressure,
                IndexedJournalOptions {
                    page_limits,
                    ..Default::default()
                },
            )?);
            (
                QueryInput::Retained(store.clone()),
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(RetainedStoreResource(store)),
                ),
                Box::new(RetainedPipeConfig {
                    resource: input_resource.clone(),
                    capacity: size(TOTAL as usize + 4),
                    durable: true,
                    retention: RetentionPolicy::Backpressure,
                    gap_policy: ReplayGapPolicy::Strict,
                }),
            )
        };
        builder = builder
            .declare_resource(ResourceSpecification {
                id: input_resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "query-input".into(),
            })?
            .provide_resource(input_resource, handle)?
            .connect(
                EdgeDefinition::new(endpoint("processor", "out")?, endpoint("query", "in")?),
                pipe,
            );
        (query_input, group)
    };
    let graph = builder.build()?;
    Ok(Fixture {
        graph,
        journal,
        admission,
        rows,
        progress,
        seen,
        query_input,
        shared,
    })
}

async fn admit(
    admission: &SourceAdmission,
    session: &ProducerSession,
    sequence: u64,
) -> Result<AdmissionReceipt> {
    let input = event(sequence)?;
    loop {
        match admission.admit(session, sequence, &input).await {
            Err(PipeError::Backend(error))
                if error.downcast_ref::<AdmissionRejection>()
                    == Some(&AdmissionRejection::Busy) =>
            {
                tokio::task::yield_now().await;
            }
            result => return Ok(result?),
        }
    }
}

fn committed(progress: &QuerySourceProgress) -> u64 {
    let snapshot = progress.snapshot();
    assert!(snapshot.checkpoints.len() <= 1);
    snapshot
        .checkpoints
        .values()
        .next()
        .map_or(0, |checkpoint| checkpoint.sequence)
}

async fn wait_processed(
    input: &QueryInput,
    progress: &QuerySourceProgress,
    sequence: u64,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if input.processed().await? == sequence && committed(progress) == sequence {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

async fn receive(seen: &mut mpsc::Receiver<Value>, sequence: u64) -> Result<()> {
    let output = tokio::time::timeout(DEADLINE, seen.recv())
        .await?
        .context("capture closed")?;
    assert_eq!(output["sequence"], sequence);
    assert_eq!(
        output["operations"],
        json!([{
            "kind": "added",
            "row": {"id": format!("item-{sequence:02}"), "value": sequence * 17},
        }])
    );
    Ok(())
}

fn result_rows(rows: &QueryResults) -> Result<Vec<Value>> {
    let mut rows = rows
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by_key(|row| row["id"].as_str().expect("fixture row id").to_owned());
    Ok(rows)
}

async fn audit(journal: &Arc<QosChannel>, session: &ProducerSession) -> Result<()> {
    let resource = ResourceId::try_new("audit-journal")?;
    let mut pipe = journal
        .definition()
        .pipe(resource.clone(), "audit")
        .create_with_resources(&BTreeMap::from([(resource, journal.resource())]))?;
    let mut receiver = pipe.pipe.take_receiver()?;
    for sequence in 1..=TOTAL {
        let delivery = receiver
            .receive()
            .await?
            .context("retained accepted input")?;
        let envelope = delivery.envelope();
        assert_eq!(envelope.system().sequence(), sequence);
        assert_eq!(
            GraphChangeCodec::decode_changes(envelope)?,
            GraphChangeCodec::decode_changes(&event(sequence)?)?
        );
        let producer = GraphProducerProgress::from_envelope(envelope)?
            .context("admitted producer identity")?;
        assert_eq!(producer.sequence(), sequence);
        assert_eq!(producer.identity().incarnation(), session.incarnation);
        assert_eq!(producer.identity().component_id().as_str(), "input");
        assert_eq!(producer.identity().graph_id(), GRAPH);
        assert_eq!(producer.identity().construction_scope(), GRAPH);
        assert!(producer.identity().persistent());
        delivery
            .into_parts()
            .1
            .context("audit acknowledgement")?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    pipe.control.cancel();
    Ok(())
}

async fn worker(path: &Path, phase: &str, plan: Plan) -> Result<()> {
    let crash = phase == "crash";
    let restoring = phase == "restore";
    if restoring {
        verify_committed_crash_state(path, plan).await?;
    }
    let fault = Arc::new(Fault::default());
    let input_fault = Arc::new(Fault::default());
    let mode = if !crash {
        0
    } else if matches!(plan.boundary, Boundary::QueryAfterCommit) {
        3
    } else {
        4
    };
    let Fixture {
        mut graph,
        journal,
        admission,
        rows,
        progress,
        mut seen,
        query_input,
        shared,
    } = fixture(
        path,
        fault.clone(),
        input_fault.clone(),
        mode,
        plan.profile,
        plan.paged,
    )
    .await?;
    if restoring {
        let state = journal.progress().await?;
        assert_eq!(state.accepted, TOTAL);
        if plan.profile == Profile::Admission {
            assert_eq!(state.processed["query"], PREFIX);
        } else {
            assert_eq!(state.processed["processor"], TOTAL);
        }
        assert_eq!(state.processed["audit"], 0);
        if plan.profile != Profile::Retained {
            assert_eq!(query_input.processed().await?, PREFIX);
            assert_eq!(query_input.accepted().await?, TOTAL);
        }
    }
    let run = graph.start()?;
    let control = run.control();
    let work = async {
        let result = async {
            control.startup_report().await?;
            admission.wait_active().await?;
            let session = admission.register_producer(ComponentId::try_new("client")?).await?;
            if restoring {
                assert_eq!(session, read_json::<ProducerSession>(&path.join("session.json"))?);
                for sequence in 1..=TOTAL {
                    let receipt = admission.admission_receipt(&session, sequence).await?.context("persisted receipt")?;
                    assert_eq!(receipt, AdmissionReceipt { session: session.clone(), sequence, position: sequence });
                    assert_eq!(admit(&admission, &session, sequence).await?, receipt);
                }
                assert_eq!(journal.progress().await?.accepted, TOTAL, "retries must not append");
                audit(&journal, &session).await?;
                query_input.audit_processor(path, PREFIX + 1, TOTAL).await?;
                for sequence in PREFIX + 1..=TOTAL {
                    receive(&mut seen, sequence).await?;
                }
            } else {
                write_json(&path.join("session.json"), &session)?;
                for sequence in 1..=PREFIX {
                    let receipt = admit(&admission, &session, sequence).await?;
                    assert_eq!(receipt.position, sequence);
                    receive(&mut seen, sequence).await?;
                    wait_processed(&query_input, &progress, sequence).await?;
                }
                query_input.audit_processor(path, 1, PREFIX).await?;
                for sequence in PREFIX + 1..=TOTAL {
                    if crash && sequence == TOTAL && matches!(plan.boundary, Boundary::AdmissionLostResponse) {
                        fault.committed.notified().await;
                        assert_eq!(committed(&progress), PREFIX);
                        input_fault.mode.store(3, Ordering::SeqCst);
                        let request = admit(&admission, &session, sequence);
                        tokio::pin!(request);
                        tokio::select! {
                            result = &mut request => anyhow::bail!("lost-response request unexpectedly returned: {result:?}"),
                            _ = input_fault.committed.notified() => {}
                        }
                        write_json(&path.join("crash-evidence.json"), &json!({
                            "accepted": TOTAL, "receipts_returned": TOTAL - 1,
                            "query_published": committed(&progress), "query_committed": PREFIX,
                        }))?;
                        write_boundary(path, phase, GRAPH);
                        std::future::pending::<()>().await;
                    }
                    let receipt = admit(&admission, &session, sequence).await?;
                    assert_eq!(receipt.position, sequence);
                    if !crash {
                        receive(&mut seen, sequence).await?;
                        wait_processed(&query_input, &progress, sequence).await?;
                    }
                }
                if crash {
                    fault.committed.notified().await;
                    assert_eq!(committed(&progress), PREFIX);
                    assert_eq!(rows.snapshot()?.as_of_sequence, PREFIX);
                    let state = journal.progress().await?;
                    assert_eq!(state.accepted, TOTAL);
                    tokio::time::timeout(DEADLINE, async {
                        while query_input.accepted().await? != TOTAL
                            || (plan.profile != Profile::Admission
                                && journal.progress().await?.processed["processor"] != TOTAL)
                        {
                            tokio::task::yield_now().await;
                        }
                        Ok::<_, anyhow::Error>(())
                    }).await??;
                    assert_eq!(query_input.processed().await?, PREFIX);
                    write_json(&path.join("crash-evidence.json"), &json!({
                        "accepted": TOTAL, "receipts_returned": TOTAL,
                        "query_published": PREFIX,
                        "query_committed": PREFIX + u64::from(matches!(plan.boundary, Boundary::QueryAfterCommit)),
                    }))?;
                    write_boundary(path, phase, GRAPH);
                    std::future::pending::<()>().await;
                }
                audit(&journal, &session).await?;
                query_input.audit_processor(path, PREFIX + 1, TOTAL).await?;
            }
            wait_processed(&query_input, &progress, TOTAL).await?;
            if plan.profile != Profile::Admission {
                tokio::time::timeout(DEADLINE, async {
                    while journal.progress().await?.processed["processor"] != TOTAL {
                        tokio::task::yield_now().await;
                    }
                    Ok::<_, anyhow::Error>(())
                }).await??;
            }
            let snapshot = rows.snapshot()?;
            assert_eq!(snapshot.as_of_sequence, TOTAL);
            assert_eq!(snapshot.generation, 0);
            assert!(progress.snapshot().persistent);
            let expected = (1..=TOTAL).map(|sequence| json!({
                "id": format!("item-{sequence:02}"), "value": sequence * 17,
            })).collect::<Vec<_>>();
            assert_eq!(result_rows(&rows)?, expected);
            write_json(&path.join("result.json"), &json!({
                "pid": std::process::id(), "sequence": snapshot.as_of_sequence,
                "generation": snapshot.generation, "rows": result_rows(&rows)?,
                "query_committed": committed(&progress),
                "input_accepted": journal.progress().await?.accepted,
                "input_processed": journal.progress().await?.processed,
                "query_input_accepted": query_input.accepted().await?,
                "query_input_processed": query_input.processed().await?,
            }))?;
            Ok::<_, anyhow::Error>(())
        }.await;
        control.cancel();
        result
    };
    let (run, work) = tokio::time::timeout(DEADLINE, async { tokio::join!(run, work) }).await?;
    let cleanup = tokio::time::timeout(DEADLINE, graph.shutdown()).await;
    work?;
    assert!(matches!(run, Err(GraphError::Cancelled)), "{run:?}");
    cleanup??;
    drop(graph);
    tokio::time::timeout(DEADLINE, query_input.shutdown()).await??;
    tokio::time::timeout(DEADLINE, journal.shutdown()).await??;
    drop(query_input);
    if let Some(shared) = shared {
        tokio::time::timeout(DEADLINE, shared.shutdown()).await??;
    }
    drop(admission);
    drop(journal);
    // Successful awaited cleanup must release the actual provider lock, too.
    let mut stores = vec![("input", "input-journal"), ("query", "query")];
    if plan.profile != Profile::Admission {
        stores.push(("processor", "processor"));
    }
    if plan.profile == Profile::Retained {
        stores.push(("retained", "retained-input"));
    }
    for (directory, id) in stores {
        let indexes = provider(&path.join(directory))
            .create_indexes(GRAPH, id)
            .await?;
        indexes
            .cleanup()
            .context("reopened cleanup owner")?
            .shutdown()
            .await?;
    }
    Ok(())
}

async fn verify_committed_crash_state(path: &Path, plan: Plan) -> Result<()> {
    let expected = PREFIX + u64::from(matches!(plan.boundary, Boundary::QueryAfterCommit));
    let indexes = provider(&path.join("query"))
        .create_indexes(GRAPH, "query")
        .await?;
    let checkpoints = indexes
        .checkpoint_store()
        .context("persistent query checkpoints")?;
    assert_eq!(
        checkpoints.read_result_sequence("query").await?,
        Some(expected)
    );
    let input_checkpoints = checkpoints
        .read_all_checkpoints()
        .await?
        .into_iter()
        .filter(|(key, _)| key.starts_with("computation:input:"))
        .collect::<Vec<_>>();
    assert_eq!(input_checkpoints.len(), 1);
    assert_eq!(input_checkpoints[0].1.sequence, expected);
    let outbox = indexes
        .outbox_writer()
        .context("persistent query outbox")?
        .read_from("query", 0)
        .await?;
    assert_eq!(outbox.len(), expected as usize);
    let mut expected_live = Vec::new();
    for (index, (sequence, bytes)) in outbox.iter().enumerate() {
        assert_eq!(*sequence, index as u64 + 1);
        let output = codec()?.decode(bytes)?;
        let evidence = output_evidence(&output)?;
        assert_eq!(evidence["sequence"], *sequence);
        assert_eq!(
            evidence["operations"],
            json!([{
                "kind": "added",
                "row": {"id": format!("item-{sequence:02}"), "value": sequence * 17},
            }])
        );
        for operation in output.changes().operations() {
            let ChangeOperation::Added { after, .. } = operation else {
                anyhow::bail!("unexpected committed operation");
            };
            expected_live.push(after.payload().to_vec());
        }
    }
    let mut live = indexes
        .live_results_writer()
        .context("persistent projection")?
        .read_snapshot("query")
        .await?
        .into_iter()
        .map(|(_, bytes)| bytes)
        .collect::<Vec<_>>();
    live.sort();
    expected_live.sort();
    assert_eq!(live, expected_live, "projection and outbox commit together");
    indexes
        .cleanup()
        .context("query cleanup")?
        .shutdown()
        .await?;
    drop(indexes);
    if plan.profile == Profile::Retained {
        let indexes = provider(&path.join("retained"))
            .create_indexes(GRAPH, "retained-input")
            .await?;
        let checkpoints = indexes.checkpoint_store().context("retained checkpoints")?;
        assert_eq!(
            checkpoints.read_result_sequence("processor-output").await?,
            Some(TOTAL)
        );
        assert_eq!(
            checkpoints
                .read_checkpoint("computation-pipe-consumer:processor-output")
                .await?
                .context("retained consumer cursor")?
                .sequence,
            PREFIX
        );
        indexes
            .cleanup()
            .context("retained cleanup")?
            .shutdown()
            .await?;
    }
    write_json(
        &path.join("recovered-commit.json"),
        &json!({
            "pid": std::process::id(), "query_committed": expected,
        }),
    )?;
    Ok(())
}

#[test]
#[ignore = "executed explicitly in subprocesses by the native input crash tests"]
fn native_input_crash_worker() -> Result<()> {
    let path = PathBuf::from(std::env::var_os("DRASI_CHILD_STATE").context("worker state")?);
    let phase = std::env::var("DRASI_CHILD_PHASE")?;
    let plan: Plan = read_json(&path.join("plan.json"))?;
    let mut runtime = if plan.current_thread {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        runtime.worker_threads(2);
        runtime
    };
    runtime
        .enable_all()
        .build()?
        .block_on(worker(&path, &phase, plan))
}

fn outputs(path: &Path) -> Result<Vec<Value>> {
    std::fs::read_to_string(path.join("outputs.jsonl"))?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn qualify(profile: Profile) -> Result<()> {
    for (current_thread, paged) in [(true, false), (false, false), (true, true), (false, true)] {
        let root = tempfile::Builder::new()
            .prefix(".native-input-crash-")
            .tempdir_in(std::env::current_dir()?)?;
        let oracle_path = root.path().join("oracle");
        std::fs::create_dir(&oracle_path)?;
        write_json(
            &oracle_path.join("plan.json"),
            &Plan {
                current_thread,
                boundary: Boundary::QueryBeforeCommit,
                profile,
                paged,
            },
        )?;
        let mut oracle = OwnedChild::start(&oracle_path, WORKER, "oracle", GRAPH);
        oracle.wait(&oracle_path, "oracle", false);
        let expected = outputs(&oracle_path)?;
        assert_eq!(expected.len(), TOTAL as usize);
        let oracle_result: Value = read_json(&oracle_path.join("result.json"))?;
        for boundary in [
            Boundary::QueryBeforeCommit,
            Boundary::QueryAfterCommit,
            Boundary::AdmissionLostResponse,
        ] {
            if profile != Profile::Admission && matches!(boundary, Boundary::AdmissionLostResponse)
            {
                continue;
            }
            let path = root.path().join(format!("{boundary:?}"));
            std::fs::create_dir(&path)?;
            write_json(
                &path.join("plan.json"),
                &Plan {
                    current_thread,
                    boundary,
                    profile,
                    paged,
                },
            )?;
            let mut child = OwnedChild::start(&path, WORKER, "crash", GRAPH);
            child.wait(&path, "crash", true);
            let evidence: Value = read_json(&path.join("crash-evidence.json"))?;
            assert_eq!(evidence["accepted"], TOTAL);
            assert!(
                evidence["query_committed"]
                    .as_u64()
                    .expect("committed prefix")
                    < TOTAL
            );
            assert_eq!(outputs(&path)?, expected[..PREFIX as usize]);
            child.kill();
            let mut recovery = OwnedChild::start(&path, WORKER, "restore", GRAPH);
            recovery.wait(&path, "restore", false);
            assert_ne!(child.id(), recovery.id());
            let result: Value = read_json(&path.join("result.json"))?;
            assert_eq!(result["pid"], recovery.id());
            let recovered: Value = read_json(&path.join("recovered-commit.json"))?;
            assert_eq!(recovered["pid"], recovery.id());
            assert_eq!(recovered["query_committed"], evidence["query_committed"]);
            for key in [
                "sequence",
                "generation",
                "rows",
                "query_committed",
                "input_accepted",
                "input_processed",
                "query_input_accepted",
                "query_input_processed",
            ] {
                assert_eq!(result[key], oracle_result[key], "{boundary:?}: {key}");
            }
            assert_eq!(
                outputs(&path)?,
                expected,
                "exact stable identities and payloads"
            );
            eprintln!("native {profile:?} {boundary:?}, current_thread={current_thread}, paged={paged}: SIGKILL pid {}, recovered pid {}, accepted {TOTAL}, prefix {PREFIX}, exact {TOTAL} outputs", child.id(), recovery.id());
        }
    }
    Ok(())
}

#[test]
fn native_qos_accepted_input_survives_sigkill() -> Result<()> {
    qualify(Profile::Admission)
}

#[test]
fn native_retained_input_survives_sigkill() -> Result<()> {
    qualify(Profile::Retained)
}

#[test]
fn native_shared_journal_input_survives_sigkill() -> Result<()> {
    qualify(Profile::Shared)
}
