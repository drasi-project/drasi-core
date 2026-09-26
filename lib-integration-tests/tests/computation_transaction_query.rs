// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    evaluation::variable_value::VariableValue,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::*;
use drasi_state_store_redb::RedbStateStoreProvider;

fn provider(root: &Path) -> Arc<dyn ComputationIndexProvider> {
    LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(root, false, false)))
}

async fn query(root: &Path) -> Result<TransactionTransformer> {
    let mut query = TransactionTransformer::query(
        ContinuousQueryDefinition {
            graph_id: "transaction-query".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) RETURN count(n) AS total".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        provider(root),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await?;
    query.start().await?;
    Ok(query)
}

fn definition(key: &str, subscribers: &[&str]) -> QosChannelDefinition {
    QosChannelDefinition {
        stream: StreamId::try_new(format!("{key}/out")).unwrap(),
        capacity: NonZeroUsize::new(8).unwrap(),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: subscribers
            .iter()
            .map(|name| (name.to_string(), SubscriptionStart::Earliest))
            .collect(),
    }
}

async fn channel(root: &Path, key: &str, subscribers: &[&str]) -> Result<Arc<QosChannel>> {
    Ok(QosChannel::persistent(
        definition(key, subscribers),
        provider(root)
            .create_indexes("transaction-query", &format!("pipe-{key}"))
            .await?,
        FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(1024 * 1024).unwrap())?,
        key,
    )
    .await?)
}

fn endpoint(
    channel: &Arc<QosChannel>,
    definition: &QosChannelDefinition,
    subscriber: &str,
) -> Result<ProvidedPipe> {
    let id = ResourceId::try_new("journal")?;
    Ok(definition
        .pipe(id.clone(), subscriber)
        .create_with_resources(&BTreeMap::from([(id, channel.resource())]))?)
}

fn input(sequence: u64) -> Result<ChangeEnvelope> {
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::default(),
            },
        },
        StreamId::try_new("input/out")?,
        sequence,
        None,
    )?)
}

fn total(query: &TransactionTransformer) -> Result<i64> {
    let snapshot = query.query_results().context("query results")?.snapshot()?;
    anyhow::ensure!(snapshot.rows.len() == 1, "expected one aggregate");
    let row = QueryChangeCodec::decode_row(snapshot.rows.values().next().unwrap())?;
    match &row.values["total"] {
        VariableValue::Integer(value) => value.as_i64().context("aggregate integer"),
        value => anyhow::bail!("unexpected aggregate {value:?}"),
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    journal: PathBuf,
}
#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)?;
        writeln!(
            file,
            "{}",
            QueryChangeCodec::query_sequence(&input.envelope)?
        )?;
        file.sync_all()?;
        Ok(())
    }
}

async fn sink(
    root: &Path,
    name: &str,
    store: Arc<RedbStateStoreProvider>,
) -> Result<CheckpointedSink> {
    let sink = Sink {
        descriptor: ComponentDescriptor::try_new(
            ComponentId::try_new(name)?,
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?,
        journal: root.join(format!("{name}.effects")),
    };
    let mut sink = CheckpointedSink::new(
        Box::new(sink),
        Arc::new(StateStoreConsumerProgress::new(
            "transaction-query",
            name,
            store,
        )?),
    )?;
    sink.start().await?;
    Ok(sink)
}

async fn consume(receiver: &mut dyn EnvelopeReceiver, sink: &mut CheckpointedSink) -> Result<()> {
    let delivery = tokio::time::timeout(Duration::from_secs(3), receiver.receive())
        .await??
        .context("expected output delivery")?;
    let (envelope, acknowledgement) = delivery.into_parts();
    sink.handle(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope,
    })
    .await?;
    acknowledgement
        .context("QoS acknowledgement")?
        .complete(HandlingOutcome::Handled)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "real process-crash worker invoked by the parent test"]
async fn query_handoff_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var("DRASI_QUERY_HANDOFF_ROOT")?);
    let phase = std::env::var("DRASI_QUERY_HANDOFF_PHASE")?;
    let boundary = std::env::var("DRASI_QUERY_HANDOFF_BOUNDARY")?;
    let incoming = channel(&root, "input", &["query"]).await?;
    let outgoing = channel(&root, "query", &["fast", "slow"]).await?;
    let mut input_endpoint = endpoint(&incoming, &definition("input", &["query"]), "query")?;
    let mut input_receiver = input_endpoint.pipe.take_receiver()?;
    let mut query = query(&root).await?;
    if phase == "seed" {
        incoming.publish(&input(1)?).await?;
        let (envelope, _unacknowledged) = input_receiver.receive().await?.unwrap().into_parts();
        let output = query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope,
            })
            .await?;
        assert_eq!(total(&query)?, 1);
        assert_eq!(output.len(), 1);
        if boundary != "before-pipe" {
            outgoing.publish(&output[0].envelope).await?;
        }
        if boundary == "confirmed" {
            query.delivery_completed(&output).await?;
        }
        std::process::exit(86);
    }
    anyhow::ensure!(phase == "recover", "invalid worker phase");
    assert_eq!(
        total(&query)?,
        1,
        "the query state was committed before process death"
    );
    if boundary == "confirmed" {
        assert!(!query.has_pending_emissions());
    } else {
        assert!(query.has_pending_emissions());
        let replay = query.continue_transform().await?;
        assert_eq!(replay.len(), 1);
        assert_eq!(QueryChangeCodec::query_sequence(&replay[0].envelope)?, 1);
        assert!(
            replay[0].envelope.system().sequence() > 1,
            "replay has a new delivery number"
        );
        outgoing.publish(&replay[0].envelope).await?;
        query.delivery_completed(&replay).await?;
    }
    // The input acknowledgement was never committed, so the pipe redelivers.
    // Its query checkpoint must suppress reevaluation of the already committed input.
    let (envelope, acknowledgement) = input_receiver.receive().await?.unwrap().into_parts();
    assert!(query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope
        })
        .await?
        .is_empty());
    acknowledgement
        .unwrap()
        .complete(HandlingOutcome::Handled)
        .await?;
    assert_eq!(total(&query)?, 1);
    let store = Arc::new(RedbStateStoreProvider::new(root.join("consumers.redb"))?);
    for name in ["fast", "slow"] {
        let mut output_endpoint =
            endpoint(&outgoing, &definition("query", &["fast", "slow"]), name)?;
        let mut receiver = output_endpoint.pipe.take_receiver()?;
        let mut sink = sink(&root, name, store.clone()).await?;
        let count = outgoing.progress().await?.accepted;
        for _ in 0..count {
            consume(receiver.as_mut(), &mut sink).await?;
        }
        assert_eq!(
            std::fs::read_to_string(root.join(format!("{name}.effects")))?,
            "1\n"
        );
        sink.stop().await?;
    }
    assert_eq!(incoming.progress().await?.processed["query"], 1);
    query.stop().await?;
    incoming.shutdown().await?;
    outgoing.shutdown().await?;
    Ok(())
}

