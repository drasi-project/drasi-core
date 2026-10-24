// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use drasi_lib::computation::v1::*;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::NativeApplicationReaction;
use shared_tests::{mock_source::MockSource, recovery_test_helpers::insert_person};
use tokio::sync::{Mutex, Notify};

fn query_output() -> Endpoint {
    Endpoint::new(
        ComponentId::try_new("q1").expect("query ID"),
        PortId::try_new("out").expect("output port"),
    )
}

async fn instance(
    reaction: NativeApplicationReaction,
) -> (DrasiLib, shared_tests::mock_source::MockSourceHandle) {
    tokio::time::timeout(Duration::from_secs(10), build_instance(reaction))
        .await
        .expect("native reaction registration and startup must complete")
}

async fn build_instance(
    reaction: NativeApplicationReaction,
) -> (DrasiLib, shared_tests::mock_source::MockSourceHandle) {
    let (source, source_handle) = MockSource::new("source").expect("mock source");
    let input = reaction.input();
    let core = DrasiLib::builder()
        .with_id("native-application-query")
        .with_source(source)
        .build()
        .await
        .expect("library");
    core.computation_component("source")
        .expect("source handle")
        .wait_created()
        .await
        .expect("source creation");
    let pipeline = core
        .computation_pipeline()
        .expect("pipeline builder")
        .source(
            core.borrow_computation_source("source")
                .await
                .expect("source host"),
            SourceSubscriptionOptions::default(),
        )
        .expect("source binding")
        .query(
            Query::cypher("q1")
                .query("MATCH (p:Person) RETURN p.name AS name")
                .from_source("source")
                .enable_bootstrap(false)
                .build(),
        )
        .build()
        .expect("native query batch");
    assert!(
        core.add_components(pipeline)
            .await
            .expect("query addition")
            .committed
    );
    let component = core
        .add_computation_component(ComponentAddition::new(ConstructedComponent::sink(
            Box::new(reaction),
        )))
        .await
        .expect("sink addition");
    let connected = core
        .computation_control()
        .expect("graph control")
        .connect(
            EdgeDefinition::new(query_output(), input),
            Box::new(BoundedPipeConfig { capacity: 1 }),
            RelationshipPolicy::default(),
        )
        .await
        .expect("declared port connection");
    assert_eq!(
        connected.summary,
        OperationSummary::Completed,
        "{connected:?}"
    );
    component.wait_created().await.expect("sink creation");
    core.start().await.expect("start instance");
    component.wait_started().await.expect("sink readiness");
    (core, source_handle)
}

fn assert_result(envelope: &ChangeEnvelope, name: &str) {
    assert_eq!(
        QueryChangeCodec::metadata(envelope)
            .expect("query metadata")
            .query_id,
        "q1"
    );
    assert_eq!(envelope.changes().operations().len(), 1);
    let ChangeOperation::Added { after, .. } = &envelope.changes().operations()[0] else {
        panic!("expected an added query row");
    };
    let row = QueryChangeCodec::decode_row(after).expect("query row");
    assert_eq!(
        QueryChangeCodec::row_values_to_json(&row.values)["name"],
        name
    );
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_delivers_envelopes_without_legacy_reaction_adapter() {
    let (reaction, mut receiver) = NativeApplicationReaction::channel(
        "native-app",
        QueryChangeCodec::schema().descriptor().clone(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();
    let (core, source) = instance(reaction).await;
    let component = core.computation_component("native-app").unwrap();
    for (id, name) in [("p1", "Alice"), ("p2", "Bob"), ("p3", "Carol")] {
        insert_person(&source, id, name, 30).await.unwrap();
        let input = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(input.port.as_str(), "in");
        assert_result(&input.envelope, name);
        tokio::time::timeout(Duration::from_secs(5), component.stop())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), component.start())
            .await
            .unwrap()
            .unwrap();
    }
    core.shutdown().await.unwrap();
    receiver.close();
    assert!(receiver.recv().await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_awaits_application_callback_completion() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let results = Arc::new(Mutex::new(Vec::new()));
    let callback_entered = entered.clone();
    let callback_release = release.clone();
    let callback_results = results.clone();
    let reaction = NativeApplicationReaction::callback(
        "native-app",
        QueryChangeCodec::schema().descriptor().clone(),
        move |input| {
            let entered = callback_entered.clone();
            let release = callback_release.clone();
            let results = callback_results.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                results.lock().await.push(input.envelope);
                Ok(())
            }
        },
    )
    .unwrap();
    let (core, source) = instance(reaction).await;
    insert_person(&source, "p1", "Alice", 30).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(results.lock().await.is_empty());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while results.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    core.shutdown().await.unwrap();
    let received = results.lock().await;
    assert_eq!(received.len(), 1);
    assert_result(&received[0], "Alice");
}
