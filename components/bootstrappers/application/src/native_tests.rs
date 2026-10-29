// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::ApplicationBootstrapProvider;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::{
    ComponentId, ComputationBootstrapProvider, ComputationComponent, ComputationQueryLanguage,
    ContinuousQueryDefinition, ContinuousQueryTransformer, GraphChangeCodec, StreamId,
};
use std::{future::Future, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

fn both_runtimes<F, Fut>(scenario: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    for current_thread in [true, false] {
        let mut builder = if current_thread {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        builder.enable_all().build().unwrap().block_on(scenario());
    }
}

fn metadata(id: &str, label: &str) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("app", id),
        labels: Arc::from(vec![Arc::from(label)]),
        effective_from: 42,
    }
}

fn node(id: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, label),
            properties: Default::default(),
        },
    }
}

fn relation(id: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata(id, label),
            properties: Default::default(),
            in_node: ElementReference::new("app", "one"),
            out_node: ElementReference::new("app", "two"),
        },
    }
}

async fn rows(provider: &ApplicationBootstrapProvider) -> Vec<SourceChange> {
    let mut snapshot = provider.snapshot().await.unwrap();
    assert!(
        snapshot.watermarks.is_empty(),
        "in-memory snapshots cannot invent live-source progress"
    );
    let mut result = Vec::new();
    while let Some(envelope) = snapshot.changes.next().await {
        let envelope = envelope.unwrap();
        assert_eq!(
            envelope.system().sequence(),
            u64::try_from(result.len()).unwrap() + 1
        );
        assert!(GraphChangeCodec::source_metadata(&envelope)
            .unwrap()
            .is_none());
        result.extend(GraphChangeCodec::decode_changes(&envelope).unwrap());
    }
    result
}

#[test]
fn native_bootstrap_streams_stored_elements_instead_of_only_counting_them() {
    both_runtimes(|| async {
        let provider = ApplicationBootstrapProvider::builder().build();
        let one = node("one", "Person");
        let edge = relation("edge", "KNOWS");
        provider.store_insert_event(one.clone()).await;
        provider.store_insert_event(edge.clone()).await;
        assert_eq!(
            provider.get_stored_events().await,
            [one.clone(), edge.clone()]
        );
        assert_eq!(rows(&provider).await, [one, edge]);
        provider.stop().await.unwrap();
    });
}

#[test]
fn native_snapshot_uses_shared_storage_and_independent_node_relation_filters() {
    both_runtimes(|| async {
        let one = node("one", "Person");
        let edge = relation("knows", "KNOWS");
        let data = Arc::new(RwLock::new(vec![
            one.clone(),
            node("other", "Device"),
            relation("likes", "LIKES"),
            edge.clone(),
        ]));
        let provider = ApplicationBootstrapProvider::with_shared_data(data.clone())
            .with_stream(StreamId::try_new("selected/bootstrap").unwrap())
            .with_node_labels(vec!["Person".into()])
            .with_relation_labels(vec!["KNOWS".into()]);
        assert_eq!(rows(&provider).await, [one.clone(), edge.clone()]);
        let mut snapshot = provider.snapshot().await.unwrap();
        assert_eq!(
            snapshot
                .changes
                .next()
                .await
                .unwrap()
                .unwrap()
                .system()
                .stream()
                .as_str(),
            "selected/bootstrap"
        );
        let all_nodes = ApplicationBootstrapProvider::with_shared_data(data.clone())
            .with_relation_labels(vec!["KNOWS".into()]);
        assert_eq!(rows(&all_nodes).await, [one, node("other", "Device"), edge]);
        let none = ApplicationBootstrapProvider::with_shared_data(data)
            .with_node_labels(vec!["Absent".into()])
            .with_relation_labels(vec!["Absent".into()]);
        assert!(rows(&none).await.is_empty());
    });
}

#[test]
fn snapshots_are_stable_and_never_hold_a_writer_lock_while_streaming() {
    both_runtimes(|| async {
        let first = node("first", "Person");
        let data = Arc::new(RwLock::new(vec![first.clone()]));
        let provider = ApplicationBootstrapProvider::builder()
            .with_shared_data(data.clone())
            .build();
        let mut snapshot = provider.snapshot().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), provider.clear_stored_events())
            .await
            .unwrap();
        provider.store_insert_event(node("later", "Person")).await;
        let envelope = snapshot.changes.next().await.unwrap().unwrap();
        assert_eq!(
            GraphChangeCodec::decode_changes(&envelope).unwrap(),
            [first]
        );
        assert!(snapshot.changes.next().await.is_none());
        assert_eq!(rows(&provider).await, [node("later", "Person")]);
        drop(provider.snapshot().await.unwrap());
        provider.stop().await.unwrap();
        data.write().await.clear();
        assert!(rows(&provider).await.is_empty());
    });
}

#[test]
fn existing_insert_storage_rule_does_not_accept_deletes_or_updates() {
    both_runtimes(|| async {
        let provider = ApplicationBootstrapProvider::default();
        let SourceChange::Insert { element } = node("one", "Person") else {
            unreachable!()
        };
        provider
            .store_insert_event(SourceChange::update(element))
            .await;
        provider
            .store_insert_event(SourceChange::Delete {
                metadata: metadata("one", "Person"),
            })
            .await;
        assert!(provider.get_stored_events().await.is_empty());
        assert!(rows(&provider).await.is_empty());
    });
}

#[test]
fn snapshot_errors_are_not_reported_as_empty_success() {
    both_runtimes(|| async {
        let mut bad = metadata("bad", "Person");
        bad.effective_from = u64::MAX;
        let data = Arc::new(RwLock::new(vec![SourceChange::Delete { metadata: bad }]));
        let provider = ApplicationBootstrapProvider::with_shared_data(data.clone());
        let mut snapshot = provider.snapshot().await.unwrap();
        assert!(snapshot.changes.next().await.unwrap().is_err());
        assert_eq!(data.read().await.len(), 1);
        assert!(snapshot.watermarks.is_empty());
        provider.stop().await.unwrap();
    });
}

#[test]
fn a_native_query_receives_the_application_snapshot() {
    both_runtimes(|| async {
        let data = Arc::new(RwLock::new(vec![
            node("one", "Person"),
            node("two", "Person"),
        ]));
        let bootstrap = Arc::new(ApplicationBootstrapProvider::with_shared_data(data));
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "application-bootstrap-test".into(),
                id: ComponentId::try_new("people").unwrap(),
                query: "MATCH (n:Person) RETURN n AS person".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("people/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(16).unwrap(),
            },
            Arc::new(InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_bootstrap(bootstrap);
        query.start().await.unwrap();
        assert_eq!(query.results().snapshot().unwrap().rows.len(), 2);
        query.stop().await.unwrap();
    });
}
