// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{LegacyIndexProviderAdapter, ResourceCleanup};
use drasi_core::{
    interface::IndexBackendPlugin,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use std::path::Path;

const TEXT: &str = "MATCH (n:Person) RETURN n.name AS name";
const OLD_GRAPH: &str = "lib-query/7175657279";
const MAIN_GRAPH: &str = "__drasi_lib_runtime__";

async fn open(
    path: &Path,
    graph: &str,
    ordinary: bool,
) -> anyhow::Result<(ContinuousQueryTransformer, Arc<LegacyIndexProviderAdapter>)> {
    let backend = Arc::new(RocksDbIndexProvider::new(path, false, false));
    let publication = if backend.supports_atomic_query_output() {
        QueryPublicationMode::Atomic
    } else {
        QueryPublicationMode::NonAtomic
    };
    let provider = LegacyIndexProviderAdapter::scoped(backend, "instance")?;
    let mut query = ContinuousQueryTransformer::construct(
        ContinuousQueryDefinition {
            graph_id: graph.into(),
            id: ComponentId::try_new("query")?,
            query: TEXT.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        provider.clone(),
        QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            publication,
        },
        QueryExecutionSettings::default(),
        None,
        ordinary,
        true,
    )
    .await?;
    query = query
        .with_recovery_scope("instance")?
        .with_delivery_tracking();
    query.start().await?;
    Ok((query, provider))
}

fn input(sequence: u64, update: bool, name: &str) -> anyhow::Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "person"),
            labels: vec!["Person".into()].into(),
            effective_from: sequence,
        },
        properties: ElementPropertyMap::from(serde_json::json!({ "name": name })),
    };
    let change = if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    };
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            change,
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

async fn close(
    mut query: ContinuousQueryTransformer,
    provider: Arc<LegacyIndexProviderAdapter>,
) -> anyhow::Result<()> {
    query.stop().await?;
    drop(query);
    provider.shutdown().await
}

async fn ordinary_query_relocation(delivered: bool) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut before, provider) = open(directory.path(), OLD_GRAPH, true).await?;
    let output = before.transform(input(1, false, "before")?).await?;
    assert_eq!(output.len(), 1);
    if delivered {
        before.delivery_completed(&output).await?;
    }
    let saved = before.results().recovery_view(Some(0))?;
    assert_eq!(saved.identity.graph_id(), OLD_GRAPH);
    assert_eq!(saved.identity.construction_scope(), "instance");
    assert_eq!(saved.identity.incarnation(), None);
    assert_eq!(saved.snapshot.as_of_sequence, 1);
    close(before, provider).await?;

    let (mut after, provider) = open(directory.path(), MAIN_GRAPH, true).await?;
    let recovered = after.results().recovery_view(Some(0))?;
    assert_eq!(
        recovered.snapshot.as_of_sequence, saved.snapshot.as_of_sequence,
        "changing graph placement must not open an empty query state partition"
    );
    assert_eq!(recovered.identity, saved.identity);
    assert_eq!(recovered.snapshot.generation, saved.snapshot.generation);
    assert_eq!(recovered.snapshot.rows, saved.snapshot.rows);
    assert_eq!(
        recovered.retained.as_ref().map(Vec::len),
        saved.retained.as_ref().map(Vec::len)
    );
    if !delivered {
        let replay = after.on_wakeup().await?;
        assert_eq!(replay.len(), 1);
        assert_eq!(
            crate::computation::v1::QueryRecoveryIdentity::from_query_output(&replay[0].envelope)?,
            saved.identity
        );
        assert_eq!(
            QueryChangeCodec::to_legacy_result(&replay[0].envelope)?.results,
            QueryChangeCodec::to_legacy_result(&output[0].envelope)?.results
        );
        after.delivery_completed(&replay).await?;
    }
    assert!(after
        .transform(input(1, false, "before")?)
        .await?
        .is_empty());
    let output = after.transform(input(2, true, "after")?).await?;
    assert_eq!(output.len(), 1);
    after.delivery_completed(&output).await?;
    let updated = after.results().snapshot()?;
    assert_eq!(updated.as_of_sequence, 2);
    assert_eq!(updated.rows.len(), 1);
    let row = QueryChangeCodec::decode_row(updated.rows.values().next().unwrap())?;
    assert_eq!(serde_json::to_value(row.values)?["name"], "after");
    close(after, provider).await
}

#[tokio::test]
async fn ordinary_query_state_and_producer_identity_survive_graph_relocation() -> anyhow::Result<()>
{
    ordinary_query_relocation(true).await
}

#[tokio::test]
async fn ordinary_query_replays_unconfirmed_output_after_graph_relocation() -> anyhow::Result<()> {
    ordinary_query_relocation(false).await
}

#[tokio::test]
async fn explicit_native_queries_keep_distinct_graph_identities() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut before, provider) = open(directory.path(), "native-before", false).await?;
    let output = before.transform(input(1, false, "before")?).await?;
    before.delivery_completed(&output).await?;
    let saved = before.results().recovery_view(None)?;
    assert_eq!(saved.identity.graph_id(), "native-before");
    close(before, provider).await?;

    let (after, provider) = open(directory.path(), "native-after", false).await?;
    let fresh = after.results().recovery_view(None)?;
    assert_eq!(fresh.identity.graph_id(), "native-after");
    assert_ne!(fresh.identity, saved.identity);
    assert_eq!(fresh.snapshot.as_of_sequence, 0);
    assert!(fresh.snapshot.rows.is_empty());
    close(after, provider).await
}
