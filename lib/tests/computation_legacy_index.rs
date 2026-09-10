// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation")]
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::IndexBackendPlugin,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use std::{num::NonZeroUsize, sync::Arc};

fn definition(query: &str) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "legacy-index".into(),
        id: ComponentId::try_new("query").expect("id"),
        query: query.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").expect("stream"),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    }
}
fn input() -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", "one"),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 1000,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({"name":"one"})),
                },
            },
            StreamId::try_new("source/out").expect("stream"),
            1,
            None,
        )
        .expect("input"),
    }
}
fn options() -> QueryOptions {
    QueryOptions {
        recovery: QueryRecoveryPolicy::Strict,
        publication: QueryPublicationMode::NonAtomic,
    }
}

#[tokio::test]
async fn ordinary_rocks_plugin_runs_in_an_isolated_non_atomic_computation_scope_and_recovers() {
    let temp = tempfile::tempdir().expect("temp");
    let plugin = Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
        temp.path(),
        false,
        false,
    ));
    let adapter = LegacyIndexProviderAdapter::new(plugin.clone());
    {
        let resources = adapter
            .create_indexes("capability", "query")
            .await
            .expect("adapted indexes");
        assert!(
            resources.atomic_result_transaction().is_err(),
            "legacy writers do not prove a shared output transaction"
        );
        resources
            .cleanup()
            .expect("I/O owner")
            .shutdown()
            .await
            .expect("cleanup");
    }
    let text = "MATCH (n:Item) RETURN n.name AS name";
    {
        let mut query = ContinuousQueryTransformer::new_with_options(
            definition(text),
            adapter.clone(),
            options(),
        )
        .await
        .expect("construct");
        query.start().await.expect("start");
        assert_eq!(
            query.transform(input()).await.expect("query")[0]
                .envelope
                .system()
                .sequence(),
            1
        );
        query.stop().await.expect("quiesce");
    }
    {
        let mut query = ContinuousQueryTransformer::new_with_options(
            definition(text),
            adapter.clone(),
            options(),
        )
        .await
        .expect("reopen");
        query.start().await.expect("recover");
        assert_eq!(query.results().snapshot().expect("snapshot").rows.len(), 1);
        assert!(query
            .transform(input())
            .await
            .expect("source checkpoint dedup")
            .is_empty());
        query.stop().await.expect("stop");
    }
    let legacy = plugin
        .create_indexes("query")
        .await
        .expect("ordinary legacy namespace");
    assert!(legacy
        .outbox_writer
        .expect("outbox")
        .read_from("query", 0)
        .await
        .expect("legacy read")
        .is_empty());
    adapter.shutdown().await.expect("constructor work cleanup");
}

#[tokio::test]
async fn compatibility_failure_before_any_output_leaves_a_durable_pre_evaluation_fence() {
    let temp = tempfile::tempdir().expect("temp");
    let adapter = LegacyIndexProviderAdapter::new(Arc::new(
        drasi_index_rocksdb::RocksDbIndexProvider::new(temp.path(), false, false),
    ));
    let text = "MATCH (n:Item) RETURN size(42) AS invalid";
    {
        let mut query = ContinuousQueryTransformer::new_with_options(
            definition(text),
            adapter.clone(),
            options(),
        )
        .await
        .expect("construct");
        query.start().await.expect("start");
        assert!(query.transform(input()).await.is_err());
        query.stop().await.expect("cleanup");
    }
    let mut query =
        ContinuousQueryTransformer::new_with_options(definition(text), adapter.clone(), options())
            .await
            .expect("reopen");
    assert!(matches!(
        query
            .start()
            .await
            .expect_err("uncertain legacy core cannot silently resume")
            .downcast_ref::<QueryRecoveryError>(),
        Some(QueryRecoveryError::PendingPublication)
    ));
    query.stop().await.expect("cleanup");
    adapter.shutdown().await.expect("provider cleanup");
}

#[tokio::test]
async fn scoped_legacy_backend_keeps_same_named_queries_in_different_instances_isolated() {
    let temp = tempfile::tempdir().expect("temp");
    let plugin = Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
        temp.path(),
        false,
        false,
    ));
    let one = LegacyIndexProviderAdapter::scoped(plugin.clone(), "one::computation::graph")
        .expect("first scope");
    let two = LegacyIndexProviderAdapter::scoped(plugin, "two::computation::graph")
        .expect("second scope");
    let text = "MATCH (n:Item) RETURN n.name AS name";
    {
        let mut first =
            ContinuousQueryTransformer::new_with_options(definition(text), one.clone(), options())
                .await
                .expect("first query");
        let mut second =
            ContinuousQueryTransformer::new_with_options(definition(text), two.clone(), options())
                .await
                .expect("second query");
        first.start().await.expect("first start");
        second.start().await.expect("second start");
        first.transform(input()).await.expect("first input");
        assert_eq!(
            first.results().snapshot().expect("first rows").rows.len(),
            1
        );
        assert!(second
            .results()
            .snapshot()
            .expect("isolated rows")
            .rows
            .is_empty());
        assert_eq!(
            second
                .transform(input())
                .await
                .expect("independent checkpoint")
                .len(),
            1
        );
        first.stop().await.expect("first stop");
        second.stop().await.expect("second stop");
    }
    one.shutdown().await.expect("first shutdown");
    two.shutdown().await.expect("second shutdown");
}
