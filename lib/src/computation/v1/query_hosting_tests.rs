// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{LegacyIndexProviderAdapter, ResourceCleanup};
use drasi_core::{
    interface::{CreatedIndexes, IndexBackendPlugin},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use std::path::Path;

const TEXT: &str = "MATCH (n:Person) RETURN n.name AS name";
const OLD_GRAPH: &str = "lib-query/7175657279";
const MAIN_GRAPH: &str = "__drasi_lib_runtime__";

#[derive(Clone, Copy, Debug)]
enum MissingResource {
    Checkpoint,
    Outbox,
    LiveResults,
    PersistentCheckpoint,
}

struct IncompleteBackend {
    inner: RocksDbIndexProvider,
    missing: MissingResource,
}

impl IncompleteBackend {
    fn omit(&self, mut indexes: CreatedIndexes) -> CreatedIndexes {
        match self.missing {
            MissingResource::Checkpoint => indexes.checkpoint_store = None,
            MissingResource::Outbox => indexes.outbox_writer = None,
            MissingResource::LiveResults => indexes.live_results_writer = None,
            MissingResource::PersistentCheckpoint => {
                indexes.checkpoint_store = Some(Arc::new(
                    drasi_core::in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore::new(),
                ));
            }
        }
        indexes
    }
}

#[async_trait]
impl IndexBackendPlugin for IncompleteBackend {
    async fn create_indexes(&self, query: &str) -> Result<CreatedIndexes, IndexError> {
        Ok(self.omit(self.inner.create_indexes(query).await?))
    }

    async fn create_scoped_indexes(
        &self,
        storage_scope: &str,
        query: &str,
    ) -> Result<CreatedIndexes, IndexError> {
        Ok(self.omit(
            self.inner
                .create_scoped_indexes(storage_scope, query)
                .await?,
        ))
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

async fn open(
    path: &Path,
    graph: &str,
    deferred_build: bool,
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
        deferred_build,
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

async fn current_query_reconstruction(delivered: bool) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut before, provider) = open(directory.path(), MAIN_GRAPH, true).await?;
    let output = before.transform(input(1, false, "before")?).await?;
    assert_eq!(output.len(), 1);
    if delivered {
        before.delivery_completed(&output).await?;
    }
    let saved = before.results().recovery_view(Some(0))?;
    assert_eq!(saved.identity.graph_id(), MAIN_GRAPH);
    assert_eq!(saved.identity.construction_scope(), "instance");
    assert_eq!(saved.identity.incarnation(), None);
    assert_eq!(saved.snapshot.as_of_sequence, 1);
    close(before, provider).await?;

    let (mut after, provider) = open(directory.path(), MAIN_GRAPH, false).await?;
    let recovered = after.results().recovery_view(Some(0))?;
    assert_eq!(
        recovered.snapshot.as_of_sequence, saved.snapshot.as_of_sequence,
        "both construction paths must reopen the same current query identity"
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
async fn query_state_and_producer_identity_survive_current_reconstruction() -> anyhow::Result<()> {
    current_query_reconstruction(true).await
}

#[tokio::test]
async fn query_replays_unconfirmed_output_after_current_reconstruction() -> anyhow::Result<()> {
    current_query_reconstruction(false).await
}

#[tokio::test]
async fn retired_query_namespace_is_not_adopted_or_deleted() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut old, provider) = open(directory.path(), OLD_GRAPH, true).await?;
    let output = old.transform(input(1, false, "retained")?).await?;
    old.delivery_completed(&output).await?;
    let saved = old.results().recovery_view(None)?;
    close(old, provider).await?;
    let (current, provider) = open(directory.path(), MAIN_GRAPH, true).await?;
    let fresh = current.results().recovery_view(None)?;
    assert_eq!(fresh.identity.graph_id(), MAIN_GRAPH);
    assert_ne!(fresh.identity, saved.identity);
    assert!(fresh.snapshot.rows.is_empty());
    assert_eq!(fresh.snapshot.as_of_sequence, 0);
    close(current, provider).await?;
    let (old, provider) = open(directory.path(), OLD_GRAPH, true).await?;
    let retained = old.results().recovery_view(None)?;
    assert_eq!(retained.identity, saved.identity);
    assert_eq!(retained.snapshot.rows, saved.snapshot.rows);
    assert_eq!(
        retained.snapshot.as_of_sequence,
        saved.snapshot.as_of_sequence
    );
    close(old, provider).await
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

async fn incomplete_storage_preserves_current_state() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut before, provider) = open(directory.path(), MAIN_GRAPH, true).await?;
    let output = before.transform(input(1, false, "retained")?).await?;
    before.delivery_completed(&output).await?;
    let saved = before.results().recovery_view(None)?;
    close(before, provider).await?;

    for deferred_build in [false, true] {
        for missing in [
            MissingResource::Checkpoint,
            MissingResource::Outbox,
            MissingResource::LiveResults,
            MissingResource::PersistentCheckpoint,
        ] {
            let provider = LegacyIndexProviderAdapter::scoped(
                Arc::new(IncompleteBackend {
                    inner: RocksDbIndexProvider::new(directory.path(), false, false),
                    missing,
                }),
                "instance",
            )?;
            let result = ContinuousQueryTransformer::construct(
                ContinuousQueryDefinition {
                    graph_id: MAIN_GRAPH.into(),
                    id: ComponentId::try_new("query")?,
                    query: TEXT.into(),
                    language: ComputationQueryLanguage::Cypher,
                    output_stream: StreamId::try_new("query/out")?,
                    outbox_capacity: NonZeroUsize::new(8).unwrap(),
                },
                provider.clone(),
                QueryOptions {
                    recovery: QueryRecoveryPolicy::AutoReset,
                    publication: QueryPublicationMode::NonAtomic,
                },
                QueryExecutionSettings::default(),
                None,
                deferred_build,
            )
            .await;
            let error = match result {
                Ok(mut query) => query
                    .start()
                    .await
                    .expect_err("incomplete storage must fail startup"),
                Err(error) => error,
            };
            assert!(
                format!("{error:#}").contains(
                    "persistent query recovery requires persistent checkpoint, outbox and live-result resources"
                ),
                "{missing:?}: {error:#}"
            );
            provider.shutdown().await?;
        }
    }
    let (after, provider) = open(directory.path(), MAIN_GRAPH, false).await?;
    let retained = after.results().recovery_view(None)?;
    assert_eq!(retained.identity, saved.identity);
    assert_eq!(retained.snapshot.rows, saved.snapshot.rows);
    assert_eq!(
        retained.snapshot.as_of_sequence,
        saved.snapshot.as_of_sequence
    );
    close(after, provider).await
}

#[tokio::test]
async fn incomplete_persistent_storage_is_rejected_without_resetting_state() -> anyhow::Result<()> {
    incomplete_storage_preserves_current_state().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_persistent_storage_is_rejected_on_multi_thread_runtime() -> anyhow::Result<()> {
    incomplete_storage_preserves_current_state().await
}

#[test]
fn retired_query_execution_mode_is_rejected() -> anyhow::Result<()> {
    let factory = Arc::new(ContinuousQueryFactory::default());
    let mut specification = factory.specification_from_config(
        ComponentId::try_new("query")?,
        serde_json::json!({
            "query": TEXT,
            "stream": "query/out",
            "runtime_compatibility": true,
        }),
    )?;
    let resource = super::super::ResourceId::try_new("indexes")?;
    specification
        .dependencies
        .insert("indexes".into(), vec![resource.clone()]);
    let graph = super::super::ComputationGraph::builder("graph")
        .declare_resource(super::super::ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::IndexBackend,
            ownership: super::super::ResourceOwnership::Borrowed,
            binding: "memory".into(),
        })?
        .provide_resource(
            resource,
            super::super::ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(Arc::new(
                    drasi_core::computation::InMemoryComputationProvider,
                ))),
            ),
        )?
        .component(specification, factory)
        .build();
    let error = match graph {
        Ok(_) => anyhow::bail!("removed execution mode must not be accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("runtime_compatibility"),
        "{error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn reset_marker_requires_current_generation_without_rewriting_state() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut query, provider) = open(directory.path(), MAIN_GRAPH, true).await?;
    let output = query.transform(input(1, false, "retained")?).await?;
    query.delivery_completed(&output).await?;
    query.options.recovery = QueryRecoveryPolicy::AutoReset;
    let resources = query.query()?.resources();
    let checkpoint = resources.checkpoint_store().unwrap().clone();
    let outbox = resources.outbox_writer().unwrap().clone();
    let bytes = serde_json::to_vec(&serde_json::json!({"sequence": 1, "in_progress": false}))?;
    query
        .query()?
        .resource_transaction(|| async {
            outbox.append(&query_reset_key("query"), 1, &bytes).await
        })
        .await?;
    let saved = checkpoint.read_all_checkpoints().await?;
    let retained = outbox.read_from("query", 0).await?;
    for _ in 0..2 {
        let error = query
            .initialize_recovery()
            .await
            .expect_err("generation is required");
        assert!(error.to_string().contains("generation"), "{error:#}");
    }
    let current = checkpoint.read_all_checkpoints().await?;
    assert_eq!(current.len(), saved.len());
    for (key, expected) in saved {
        let actual = current.get(&key).expect("checkpoint must not be removed");
        assert_eq!(actual.sequence, expected.sequence);
        assert_eq!(actual.source_position, expected.source_position);
    }
    assert_eq!(outbox.read_from("query", 0).await?, retained);
    assert_eq!(
        outbox.read_from(&query_reset_key("query"), 0).await?,
        vec![(1, bytes)]
    );
    close(query, provider).await
}
