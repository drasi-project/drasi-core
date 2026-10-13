// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod codec;
#[cfg(unix)]
mod integration;
#[cfg(unix)]
mod tls;

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use bytes::Bytes;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes},
    interface::FailureMode,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;

use crate::{BridgeConfig, Limits, Namespace, PortableSchema, Profile, Transport};

const GRAPH: &str = "__drasi_lib_runtime__";
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Result<Self> {
        let path =
            PathBuf::from(".bridge-test-data").join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn namespace() -> Namespace {
    Namespace {
        version: 1,
        source_prefixes: vec!["plant/".into()],
        query_prefixes: vec!["plant/".into()],
    }
}

fn configuration(
    directory: &Directory,
    source: bool,
    profile: Profile,
    schema: PortableSchema,
) -> BridgeConfig {
    BridgeConfig {
        link_id: "link".into(),
        sender_instance: "left".into(),
        receiver_instance: "right".into(),
        sender_endpoint: "bridge-out".into(),
        receiver_endpoint: "bridge-in".into(),
        stream: if source { "receiver/in" } else { "sender/out" }.into(),
        peer_stream: if source { "sender/out" } else { "receiver/in" }.into(),
        schema,
        profile,
        failure_scope: FailureMode::ProcessRestart,
        namespace: namespace(),
        transport: Transport::Unix {
            data_path: directory.path().join("data.sock").to_string_lossy().into(),
            control_path: directory
                .path()
                .join("control.sock")
                .to_string_lossy()
                .into(),
        },
        limits: Limits {
            retry_attempts: 1,
            queue_capacity: 1,
            timeout_ms: 2_000,
            ..Limits::default()
        },
    }
}

fn graph_input(sequence: u64, value: i64) -> Result<ChangeEnvelope> {
    let mut properties = ElementPropertyMap::new();
    properties.insert("number", ElementValue::Integer(value));
    properties.insert(
        "nested",
        ElementValue::List(vec![
            ElementValue::String(Arc::from("λ")),
            ElementValue::Object(ElementPropertyMap::from(serde_json::json!({"flag":true}))),
        ]),
    );
    let event = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("plant/db", "item"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1_700_000_000_000,
                },
                properties,
            },
        },
        StreamId::try_new("sender/out")?,
        sequence,
        Some(chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("timestamp")),
    )?;
    let mut envelope = ChangeEnvelope::new(
        event.id().clone(),
        event.changes().clone(),
        event
            .system()
            .as_ref()
            .clone()
            .with_source_position(Bytes::from_static(b"commit/42")),
    );
    for value in [3, 8] {
        envelope.append_annotation(ContextEntry::try_new(
            ComponentId::try_new("plant/source")?,
            "application/duplicate",
            ContextValue::Unsigned(value),
        )?)?;
    }
    Ok(envelope)
}

fn query_input(sequence: u64) -> Result<ChangeEnvelope> {
    use drasi_core::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };
    let values = BTreeMap::from([
        ("number".into(), VariableValue::Integer(42.into())),
        (
            "nested".into(),
            VariableValue::List(vec![
                VariableValue::String("query value".into()),
                VariableValue::Null,
            ]),
        ),
        (
            "when".into(),
            VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2026, 10, 12).expect("date")),
        ),
    ]);
    Ok(QueryChangeCodec::encode_evaluation(
        None,
        &ComponentId::try_new("plant/query")?,
        SystemMetadata::new(StreamId::try_new("sender/out")?, sequence),
        &[QueryPartEvaluationContext::Adding {
            after: values,
            row_signature: 42,
        }],
        QueryOutputMetadata {
            query_id: "plant/query".into(),
            source_id: Some("plant/db".into()),
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789)
                .expect("timestamp"),
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .expect("query changes"))
}

fn transaction_input(sequence: u64) -> Result<ChangeEnvelope> {
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(10).unwrap(),
        max_bytes: NonZeroUsize::new(65_536).unwrap(),
        max_duration_ms: std::num::NonZeroU64::new(10_000).unwrap(),
    };
    let mut transaction = SourceTransactionBuilder::new("plant/db", limits)?;
    for value in [1, 2] {
        for change in GraphChangeCodec::decode_changes(&graph_input(sequence, value)?)? {
            transaction.push(change)?;
        }
    }
    Ok(transaction
        .commit(
            Bytes::from_static(b"transaction-1"),
            Bytes::from_static(b"commit/42"),
        )?
        .into_envelope(
            StreamId::try_new("sender/out")?,
            sequence,
            chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap(),
        )?)
}

fn provider(path: &Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
        ),
    ))
}

async fn indexes(path: &Path) -> Result<ComputationIndexes> {
    Ok(provider(path).create_indexes("mapping", "sender").await?)
}

fn envelope_codec() -> Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(8 << 20).unwrap());
    for schema in [
        GraphChangeCodec::schema(),
        QueryChangeCodec::schema(),
        SourceTransactionCodec::schema(),
    ] {
        codec.register_schema(schema)?;
    }
    Ok(codec)
}

fn definition(stream: &str, subscriber: &str, capacity: usize) -> Result<QosChannelDefinition> {
    Ok(QosChannelDefinition {
        stream: StreamId::try_new(stream)?,
        capacity: NonZeroUsize::new(capacity).unwrap(),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([(subscriber.into(), SubscriptionStart::Earliest)]),
    })
}

async fn channel(
    path: &Path,
    stream: &str,
    instance: &str,
    component: &str,
    subscriber: &str,
    capacity: usize,
) -> Result<Arc<QosChannel>> {
    let indexes = provider(path)
        .create_indexes("bridge-test-ingress", component)
        .await?;
    Ok(QosChannel::persistent_with_recovery(
        definition(stream, subscriber, capacity)?,
        indexes,
        envelope_codec()?,
        "bridge-test-ingress",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: instance.into(),
            graph_id: GRAPH.into(),
            component_id: ComponentId::try_new(component)?,
            failure_scope: FailureMode::ProcessRestart,
            max_producers: NonZeroUsize::MIN,
            receipts_per_producer: NonZeroUsize::MIN,
        }),
    )
    .await?)
}

fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(
        ComponentId::try_new(component).unwrap(),
        PortId::try_new(port).unwrap(),
    )
}

fn declaration(id: &str, role: ResourceRole) -> ResourceSpecification {
    ResourceSpecification {
        id: ResourceId::try_new(id).unwrap(),
        role,
        ownership: ResourceOwnership::Borrowed,
        binding: id.into(),
    }
}

fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").unwrap(),
        envelope,
    }
}