#[test]
fn process_crashes_preserve_atomic_query_state_and_every_durable_handoff() -> Result<()> {
    for boundary in ["before-pipe", "accepted", "confirmed"] {
        let directory = tempfile::tempdir()?;
        for phase in ["seed", "recover"] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "query_handoff_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("DRASI_QUERY_HANDOFF_ROOT", directory.path())
                .env("DRASI_QUERY_HANDOFF_PHASE", phase)
                .env("DRASI_QUERY_HANDOFF_BOUNDARY", boundary)
                .output()?;
            assert_eq!(
                output.status.code(),
                Some(if phase == "seed" { 86 } else { 0 }),
                "{boundary}/{phase}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "scheduled query process worker invoked by the parent test"]
async fn scheduled_query_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var("DRASI_QUERY_HANDOFF_ROOT")?);
    let phase = std::env::var("DRASI_QUERY_HANDOFF_PHASE")?;
    let boundary = std::env::var("DRASI_QUERY_HANDOFF_BOUNDARY")?;
    let scheduling = Arc::new(QuerySchedulingResource::default());
    let mut query = TransactionTransformer::query(
        ContinuousQueryDefinition {
            graph_id: "scheduled-query".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) WHERE drasi.trueLater(true, 2000) RETURN n".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        provider(&root),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await?
    .with_scheduling(scheduling.clone())?;
    let mut source = QueryScheduledSource::new(
        ComponentId::try_new("clock")?,
        StreamId::try_new("clock/out")?,
        scheduling,
    )?;
    query.start().await?;
    source.start().await?;
    if phase == "seed" {
        assert!(query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: input(1)?
            })
            .await?
            .is_empty());
        if boundary == "after-due-commit" {
            let notification = tokio::time::timeout(Duration::from_secs(3), source.next())
                .await??
                .unwrap();
            assert_eq!(
                query
                    .transform(InputEnvelope {
                        port: PortId::try_new("in")?,
                        envelope: notification.envelope
                    })
                    .await?
                    .len(),
                1
            );
        }
        std::process::exit(87);
    }
    let output = if boundary == "after-due-commit" {
        assert!(
            query.has_pending_emissions(),
            "committed timer output awaits durable delivery"
        );
        query.continue_transform().await?
    } else {
        assert!(!query.has_pending_emissions());
        let notification = tokio::time::timeout(Duration::from_secs(3), source.next())
            .await??
            .unwrap();
        query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: notification.envelope,
            })
            .await?
    };
    assert_eq!(output.len(), 1);
    assert_eq!(QueryChangeCodec::query_sequence(&output[0].envelope)?, 1);
    query.delivery_completed(&output).await?;
    let repeated_hint = GraphChangeCodec::encode_futures_due(
        &ComponentId::try_new("clock")?,
        StreamId::try_new("clock/out")?,
        99,
        chrono::DateTime::from_timestamp_millis(2000).unwrap(),
    )?;
    assert!(query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: repeated_hint
        })
        .await?
        .is_empty());
    assert_eq!(query.query_results().unwrap().snapshot()?.rows.len(), 1);
    assert_eq!(query.query_results().unwrap().snapshot()?.as_of_sequence, 1);
    source.stop().await?;
    query.stop().await?;
    Ok(())
}

#[test]
fn scheduled_work_and_its_committed_output_survive_process_crashes() -> Result<()> {
    for boundary in ["before-due", "after-due-commit"] {
        let directory = tempfile::tempdir()?;
        for phase in ["seed", "recover"] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "scheduled_query_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("DRASI_QUERY_HANDOFF_ROOT", directory.path())
                .env("DRASI_QUERY_HANDOFF_PHASE", phase)
                .env("DRASI_QUERY_HANDOFF_BOUNDARY", boundary)
                .output()?;
            assert_eq!(
                output.status.code(),
                Some(if phase == "seed" { 87 } else { 0 }),
                "{boundary}/{phase}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}
