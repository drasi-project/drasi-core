// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Database-free tests. Live SQL Server qualification is deferred (CW-78);
//! the shared cursor protocol runs against an in-memory session in
//! `drasi_computation_storedproc_common::conformance`.

mod config;
mod delivery;
mod injection;
mod management;
mod sink;

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentId, DeliveryBatch, DeliveryScope, EnvelopeCodec, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, InputBinding, InputEnvelope, OutputEndpoint,
    PortId, StreamId,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::{json, Value};
use std::{num::NonZeroUsize, sync::Arc};

pub(crate) const SINK_ID: &str = "df1e84de-e73a-5f3c-a426-cfc15b6d0a93";
pub(crate) const OUTPUT_ID: &str = "10739661-2175-5f1b-ba4c-65f05af8348c";
pub(crate) const GRAPH: &str = "graph-a";
pub(crate) const STREAM: &str = "q/out";

pub(crate) fn inputs() -> Vec<InputBinding> {
    vec![InputBinding {
        input_port: PortId::try_new("in").expect("port"),
        output: OutputEndpoint::new(
            ComponentId::try_new(OUTPUT_ID).expect("component"),
            PortId::try_new("out").expect("port"),
        ),
        stream: StreamId::try_new(STREAM).expect("stream"),
    }]
}

/// A minimal valid flat configuration. No I/O happens at construction.
pub(crate) fn config() -> Value {
    json!({
        "host": "no-network-at-construction",
        "database": "d",
        "user": "u",
        "password": "p",
        "outputs": {"q": {"component": OUTPUT_ID, "port": "out"}},
        "defaultTemplate": {"added": {"template": "EXEC dbo.p {{param after.id}}"}}
    })
}

pub(crate) fn sink(configuration: Value) -> Result<crate::MsSqlStoredProcSink> {
    crate::MsSqlStoredProcSink::new(
        ComponentId::try_new(SINK_ID)?,
        GRAPH,
        crate::MsSqlStoredProcConfig::parse(configuration)?,
        &inputs(),
        false,
    )
}

fn metadata(id: &str) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(OUTPUT_ID, id),
        labels: Arc::from([Arc::<str>::from("Room")]),
        effective_from: 1_000,
    }
}

pub(crate) fn insert(id: &str, properties: &[(&str, ElementValue)]) -> SourceChange {
    let mut map = ElementPropertyMap::new();
    for (name, value) in properties {
        map.insert(name, value.clone());
    }
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id),
            properties: map,
        },
    }
}

pub(crate) fn delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: metadata(id),
    }
}

/// A delivery batch exactly as the host runner builds it.
pub(crate) fn batch(sequence: u64, changes: &[SourceChange]) -> Result<DeliveryBatch> {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 << 20).expect("bound"));
    codec.register_schema(GraphChangeCodec::schema())?;
    let producer = GraphProducerIdentity::new(
        "instance".into(),
        GRAPH.into(),
        ComponentId::try_new(OUTPUT_ID)?,
        StreamId::try_new(STREAM)?,
        true,
    )?;
    let mut input = InputEnvelope {
        execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new(STREAM)?,
            sequence,
            None,
        )?,
    };
    GraphProducerProgress::annotate(&mut input.envelope, &producer, sequence)?;
    DeliveryBatch::new(
        DeliveryScope::new(
            "instance".into(),
            GRAPH.into(),
            ComponentId::try_new(SINK_ID)?,
        )?,
        input,
        &codec,
    )
}

/// Every async test runs under this bound, so a hang fails fast instead of
/// stalling the suite.
pub(crate) async fn within(test: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), test)
        .await
        .map_err(|_| anyhow::anyhow!("test exceeded its 30 s bound"))?
}
